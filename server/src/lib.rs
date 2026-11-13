//! Promptnite SpacetimeDB module.
//!
//! Owns the rooms, the round loop and the authoritative match simulation.
//! See docs/GAME_DESIGN.md for the big picture.
//!
//! Players meet in rooms: everyone who enters the same room id plays together,
//! and whoever created the room owns it and picks its settings. A room holds
//! at most four players, one per edge of the arena.
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
//!
//! What happens is logged with `emit`, which also ships it to Axiom when a
//! token is configured; see "Logging" below.
//!
//! What clients may see: every table is private. Clients subscribe to the
//! views at the bottom of this file, which show a caller the room they are in
//! and nothing else, and hold a match back until its fight starts.

#[cfg(target_arch = "wasm32")]
mod entropy;

use std::time::Duration;

use log::Level;
use promptnite_sim::{self as sim, rules};
use serde_json::{Value, json};
use spacetimedb::{
    Identity, ProcedureContext, ReducerContext, ScheduleAt, SpacetimeType, Table, TimeDuration,
    Timestamp, ViewContext,
    http::{Request, Timeout},
    procedure, reducer, table, view,
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

/// Log events wait this long for company, so a burst goes out as one request.
const LOG_SHIP_DELAY: Duration = Duration::from_secs(2);
/// How long to wait before trying again when Axiom could not be reached.
const LOG_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Events per request. A request has to finish within the 500 ms HTTP limit.
const LOG_BATCH: usize = 500;
/// With this many events waiting (Axiom is down), new ones are not kept.
const MAX_PENDING_LOGS: u64 = 5_000;
const AXIOM_TIMEOUT: Duration = Duration::from_millis(450);
const DEFAULT_AXIOM_DOMAIN: &str = "us-east-1.aws.edge.axiom.co";
const DEFAULT_AXIOM_DATASET: &str = "promptnite";
/// A player may send a prompt this often. Each one replaces the last and
/// starts a model call.
const PROMPT_INTERVAL: Duration = Duration::from_secs(1);
/// Model calls the whole database may start per minute and per day, retries
/// included. Identities are free to make, so the per-player limit alone does
/// not protect the model API key; these do. Above them prompts are turned
/// away until the window ends.
///
/// Sized for 50 people playing at once. The minute covers all of them
/// sending three prompts that each need their retry (50 * 3 * 2). The day
/// covers eight hours of a full house at a prompt a minute each, a quarter
/// of them retried.
const MAX_AGENT_CALLS_PER_MINUTE: u32 = 300;
const MAX_AGENT_CALLS_PER_DAY: u32 = 30_000;
const BUSY: &str = "The agents are busy. Try again in a moment.";

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
    /// An Axiom API token that may ingest into the dataset. When unset, log
    /// events only go to the module's own log (`spacetime logs`).
    pub AXIOM_TOKEN: Option<String>,
    /// Defaults to `promptnite`.
    pub AXIOM_DATASET: Option<String>,
    /// The dataset's edge deployment. Defaults to Axiom's US one.
    pub AXIOM_DOMAIN: Option<String>,
}

// Tables -------------------------------------------------------------------

/// Private. Clients see their own row through `my_player` and their room's
/// through `room_players`.
#[table(accessor = player)]
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
    /// Which edge of the arena is theirs (`0..rules::MAX_BOTS`): their bot
    /// starts there and their camera sits behind it. Handed out by `join`;
    /// no two online players of a room share one.
    #[default(0)]
    edge: u32,
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
/// Private: players see only the room they are in, through `my_room`.
#[table(accessor = room)]
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

/// "Is this player ready?" The prompt and script are kept apart, in
/// `bot_script`. Private; a room's rows are served by `room_submissions`.
#[table(accessor = submission)]
pub struct Submission {
    /// Validated allocation, ordered health, speed, power, reach, recovery.
    build: Vec<u32>,
    max_hp: i32,
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
/// own. Clients only get the `submission` status.
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
///
/// Private: the whole match exists from the moment the countdown starts, so
/// `match_frames` hands it out only once the fight is on.
#[table(accessor = frame)]
pub struct Frame {
    /// `frame_key(room_id, tick)`.
    #[primary_key]
    key: u64,
    #[index(btree)]
    room_id: u32,
    tick: u32,
    bots: Vec<BotFrame>,
}

/// Room in the high half, tick in the low half.
fn frame_key(room_id: u32, tick: u32) -> u64 {
    (room_id as u64) << 32 | tick as u64
}

/// First script error of each bot in a room's current match. Private, served
/// by `match_faults`.
#[table(accessor = match_fault)]
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

/// Outbox: log events on their way to Axiom. Private.
#[table(accessor = log_event)]
pub struct LogEvent {
    #[primary_key]
    #[auto_inc]
    id: u64,
    time: Timestamp,
    /// `error`, `warn`, `info`, ...
    level: String,
    message: String,
    /// A JSON object: the ids and numbers that go with the message.
    fields: String,
}

/// A pending run of `ship_logs`. There is one while events are waiting.
#[table(accessor = log_shipment, scheduled(ship_logs))]
pub struct LogShipment {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: ScheduleAt,
}

/// When a player last sent a prompt. Private.
#[table(accessor = prompt_limit)]
pub struct PromptLimit {
    #[primary_key]
    player_id: u32,
    last_prompt_at: Timestamp,
}

/// How many model calls were started in the current minute and day. One row.
/// Private.
#[table(accessor = agent_budget)]
pub struct AgentBudget {
    #[primary_key]
    id: u8,
    /// Minutes since the Unix epoch.
    minute: i64,
    calls_this_minute: u32,
    /// Days since the Unix epoch.
    day: i64,
    calls_today: u32,
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
        if player.online {
            emit(
                ctx,
                Level::Info,
                "player left",
                json!({ "player_id": player.id, "room_id": room_id }),
            );
        }
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
                edge: 0,
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

    // One edge each. Somebody coming back keeps theirs if it is still free.
    let taken: Vec<u32> = ctx
        .db
        .player()
        .room_id()
        .filter(room.id)
        .filter(|other| other.online && other.id != player.id)
        .map(|other| other.edge)
        .collect();
    let free = |edge: &u32| !taken.contains(edge);
    let Some(edge) = Some(player.edge)
        .filter(|edge| player.room_id == room.id && free(edge))
        .or_else(|| (0..rules::MAX_BOTS as u32).find(free))
    else {
        return Err(format!(
            "Room \"{}\" is full ({} players). Try another room id.",
            room.code,
            rules::MAX_BOTS
        ));
    };

    emit(
        ctx,
        Level::Info,
        "player joined",
        json!({
            "player_id": player.id,
            "identity": ctx.sender().to_string(),
            "name": name,
            "room_id": room.id,
            "room": room.code,
            "edge": edge,
        }),
    );
    let left = player.room_id;
    if left != room.id {
        // A script belongs to a round of the room it was written in.
        clear_entry(ctx, player.id);
    }
    ctx.db.player().identity().update(Player {
        name,
        room_id: room.id,
        online: true,
        edge,
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
    emit(
        ctx,
        Level::Info,
        "room settings changed",
        json!({ "player_id": player.id, "room_id": room_id, "match_secs": match_secs, "house_bot": house_bot }),
    );
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

    // One prompt a second per player. Returning an error undoes everything
    // this call wrote, so only prompts that got through count.
    let limits = ctx.db.prompt_limit();
    let limit = PromptLimit {
        player_id: player.id,
        last_prompt_at: ctx.timestamp,
    };
    match limits.player_id().find(player.id) {
        Some(last)
            if ctx
                .timestamp
                .duration_since(last.last_prompt_at)
                .is_some_and(|since| since < PROMPT_INTERVAL) =>
        {
            return Err("One prompt a second, please.".into());
        }
        Some(_) => {
            limits.player_id().update(limit);
        }
        None => {
            limits.insert(limit);
        }
    }

    clear_entry(ctx, player.id);
    ctx.db.submission().insert(Submission {
        player_id: player.id,
        room_id: room.id,
        round: room.round,
        status: SubmissionStatus::Generating,
        build: Vec::new(),
        max_hp: rules::MAX_HP,
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
    if !take_agent_call(ctx) {
        return Err(BUSY.into());
    }
    let job = ctx.db.agent_job().insert(AgentJob {
        id: 0, // auto_inc
        player_id: player.id,
        round: room.round,
        prompt,
        attempt: 1,
    });
    // The prompt itself is the player's and stays out of the logs.
    emit(
        ctx,
        Level::Info,
        "prompt submitted",
        json!({
            "job_id": job.id,
            "player_id": player.id,
            "room_id": room.id,
            "round": room.round,
            "prompt_chars": job.prompt.chars().count(),
        }),
    );
    call_agent(ctx, &job, String::new(), String::new());
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
        let job_id = call.job_id;
        ctx.with_tx(|tx| {
            emit(
                tx,
                Level::Error,
                "agent worker unreachable",
                json!({ "job_id": job_id, "player_id": call.player_id, "problem": problem }),
            );
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
            emit(
                ctx,
                Level::Info,
                "script accepted",
                json!({
                    "job_id": job.id,
                    "player_id": job.player_id,
                    "attempt": job.attempt,
                    "script_chars": script.chars().count(),
                }),
            );
            ctx.db.agent_job().id().delete(job.id);
            accept_script(ctx, job.player_id, script)
        }
        Err(error) if job.attempt < MAX_ATTEMPTS => {
            if !take_agent_call(ctx) {
                fail(ctx, job, BUSY.into());
                return Ok(());
            }
            emit(
                ctx,
                Level::Warn,
                "script rejected, asking again",
                json!({
                    "job_id": job.id,
                    "player_id": job.player_id,
                    "attempt": job.attempt,
                    "error": error,
                }),
            );
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

/// Count one model call against the database-wide budget. `false` when the
/// minute or the day is used up.
fn take_agent_call(ctx: &ReducerContext) -> bool {
    const MICROS_PER_MINUTE: i64 = 60_000_000;
    let minute = ctx.timestamp.to_micros_since_unix_epoch() / MICROS_PER_MINUTE;
    let day = minute / (24 * 60);

    let budget = ctx.db.agent_budget();
    let mut row = budget.id().find(0).unwrap_or_else(|| {
        budget.insert(AgentBudget {
            id: 0,
            minute,
            calls_this_minute: 0,
            day,
            calls_today: 0,
        })
    });
    if row.minute != minute {
        row.minute = minute;
        row.calls_this_minute = 0;
    }
    if row.day != day {
        row.day = day;
        row.calls_today = 0;
    }
    if row.calls_this_minute >= MAX_AGENT_CALLS_PER_MINUTE
        || row.calls_today >= MAX_AGENT_CALLS_PER_DAY
    {
        log::warn!("agent budget used up; turning a prompt away");
        return false;
    }
    row.calls_this_minute += 1;
    row.calls_today += 1;
    budget.id().update(row);
    true
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
    let stats = sim::LuaBot::new(&script, 0)?.stats;
    if let Some(row) = ctx.db.submission().player_id().find(player_id) {
        ctx.db.submission().player_id().update(Submission {
            build: vec![
                ((stats.max_hp - 60) / 20) as u32,
                ((stats.walk_speed - 2.2) / 0.4).round() as u32,
                ((stats.attack_damage - 6) / 2) as u32,
                ((stats.attack_range - 1.2) / 0.15).round() as u32,
                7 - stats.attack_cooldown,
            ],
            max_hp: stats.max_hp,
            ..row
        });
    }
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
    emit(
        ctx,
        Level::Warn,
        "job failed",
        json!({
            "job_id": job.id,
            "player_id": job.player_id,
            "attempt": job.attempt,
            "error": error,
        }),
    );
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
            edge: player.edge,
            script: script.script,
        });
    }
    // Stable spawn order regardless of table iteration order.
    bots.sort_by_key(|bot| bot.id);
    // The house bot takes an edge nobody is using; with four players there is
    // none left and it sits the match out.
    let free_edge = (0..rules::MAX_BOTS as u32).find(|edge| bots.iter().all(|b| b.edge != *edge));
    if room.house_bot
        && !bots.is_empty()
        && let Some(edge) = free_edge
    {
        bots.push(sim::BotSpec {
            id: HOUSE_BOT_ID,
            edge,
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
    let faults = outcome.faults.len();
    for fault in outcome.faults {
        emit(
            ctx,
            Level::Warn,
            "bot script fault",
            json!({
                "room_id": room_id,
                "round": room.round,
                "player_id": fault.bot_id,
                "tick": fault.tick,
                "error": fault.message,
            }),
        );
        ctx.db.match_fault().insert(MatchFault {
            id: 0, // auto_inc
            room_id,
            player_id: fault.bot_id,
            tick: fault.tick,
            message: fault.message,
        });
    }

    let match_ticks = outcome.frames.last().map_or(0, |frame| frame.tick);
    emit(
        ctx,
        Level::Info,
        "match simulated",
        json!({
            "room_id": room_id,
            "room": room.code,
            "round": room.round,
            "bots": bots.len(),
            "ticks": match_ticks,
            "winner": outcome.winner,
            "faults": faults,
        }),
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

// Logging ------------------------------------------------------------------
//
// The module, the agent worker and the game client log to one Axiom dataset
// with the same core fields (`source`, `level`, `message`, plus ids such as
// `job_id`, `room_id` and `player_id`), so one query can follow a job or a
// room across all three. The worker's half is in worker/src/log.ts.
//
// Reducers can't do HTTP, so `emit` writes to an outbox table and the
// `ship_logs` procedure posts it to Axiom a moment later.
//
// One thing this cannot do: when a reducer fails or panics, its transaction is
// rolled back and the events it emitted go with it. Those only show up in the
// module's own log.

/// Log something that happened. `fields` is a JSON object.
fn emit(ctx: &ReducerContext, level: Level, message: &str, fields: Value) {
    log::log!(level, "{message} {fields}");
    if ctx.env.AXIOM_TOKEN().is_none_or(|token| token.is_empty())
        || ctx.db.log_event().count() >= MAX_PENDING_LOGS
    {
        return;
    }
    ctx.db.log_event().insert(LogEvent {
        id: 0, // auto_inc
        time: ctx.timestamp,
        level: level.as_str().to_lowercase(),
        message: message.to_string(),
        fields: fields.to_string(),
    });
    if ctx.db.log_shipment().count() == 0 {
        ship_logs_at(ctx, ctx.timestamp + LOG_SHIP_DELAY);
    }
}

fn ship_logs_at(ctx: &ReducerContext, at: Timestamp) {
    ctx.db.log_shipment().insert(LogShipment {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(at),
    });
}

/// Posts waiting log events to Axiom. Runs from the `log_shipment` schedule.
#[procedure]
pub fn ship_logs(ctx: &mut ProcedureContext, shipment: LogShipment) {
    // Scheduled procedures are callable by anyone unless we check.
    if ctx.sender() != ctx.database_identity() {
        return;
    }
    let token = ctx.env.AXIOM_TOKEN().unwrap_or_default();
    let domain = ctx
        .env
        .AXIOM_DOMAIN()
        .filter(|domain| !domain.is_empty())
        .unwrap_or_else(|| DEFAULT_AXIOM_DOMAIN.into());
    let dataset = ctx
        .env
        .AXIOM_DATASET()
        .filter(|dataset| !dataset.is_empty())
        .unwrap_or_else(|| DEFAULT_AXIOM_DATASET.into());

    let batch = ctx.with_tx(|tx| {
        let mut events: Vec<LogEvent> = tx.db.log_event().iter().collect();
        events.sort_by_key(|event| event.id);
        events.truncate(LOG_BATCH);
        events
    });
    let body: Vec<Value> = batch
        .iter()
        .map(|event| {
            let mut object = match serde_json::from_str(&event.fields) {
                Ok(Value::Object(fields)) => fields,
                _ => Default::default(),
            };
            if let Ok(time) = event.time.to_rfc3339() {
                object.insert("_time".into(), time.into());
            }
            object.insert("source".into(), "module".into());
            object.insert("level".into(), event.level.clone().into());
            object.insert("message".into(), event.message.clone().into());
            Value::Object(object)
        })
        .collect();

    // Keep the events only when another try could work. Axiom answers 4xx for
    // a bad token or dataset, and repeating that would fill the outbox.
    let mut keep = false;
    if !batch.is_empty() && !token.is_empty() {
        let request = Request::builder()
            .uri(format!("https://{domain}/v1/ingest/{dataset}"))
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .extension(Timeout::from(TimeDuration::from(AXIOM_TIMEOUT)))
            .body(Value::Array(body).to_string());
        // Plain `log`, not `emit`: these are about the outbox itself.
        match request.map(|request| ctx.http.send(request)) {
            Ok(Ok(response)) if response.status().is_success() => {}
            Ok(Ok(response)) if response.status().is_client_error() => log::error!(
                "Axiom refused {} log events ({}): {}",
                batch.len(),
                response.status(),
                response.into_body().into_string_lossy()
            ),
            Ok(Ok(response)) => {
                log::warn!("Axiom answered {}, trying again later", response.status());
                keep = true;
            }
            Ok(Err(err)) => {
                log::warn!("could not reach Axiom, trying again later: {err}");
                keep = true;
            }
            Err(err) => log::error!("bad AXIOM_DOMAIN or AXIOM_DATASET: {err}"),
        }
    }

    ctx.with_tx(|tx| {
        if !keep {
            for event in &batch {
                tx.db.log_event().id().delete(event.id);
            }
        }
        tx.db
            .log_shipment()
            .scheduled_id()
            .delete(shipment.scheduled_id);
        // Events that arrived meanwhile saw this run's row and scheduled nothing.
        if tx.db.log_event().count() > 0 && tx.db.log_shipment().count() == 0 {
            let wait = if keep {
                LOG_RETRY_DELAY
            } else {
                Duration::ZERO
            };
            ship_logs_at(tx, tx.timestamp + wait);
        }
    });
}

// Views --------------------------------------------------------------------
//
// The only way clients read anything. Each one answers for the caller, so a
// client cannot ask about a room it is not in.

/// The caller's own player row.
#[view(accessor = my_player, public)]
fn my_player(ctx: &ViewContext) -> Option<Player> {
    ctx.db.player().identity().find(ctx.sender())
}

/// The room the caller is in right now, as stored.
fn callers_room(ctx: &ViewContext) -> Option<Room> {
    let player = my_player(ctx).filter(|player| player.online)?;
    ctx.db.room().id().find(player.room_id)
}

/// The room the caller is in right now.
#[view(accessor = my_room, public)]
fn my_room(ctx: &ViewContext) -> Option<Room> {
    let room = callers_room(ctx)?;
    Some(match room.phase {
        // How long the match lasts says whether somebody gets knocked out.
        Phase::Countdown => Room {
            match_ticks: 0,
            ..room
        },
        _ => room,
    })
}

/// Everyone in the caller's room.
#[view(accessor = room_players, public)]
fn room_players(ctx: &ViewContext) -> Vec<Player> {
    let Some(room) = callers_room(ctx) else {
        return Vec::new();
    };
    ctx.db.player().room_id().filter(room.id).collect()
}

/// Who in the caller's room is ready.
#[view(accessor = room_submissions, public)]
fn room_submissions(ctx: &ViewContext) -> Vec<Submission> {
    let Some(room) = callers_room(ctx) else {
        return Vec::new();
    };
    let caller_id = my_player(ctx).map(|p| p.id);
    ctx.db
        .submission()
        .room_id()
        .filter(room.id)
        .map(|mut submission| {
            if matches!(room.phase, Phase::Prompting | Phase::Countdown)
                && Some(submission.player_id) != caller_id
            {
                submission.build.clear();
                submission.max_hp = rules::MAX_HP;
            }
            submission
        })
        .collect()
}

/// The match of the caller's room. During the countdown that is only the
/// opening frame; the rest appears when the fight starts.
#[view(accessor = match_frames, public)]
fn match_frames(ctx: &ViewContext) -> Vec<Frame> {
    let Some(room) = callers_room(ctx) else {
        return Vec::new();
    };
    match room.phase {
        Phase::Countdown => ctx
            .db
            .frame()
            .key()
            .find(frame_key(room.id, 0))
            .into_iter()
            .collect(),
        _ => ctx.db.frame().room_id().filter(room.id).collect(),
    }
}

/// Script errors in the match of the caller's room, once the fight started.
#[view(accessor = match_faults, public)]
fn match_faults(ctx: &ViewContext) -> Vec<MatchFault> {
    match callers_room(ctx) {
        Some(room) if room.phase != Phase::Countdown => {
            ctx.db.match_fault().room_id().filter(room.id).collect()
        }
        _ => Vec::new(),
    }
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
