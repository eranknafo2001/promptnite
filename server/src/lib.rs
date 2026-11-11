//! Promptnite SpacetimeDB module.
//!
//! Owns the rooms, the round loop and the authoritative match simulation.
//! See docs/GAME_DESIGN.md for the big picture.
//!
//! Players meet in rooms: everyone who enters the same room id plays together,
//! and whoever created the room owns it and picks its settings.
//!
//! Round loop (each room runs its own):
//!
//!   Prompting --all scripts ready--> Countdown --3s--> Fighting --match length--> Results --8s--> Prompting
//!
//! The LLM call does *not* happen here: a module's HTTP requests are cut off
//! after 500 ms. Instead the module pokes an agent worker (`worker/`, a
//! Cloudflare Worker) and the worker calls back:
//!
//!   submit_prompt ──► agent_call (scheduled) ──► notify_agent ── POST /job ──► worker
//!   deliver_script / fail_job  ◄───────────── HTTP reducer call ◄───────────── worker
//!
//! The worker is a dumb pipe to the model. Everything that decides whether a
//! script is acceptable lives here: `deliver_script` validates the Lua, sends
//! an invalid script back to the worker with the error for another try, and
//! only marks the player ready once a script passed.

#[cfg(target_arch = "wasm32")]
mod entropy;

use std::time::Duration;

use promptnite_sim::{self as sim, rules};
use serde_json::json;
use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp,
    http::{Request, Timeout},
    procedure, reducer, table,
};

const COUNTDOWN: Duration = Duration::from_secs(3);
const RESULTS: Duration = Duration::from_secs(8);
const MAX_PROMPT_CHARS: usize = 2_000;
const MAX_NAME_CHARS: usize = 24;
const MAX_ROOM_CHARS: usize = 24;

/// `Player::room_id` of somebody who has not picked a room yet. Room ids
/// start at 1.
const NO_ROOM: u32 = 0;

/// How many scripts the agent may write for one prompt before we give up.
const MAX_ATTEMPTS: u32 = 2;
/// How long one attempt may take before the job is failed. A Cloudflare
/// Worker gets about 30 s of background time, so nothing arrives after this.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);
/// The worker only has to acknowledge the job, not finish it.
const NOTIFY_TIMEOUT: Duration = Duration::from_millis(450);
const MAX_SCRIPT_CHARS: usize = 20_000;
const MAX_ERROR_CHARS: usize = 300;

/// Bot id of the built-in sparring partner, in rooms that have it switched on.
/// Player ids start at 1, so 0 is never a real player.
const HOUSE_BOT_ID: u32 = 0;

// Environment --------------------------------------------------------------

/// Set at publish time: `AGENT_URL=... AGENT_SECRET=... spacetime publish ...`.
#[spacetimedb::env]
pub struct Env {
    /// Base URL of the agent worker, e.g. `https://promptnite-agent.you.workers.dev`.
    /// When unset there is no agent and every prompt gets the example bot.
    pub AGENT_URL: Option<String>,
    /// Shared secret: sent to the worker with each job, and required back
    /// from it in `deliver_script` / `fail_job`.
    pub AGENT_SECRET: Option<String>,
}

// Tables -------------------------------------------------------------------

#[table(accessor = player, public)]
pub struct Player {
    #[primary_key]
    identity: Identity,
    /// Short id used everywhere else (frames, submissions, Lua `id` fields).
    #[unique]
    #[auto_inc]
    id: u32,
    name: String,
    /// The room they are in (or were last in, while offline). `NO_ROOM`
    /// before they picked their first one.
    #[index(btree)]
    room_id: u32,
    /// In the app right now, with a room picked. A player picks a room every
    /// time they open the app, so this is set by `join`, not by connecting.
    online: bool,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Players write prompts; agents turn them into Lua.
    Prompting,
    /// Everyone is ready; the match is already simulated and about to play.
    Countdown,
    /// Clients are replaying the match frames.
    Fighting,
    /// Winner is shown.
    Results,
}

/// One game. Everyone who joins with the same code is in the same room.
#[table(accessor = room, public)]
pub struct Room {
    #[primary_key]
    #[auto_inc]
    id: u32,
    /// What players type to get in. Trimmed and lowercased.
    #[unique]
    code: String,
    /// Player id of whoever created the room. Only they can change the
    /// settings below.
    owner: u32,

    // Settings.
    /// Time limit of a match, in seconds.
    match_secs: u32,
    /// Whether the house bot fights in every match.
    house_bot: bool,

    round: u32,
    phase: Phase,
    /// When the current phase ends. `None` while waiting for players.
    phase_ends_at: Option<Timestamp>,
    /// Last tick of the current match (frames run `0..=match_ticks`).
    match_ticks: u32,
    /// Only meaningful in `Results`. `None` there means a draw.
    winner: Option<u32>,

    // Rule constants the client needs to draw the arena. Copied from
    // `promptnite_sim::rules` so the client doesn't have to link the sim.
    ticks_per_sec: u32,
    arena_half_size: f32,
    bot_radius: f32,
    attack_range: f32,
    max_hp: i32,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionStatus {
    /// Prompt received, waiting for the agent to write the script.
    Generating,
    Ready,
    /// The agent failed or wrote an invalid script; the player may try again.
    Failed,
}

/// Public "is this player ready?" status. The prompt and script stay private.
#[table(accessor = submission, public)]
pub struct Submission {
    #[primary_key]
    player_id: u32,
    #[index(btree)]
    room_id: u32,
    round: u32,
    status: SubmissionStatus,
    /// Why it failed, empty otherwise.
    error: String,
}

/// A player's prompt and the Lua their agent wrote. Private, and on purpose
/// not exposed through any view: players never see the script, not even their
/// own. Clients only get the public `submission` status.
#[table(accessor = bot_script)]
pub struct BotScript {
    #[primary_key]
    player_id: u32,
    round: u32,
    prompt: String,
    /// Empty until the agent delivers.
    script: String,
}

/// A prompt the agent worker is turning into a script. Private.
#[table(accessor = agent_job)]
pub struct AgentJob {
    /// What the worker calls back with. A new prompt gets a new id, so an
    /// answer to an old prompt can't land on the new one.
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[unique]
    player_id: u32,
    round: u32,
    prompt: String,
    /// 1 for the first script, 2 after one was rejected, ...
    attempt: u32,
}

/// Outbox: one row per HTTP request to the agent worker. Reducers can't do
/// HTTP, so they insert here and the `notify_agent` procedure sends it.
#[table(accessor = agent_call, scheduled(notify_agent))]
pub struct AgentCall {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: ScheduleAt,
    job_id: u64,
    player_id: u32,
    round: u32,
    prompt: String,
    /// Both empty on the first attempt. On a retry: the script the module
    /// rejected and why, so the model can fix it.
    rejected_script: String,
    rejected_error: String,
}

/// Fails a job whose worker never answered.
#[table(accessor = job_timeout, scheduled(expire_job))]
pub struct JobTimeout {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: ScheduleAt,
    job_id: u64,
    attempt: u32,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotAction {
    Idle,
    Walk,
    /// Swung and hit.
    Attack,
    /// Swung at nothing.
    Miss,
    Dead,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct BotFrame {
    player_id: u32,
    x: f32,
    y: f32,
    hp: i32,
    action: BotAction,
}

/// A room's current match, one row per tick. Clients replay these.
#[table(accessor = frame, public)]
pub struct Frame {
    /// `frame_key(room_id, tick)`, so a client can look a tick up directly.
    #[primary_key]
    key: u64,
    #[index(btree)]
    room_id: u32,
    tick: u32,
    bots: Vec<BotFrame>,
}

/// Room in the high half, tick in the low half. The client computes the same
/// key (`client/src/arena.rs`).
fn frame_key(room_id: u32, tick: u32) -> u64 {
    (room_id as u64) << 32 | tick as u64
}

/// First script error of each bot in a room's current match.
#[table(accessor = match_fault, public)]
pub struct MatchFault {
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[index(btree)]
    room_id: u32,
    player_id: u32,
    tick: u32,
    message: String,
}

#[table(accessor = phase_timer, scheduled(advance_phase))]
pub struct PhaseTimer {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: ScheduleAt,
    room_id: u32,
    round: u32,
    to: Phase,
    /// Carried along so the winner isn't public before the replay ends.
    winner: Option<u32>,
}

// Lifecycle ----------------------------------------------------------------

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    if let Some(player) = ctx.db.player().identity().find(ctx.sender()) {
        let room_id = player.room_id;
        ctx.db.player().identity().update(Player {
            online: false,
            ..player
        });
        // Offline players neither block the round nor fight in it. Their entry
        // is kept, so a page reload during prompting doesn't lose the script
        // if they come back to the same room.
        try_start_match(ctx, room_id);
    }
}

// Player reducers ----------------------------------------------------------

/// Enter a room under a name. The first one in creates the room and owns it.
///
/// Clients call this every time they open the app, and again after a
/// reconnect.
#[reducer]
pub fn join(ctx: &ReducerContext, name: String, room: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("Name must be 1-{MAX_NAME_CHARS} characters"));
    }
    let code = room.trim().to_lowercase();
    if code.is_empty() || code.chars().count() > MAX_ROOM_CHARS {
        return Err(format!("Room id must be 1-{MAX_ROOM_CHARS} characters"));
    }

    let player = ctx
        .db
        .player()
        .identity()
        .find(ctx.sender())
        .unwrap_or_else(|| {
            ctx.db.player().insert(Player {
                identity: ctx.sender(),
                id: 0, // auto_inc
                name: name.clone(),
                room_id: NO_ROOM,
                online: false,
            })
        });

    let room = match ctx.db.room().code().find(&code) {
        Some(room) => {
            let deserted = !ctx
                .db
                .player()
                .room_id()
                .filter(room.id)
                .any(|other| other.online && other.id != player.id);
            if deserted && room.owner != player.id {
                // Everybody left: to the newcomer this is a new room.
                ctx.db.room().id().update(Room {
                    owner: player.id,
                    match_secs: rules::DEFAULT_MATCH_SECS,
                    house_bot: true,
                    ..room
                })
            } else {
                room
            }
        }
        None => ctx.db.room().insert(Room {
            id: 0, // auto_inc
            code,
            owner: player.id,
            match_secs: rules::DEFAULT_MATCH_SECS,
            house_bot: true,
            round: 1,
            phase: Phase::Prompting,
            phase_ends_at: None,
            match_ticks: 0,
            winner: None,
            ticks_per_sec: rules::TICKS_PER_SEC,
            arena_half_size: rules::ARENA_HALF_SIZE,
            bot_radius: rules::BOT_RADIUS,
            attack_range: rules::ATTACK_RANGE,
            max_hp: rules::MAX_HP,
        }),
    };

    let left = player.room_id;
    if left != room.id {
        // A script belongs to a round of the room it was written in.
        clear_entry(ctx, player.id);
    }
    ctx.db.player().identity().update(Player {
        name,
        room_id: room.id,
        online: true,
        ..player
    });
    if left != room.id {
        // The room they left may have been waiting only for them.
        try_start_match(ctx, left);
    }
    // Coming back online with a ready script may complete this room.
    try_start_match(ctx, room.id);
    Ok(())
}

/// Change your room's settings. Owner only. Takes effect from the next match.
#[reducer]
pub fn set_room_settings(
    ctx: &ReducerContext,
    match_secs: u32,
    house_bot: bool,
) -> Result<(), String> {
    let (player, room) = player_in_room(ctx)?;
    if room.owner != player.id {
        return Err("Only the room's owner can change its settings".into());
    }
    if !(rules::MIN_MATCH_SECS..=rules::MAX_MATCH_SECS).contains(&match_secs) {
        return Err(format!(
            "Match length must be {}-{} seconds",
            rules::MIN_MATCH_SECS,
            rules::MAX_MATCH_SECS
        ));
    }
    let room_id = room.id;
    ctx.db.room().id().update(Room {
        match_secs,
        house_bot,
        ..room
    });
    // Switching the house bot on may be all a lone, ready player was missing.
    try_start_match(ctx, room_id);
    Ok(())
}

/// Hand this round's prompt to your agent.
#[reducer]
pub fn submit_prompt(ctx: &ReducerContext, prompt: String) -> Result<(), String> {
    let (player, room) = player_in_prompting(ctx)?;
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(format!("Prompt must be 1-{MAX_PROMPT_CHARS} characters"));
    }

    clear_entry(ctx, player.id);
    ctx.db.submission().insert(Submission {
        player_id: player.id,
        room_id: room.id,
        round: room.round,
        status: SubmissionStatus::Generating,
        error: String::new(),
    });
    ctx.db.bot_script().insert(BotScript {
        player_id: player.id,
        round: room.round,
        prompt: prompt.clone(),
        script: String::new(),
    });

    if agent_url(ctx).is_none() {
        // No agent configured: everyone gets the example bot, so the game
        // loop can be tried without a worker or an API key.
        return accept_script(ctx, player.id, sim::EXAMPLE_SCRIPT.to_string());
    }
    let job = ctx.db.agent_job().insert(AgentJob {
        id: 0, // auto_inc
        player_id: player.id,
        round: room.round,
        prompt,
        attempt: 1,
    });
    call_agent(ctx, &job, String::new(), String::new());
    Ok(())
}

/// Skip the agent and submit Lua directly. Handy for development and for
/// testing the game without an LLM key.
#[reducer]
pub fn submit_script(ctx: &ReducerContext, script: String) -> Result<(), String> {
    let (player, room) = player_in_prompting(ctx)?;
    sim::validate_script(&script)?;

    clear_entry(ctx, player.id);
    ctx.db.submission().insert(Submission {
        player_id: player.id,
        room_id: room.id,
        round: room.round,
        status: SubmissionStatus::Ready,
        error: String::new(),
    });
    ctx.db.bot_script().insert(BotScript {
        player_id: player.id,
        round: room.round,
        prompt: "(hand-written script)".into(),
        script,
    });
    try_start_match(ctx, room.id);
    Ok(())
}

// Agent worker ------------------------------------------------------------

/// Sends one job to the agent worker. Runs from the `agent_call` schedule.
///
/// The worker answers "accepted" straight away and calls `deliver_script` or
/// `fail_job` when the model is done, so this fits the 500 ms HTTP limit.
#[procedure]
pub fn notify_agent(ctx: &mut ProcedureContext, call: AgentCall) {
    // Scheduled procedures are callable by anyone unless we check.
    if ctx.sender() != ctx.database_identity() {
        log::warn!("notify_agent called by {}", ctx.sender());
        return;
    }
    let (Some(url), Some(secret)) = (ctx.env.AGENT_URL(), ctx.env.AGENT_SECRET()) else {
        return;
    };

    let mut body = json!({
        "job_id": call.job_id,
        "prompt": call.prompt,
    });
    if !call.rejected_script.is_empty() {
        body["rejected"] = json!({
            "script": call.rejected_script,
            "error": call.rejected_error,
        });
    }
    let request = Request::builder()
        .uri(format!("{}/job", url.trim_end_matches('/')))
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {secret}"))
        .extension(Timeout::from(TimeDuration::from(NOTIFY_TIMEOUT)))
        .body(body.to_string());

    let problem = match request {
        Err(err) => Some(format!("bad AGENT_URL: {err}")),
        Ok(request) => match ctx.http.send(request) {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("it answered {}", response.status())),
            Err(err) => Some(err.to_string()),
        },
    };
    if let Some(problem) = problem {
        log::error!("job {}: agent worker unreachable: {problem}", call.job_id);
        let job_id = call.job_id;
        ctx.with_tx(|tx| {
            if let Some(job) = tx.db.agent_job().id().find(job_id) {
                fail(tx, job, "Could not reach the agent. Try again.".into());
            }
        });
    }
}

/// The agent wrote a script. Called by the worker over HTTP.
///
/// This is the only place a generated script is judged. A valid one makes the
/// player ready. An invalid one goes back to the worker together with the
/// error; the player stays "generating" and nothing else changes until a
/// script passes or the attempts run out.
#[reducer]
pub fn deliver_script(
    ctx: &ReducerContext,
    secret: String,
    job_id: u64,
    script: String,
) -> Result<(), String> {
    let job = job_for_worker(ctx, &secret, job_id)?;

    let verdict = if script.chars().count() > MAX_SCRIPT_CHARS {
        Err(format!(
            "the script is longer than {MAX_SCRIPT_CHARS} characters"
        ))
    } else {
        sim::validate_script(&script)
    };
    match verdict {
        Ok(()) => {
            ctx.db.agent_job().id().delete(job.id);
            accept_script(ctx, job.player_id, script)
        }
        Err(error) if job.attempt < MAX_ATTEMPTS => {
            log::info!("job {}: script rejected, asking again: {error}", job.id);
            let job = ctx.db.agent_job().id().update(AgentJob {
                attempt: job.attempt + 1,
                ..job
            });
            call_agent(ctx, &job, script, error);
            Ok(())
        }
        Err(error) => {
            fail(
                ctx,
                job,
                format!("The agent wrote an invalid script: {error}"),
            );
            Ok(())
        }
    }
}

/// The agent could not produce a script (API error, refusal, ...).
#[reducer]
pub fn fail_job(
    ctx: &ReducerContext,
    secret: String,
    job_id: u64,
    error: String,
) -> Result<(), String> {
    let job = job_for_worker(ctx, &secret, job_id)?;
    let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
    fail(ctx, job, error);
    Ok(())
}

#[reducer]
pub fn expire_job(ctx: &ReducerContext, timeout: JobTimeout) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("expire_job is internal".into());
    }
    // A job that was answered is gone; one that was retried has moved on to
    // the next attempt and has its own timeout.
    if let Some(job) = ctx.db.agent_job().id().find(timeout.job_id)
        && job.attempt == timeout.attempt
    {
        fail(ctx, job, "The agent took too long. Try again.".into());
    }
    Ok(())
}

fn agent_url(ctx: &ReducerContext) -> Option<String> {
    ctx.env.AGENT_URL().filter(|url| !url.is_empty())
}

/// Queue the HTTP request for this job's current attempt, plus its timeout.
fn call_agent(
    ctx: &ReducerContext,
    job: &AgentJob,
    rejected_script: String,
    rejected_error: String,
) {
    ctx.db.agent_call().insert(AgentCall {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp),
        job_id: job.id,
        player_id: job.player_id,
        round: job.round,
        prompt: job.prompt.clone(),
        rejected_script,
        rejected_error,
    });
    ctx.db.job_timeout().insert(JobTimeout {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp + ATTEMPT_TIMEOUT),
        job_id: job.id,
        attempt: job.attempt,
    });
}

/// Check the shared secret and find the job the worker is reporting on.
fn job_for_worker(ctx: &ReducerContext, secret: &str, job_id: u64) -> Result<AgentJob, String> {
    match ctx.env.AGENT_SECRET() {
        Some(expected) if !expected.is_empty() && expected == secret => {}
        _ => return Err("Wrong agent secret".into()),
    }
    ctx.db.agent_job().id().find(job_id).ok_or_else(|| {
        "No such job (the player resubmitted, it timed out or the round moved on)".into()
    })
}

/// A validated script: store it, mark the player ready, maybe start the match.
fn accept_script(ctx: &ReducerContext, player_id: u32, script: String) -> Result<(), String> {
    let entry = ctx
        .db
        .bot_script()
        .player_id()
        .find(player_id)
        .ok_or("The player's entry is gone")?;
    ctx.db
        .bot_script()
        .player_id()
        .update(BotScript { script, ..entry });
    if let Some(room_id) = set_submission(ctx, player_id, SubmissionStatus::Ready, String::new()) {
        try_start_match(ctx, room_id);
    }
    Ok(())
}

fn fail(ctx: &ReducerContext, job: AgentJob, error: String) {
    ctx.db.agent_job().id().delete(job.id);
    set_submission(ctx, job.player_id, SubmissionStatus::Failed, error);
}

// Round loop ---------------------------------------------------------------

/// Start the room's match once every online player in it has a valid script.
fn try_start_match(ctx: &ReducerContext, room_id: u32) {
    let Some(room) = ctx.db.room().id().find(room_id) else {
        return;
    };
    if room.phase != Phase::Prompting {
        return;
    }

    let mut bots = Vec::new();
    for player in ctx.db.player().room_id().filter(room_id) {
        if !player.online {
            continue;
        }
        let ready = ctx
            .db
            .submission()
            .player_id()
            .find(player.id)
            .is_some_and(|s| s.round == room.round && s.status == SubmissionStatus::Ready);
        let Some(script) = ctx
            .db
            .bot_script()
            .player_id()
            .find(player.id)
            .filter(|_| ready)
        else {
            return; // somebody is still writing or generating
        };
        bots.push(sim::BotSpec {
            id: player.id,
            script: script.script,
        });
    }
    // Stable spawn order regardless of table iteration order.
    bots.sort_by_key(|bot| bot.id);
    if room.house_bot && !bots.is_empty() {
        bots.push(sim::BotSpec {
            id: HOUSE_BOT_ID,
            script: sim::EXAMPLE_SCRIPT.to_string(),
        });
    }
    if bots.len() < 2 {
        return; // nobody to fight yet
    }

    // Nobody can influence a match after this point, so simulate all of it now
    // and let clients replay the frames.
    let seed = ctx.timestamp.to_micros_since_unix_epoch() as u64;
    let outcome = sim::run_match(&bots, seed, room.match_secs * rules::TICKS_PER_SEC);

    ctx.db.frame().room_id().delete(room_id);
    ctx.db.match_fault().room_id().delete(room_id);
    for frame in &outcome.frames {
        ctx.db.frame().insert(Frame {
            key: frame_key(room_id, frame.tick),
            room_id,
            tick: frame.tick,
            bots: frame
                .bots
                .iter()
                .map(|bot| BotFrame {
                    player_id: bot.id,
                    x: bot.x,
                    y: bot.y,
                    hp: bot.hp,
                    action: match bot.action {
                        sim::ActionKind::Idle => BotAction::Idle,
                        sim::ActionKind::Walk => BotAction::Walk,
                        sim::ActionKind::Attack => BotAction::Attack,
                        sim::ActionKind::Miss => BotAction::Miss,
                        sim::ActionKind::Dead => BotAction::Dead,
                    },
                })
                .collect(),
        });
    }
    for fault in outcome.faults {
        ctx.db.match_fault().insert(MatchFault {
            id: 0, // auto_inc
            room_id,
            player_id: fault.bot_id,
            tick: fault.tick,
            message: fault.message,
        });
    }

    let match_ticks = outcome.frames.last().map_or(0, |frame| frame.tick);
    log::info!(
        "room {:?} round {}: {} bots, {} ticks, winner {:?}",
        room.code,
        room.round,
        bots.len(),
        match_ticks,
        outcome.winner
    );

    let round = room.round;
    let ends_at = ctx.timestamp + COUNTDOWN;
    ctx.db.room().id().update(Room {
        phase: Phase::Countdown,
        phase_ends_at: Some(ends_at),
        match_ticks,
        winner: None,
        ..room
    });
    schedule(
        ctx,
        ends_at,
        room_id,
        round,
        Phase::Fighting,
        outcome.winner,
    );
}

#[reducer]
pub fn advance_phase(ctx: &ReducerContext, timer: PhaseTimer) -> Result<(), String> {
    // Scheduled reducers are callable by anyone unless we check.
    if ctx.sender() != ctx.database_identity() {
        return Err("advance_phase is internal".into());
    }
    let Some(room) = ctx
        .db
        .room()
        .id()
        .find(timer.room_id)
        .filter(|room| room.round == timer.round)
    else {
        return Ok(()); // stale timer
    };
    let room_id = room.id;

    match timer.to {
        Phase::Fighting => {
            let length =
                Duration::from_secs_f32(room.match_ticks as f32 / room.ticks_per_sec.max(1) as f32);
            let ends_at = ctx.timestamp + length;
            ctx.db.room().id().update(Room {
                phase: Phase::Fighting,
                phase_ends_at: Some(ends_at),
                ..room
            });
            schedule(
                ctx,
                ends_at,
                room_id,
                timer.round,
                Phase::Results,
                timer.winner,
            );
        }
        Phase::Results => {
            let ends_at = ctx.timestamp + RESULTS;
            ctx.db.room().id().update(Room {
                phase: Phase::Results,
                phase_ends_at: Some(ends_at),
                winner: timer.winner,
                ..room
            });
            schedule(ctx, ends_at, room_id, timer.round, Phase::Prompting, None);
        }
        Phase::Prompting => {
            let entries: Vec<u32> = ctx
                .db
                .submission()
                .room_id()
                .filter(room_id)
                .map(|row| row.player_id)
                .collect();
            for player_id in entries {
                clear_entry(ctx, player_id);
            }
            ctx.db.room().id().update(Room {
                round: room.round + 1,
                phase: Phase::Prompting,
                phase_ends_at: None,
                winner: None,
                ..room
            });
        }
        Phase::Countdown => {}
    }
    Ok(())
}

// Helpers ------------------------------------------------------------------

/// The caller and the room they are in right now.
fn player_in_room(ctx: &ReducerContext) -> Result<(Player, Room), String> {
    let player = ctx
        .db
        .player()
        .identity()
        .find(ctx.sender())
        .filter(|player| player.online)
        .ok_or("Join a room first")?;
    let room = ctx
        .db
        .room()
        .id()
        .find(player.room_id)
        .ok_or("Join a room first")?;
    Ok((player, room))
}

fn player_in_prompting(ctx: &ReducerContext) -> Result<(Player, Room), String> {
    let (player, room) = player_in_room(ctx)?;
    if room.phase != Phase::Prompting {
        return Err("The round already started; wait for the next one".into());
    }
    Ok((player, room))
}

/// Remove everything a player submitted this round.
fn clear_entry(ctx: &ReducerContext, player_id: u32) {
    ctx.db.submission().player_id().delete(player_id);
    ctx.db.bot_script().player_id().delete(player_id);
    ctx.db.agent_job().player_id().delete(player_id);
}

/// Update a player's submission, if they still have one. Returns its room.
fn set_submission(
    ctx: &ReducerContext,
    player_id: u32,
    status: SubmissionStatus,
    error: String,
) -> Option<u32> {
    let row = ctx.db.submission().player_id().find(player_id)?;
    let room_id = row.room_id;
    ctx.db.submission().player_id().update(Submission {
        status,
        error,
        ..row
    });
    Some(room_id)
}

fn schedule(
    ctx: &ReducerContext,
    at: Timestamp,
    room_id: u32,
    round: u32,
    to: Phase,
    winner: Option<u32>,
) {
    ctx.db.phase_timer().insert(PhaseTimer {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(at),
        room_id,
        round,
        to,
        winner,
    });
}
