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
//! after 500 ms, and it may not call local addresses at all. Instead an agent
//! worker (`worker/`) connects like any other client and takes its work from
//! a queue, the `agent_job` table:
//!
//!   submit_prompt ──► agent_job row ──► `agent_work` subscription ──► worker
//!   deliver_script / fail_job  ◄──────────── reducer call ◄────────── worker
//!
//! The module never sends the worker its work, so local development needs no
//! address for the worker. A deployed worker goes to sleep when there is
//! nothing to do; then the module only knocks on its door (`AGENT_URL`) and
//! keeps the job until the worker has connected.
//!
//! The worker is a dumb pipe to the model. Everything that decides whether a
//! script is acceptable lives here: `deliver_script` validates the Lua, puts
//! an invalid script back in the queue with the error for another try, and
//! only marks the player ready once a script passed.

#[cfg(target_arch = "wasm32")]
mod entropy;

use std::time::Duration;

use promptnite_sim::{self as sim, rules};
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
/// How long one attempt may take before the job is failed. The worker gives
/// up on the model sooner (`MODEL_TIMEOUT_MS` in `worker/src/llm.ts`).
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);
/// The worker only has to wake up, not do anything. A module's HTTP requests
/// are cut off after 500 ms anyway.
const WAKE_TIMEOUT: Duration = Duration::from_millis(450);
/// `AgentJob::worker` of a job that is waiting for a worker to connect.
const NO_WORKER: Identity = Identity::ZERO;
const MAX_SCRIPT_CHARS: usize = 20_000;
const MAX_ERROR_CHARS: usize = 300;

/// Bot id of the built-in sparring partner, in rooms that have it switched on.
/// Player ids start at 1, so 0 is never a real player.
const HOUSE_BOT_ID: u32 = 0;

// Environment --------------------------------------------------------------

/// Set at publish time: `AGENT_SECRET=... AGENT_URL=... spacetime publish ...`.
#[spacetimedb::env]
pub struct Env {
    /// Password an agent worker shows in `register_worker` to be given work.
    /// When unset there is no agent and every prompt gets the example bot.
    pub AGENT_SECRET: Option<String>,
    /// Address of a deployed worker that sleeps while there is no work, e.g.
    /// `https://promptnite-agent.you.workers.dev`. A request to it wakes the
    /// worker up. When unset, a worker has to be connected already.
    pub AGENT_URL: Option<String>,
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

/// An agent worker that is connected right now. Private.
#[table(accessor = agent_worker)]
pub struct AgentWorker {
    #[primary_key]
    identity: Identity,
}

/// The work queue: a prompt an agent worker is turning into a script. The
/// row is there for as long as the work is open. Private; a worker reads its
/// own rows through the `agent_work` view.
#[table(accessor = agent_job)]
pub struct AgentJob {
    /// What the worker calls back with. A new prompt gets a new id, so an
    /// answer to an old prompt can't land on the new one.
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[unique]
    player_id: u32,
    /// The worker this job was given to, or `NO_WORKER` while it waits for
    /// one to wake up.
    #[index(btree)]
    worker: Identity,
    round: u32,
    prompt: String,
    /// 1 for the first script, 2 after one was rejected, ...
    attempt: u32,
    /// Both empty on the first attempt. On a retry: the script the module
    /// rejected and why, so the model can fix it.
    rejected_script: String,
    rejected_error: String,
}

/// Outbox: one row per wake-up call to the worker. Reducers can't do HTTP,
/// so they insert here and the `wake_agent` procedure sends it.
#[table(accessor = agent_wake, scheduled(wake_agent))]
pub struct AgentWake {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: ScheduleAt,
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

// Views --------------------------------------------------------------------

/// The jobs given to the calling agent worker. This is what a worker
/// subscribes to: a job shows up here when there is a script to write, shows
/// up again with the next `attempt` when that script was rejected, and is
/// gone once it is settled.
#[view(accessor = agent_work, public)]
fn agent_work(ctx: &ViewContext) -> Vec<AgentJob> {
    ctx.db.agent_job().worker().filter(ctx.sender()).collect()
}

// Lifecycle ----------------------------------------------------------------

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    if ctx.db.agent_worker().identity().delete(ctx.sender()) {
        // Nobody is writing these any more.
        let orphans: Vec<AgentJob> = ctx.db.agent_job().worker().filter(ctx.sender()).collect();
        let wake = agent_url(ctx).is_some() && !orphans.is_empty();
        for job in orphans {
            if wake {
                // A worker that sleeps may have nodded off just as the job
                // came in. Keep it for when the worker is back.
                ctx.db.agent_job().id().update(AgentJob {
                    worker: NO_WORKER,
                    ..job
                });
            } else {
                // Fail it now so the player doesn't sit out the timeout.
                fail(ctx, job, "The agent went offline. Try again.".into());
            }
        }
        if wake {
            wake_worker(ctx);
        }
    }
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

    if agent_secret(ctx).is_none() {
        // No agent configured: everyone gets the example bot, so the game
        // loop can be tried without a worker or an API key.
        return accept_script(ctx, player.id, sim::EXAMPLE_SCRIPT.to_string());
    }
    // Usually there is one worker. With several, the least busy one.
    let worker = ctx
        .db
        .agent_worker()
        .iter()
        .min_by_key(|worker| ctx.db.agent_job().worker().filter(worker.identity).count());
    let worker = match worker {
        Some(worker) => worker.identity,
        None if agent_url(ctx).is_some() => {
            // The worker is asleep. The job waits for it: see `register_worker`.
            wake_worker(ctx);
            NO_WORKER
        }
        None => {
            set_submission(
                ctx,
                player.id,
                SubmissionStatus::Failed,
                "The agent is offline. Try again later.".into(),
            );
            return Ok(());
        }
    };
    let job = ctx.db.agent_job().insert(AgentJob {
        id: 0, // auto_inc
        player_id: player.id,
        worker,
        round: room.round,
        prompt,
        attempt: 1,
        rejected_script: String::new(),
        rejected_error: String::new(),
    });
    start_timeout(ctx, &job);
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

/// An agent worker reports for duty. It is given the jobs that were waiting
/// for a worker, and from here on new prompts, until it disconnects.
#[reducer]
pub fn register_worker(ctx: &ReducerContext, secret: String) -> Result<(), String> {
    match agent_secret(ctx) {
        Some(expected) if expected == secret => {}
        _ => return Err("Wrong agent secret".into()),
    }
    if ctx
        .db
        .agent_worker()
        .identity()
        .find(ctx.sender())
        .is_none()
    {
        ctx.db.agent_worker().insert(AgentWorker {
            identity: ctx.sender(),
        });
        log::info!("agent worker {} registered", ctx.sender());
    }
    let waiting: Vec<AgentJob> = ctx.db.agent_job().worker().filter(NO_WORKER).collect();
    for job in waiting {
        ctx.db.agent_job().id().update(AgentJob {
            worker: ctx.sender(),
            ..job
        });
    }
    Ok(())
}

/// Knocks on the sleeping worker's door. Runs from the `agent_wake` schedule.
///
/// Any request wakes the worker; it then connects and registers by itself.
#[procedure]
pub fn wake_agent(ctx: &mut ProcedureContext, _wake: AgentWake) {
    // Scheduled procedures are callable by anyone unless we check.
    if ctx.sender() != ctx.database_identity() {
        log::warn!("wake_agent called by {}", ctx.sender());
        return;
    }
    let Some(url) = ctx.env.AGENT_URL().filter(|url| !url.is_empty()) else {
        return;
    };
    let request = Request::builder()
        .uri(url)
        .method("GET")
        .extension(Timeout::from(TimeDuration::from(WAKE_TIMEOUT)))
        .body(String::new());
    let problem = match request {
        Err(err) => format!("bad AGENT_URL: {err}"),
        Ok(request) => match ctx.http.send(request) {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => format!("it answered {}", response.status()),
            Err(err) => err.to_string(),
        },
    };
    log::error!("could not wake the agent worker: {problem}");
    ctx.with_tx(|tx| {
        // Unless a worker turned up anyway, nobody is coming for these.
        let waiting: Vec<AgentJob> = tx.db.agent_job().worker().filter(NO_WORKER).collect();
        for job in waiting {
            fail(
                tx,
                job,
                "Could not reach the agent. Try again later.".into(),
            );
        }
    });
}

/// The agent wrote a script. Called by the worker the job was given to.
///
/// This is the only place a generated script is judged. A valid one makes the
/// player ready. An invalid one goes back in the queue together with the
/// error; the player stays "generating" and nothing else changes until a
/// script passes or the attempts run out.
#[reducer]
pub fn deliver_script(ctx: &ReducerContext, job_id: u64, script: String) -> Result<(), String> {
    let job = job_for_worker(ctx, job_id)?;

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
                rejected_script: script,
                rejected_error: error,
                ..job
            });
            start_timeout(ctx, &job);
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
pub fn fail_job(ctx: &ReducerContext, job_id: u64, error: String) -> Result<(), String> {
    let job = job_for_worker(ctx, job_id)?;
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

fn agent_secret(ctx: &ReducerContext) -> Option<String> {
    ctx.env.AGENT_SECRET().filter(|secret| !secret.is_empty())
}

fn agent_url(ctx: &ReducerContext) -> Option<String> {
    ctx.env.AGENT_URL().filter(|url| !url.is_empty())
}

/// Have `wake_agent` run right after this transaction.
fn wake_worker(ctx: &ReducerContext) {
    ctx.db.agent_wake().insert(AgentWake {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp),
    });
}

/// Fail this attempt of the job if the worker hasn't answered in time.
fn start_timeout(ctx: &ReducerContext, job: &AgentJob) {
    ctx.db.job_timeout().insert(JobTimeout {
        scheduled_id: 0,
        scheduled_at: ScheduleAt::Time(ctx.timestamp + ATTEMPT_TIMEOUT),
        job_id: job.id,
        attempt: job.attempt,
    });
}

/// Find the job a worker is reporting on. Only the worker it was given to
/// may answer it, and only registered workers are ever given one.
fn job_for_worker(ctx: &ReducerContext, job_id: u64) -> Result<AgentJob, String> {
    ctx.db
        .agent_job()
        .id()
        .find(job_id)
        .filter(|job| job.worker == ctx.sender())
        .ok_or_else(|| {
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
