//! Design sandbox: the client with made-up data instead of a server, for
//! working on the look. `make sandbox`, or `?sandbox` in the browser.
//!
//! Every screen of the game is a key away (see the help in the window). Keys
//! only work while no text box has the focus; Esc gives it up.
//!
//! `PROMPTNITE_SHOTS=<dir> make sandbox` (desktop) instead goes through every
//! screen on its own, saves a PNG of each into `<dir>` and quits: a quick
//! before/after of a restyle.
//!
//! `PROMPTNITE_VIDEO=<dir> make sandbox` (desktop) plays the fight with time
//! stepped exactly 1/30 s a frame, saves every frame into `<dir>` and quits.
//! Turn them into a video with
//! `ffmpeg -framerate 30 -i <dir>/%04d.png -pix_fmt yuv420p fight.mp4`.

use std::f32::consts::FRAC_PI_2;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};
use promptnite_bindings::{sdk::Identity, *};

use crate::{
    arena::{self, BotView, Replay},
    net, theme,
    ui::{Action, Screen, Snapshot},
};

pub fn enabled() -> bool {
    std::env::args().any(|arg| arg == "--sandbox")
        || net::setting("sandbox", "PROMPTNITE_SANDBOX").is_some()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Connecting,
    Join,
    JoinRefused,
    Entering,
    Prompting,
    PromptFailed,
    Generating,
    Waiting,
    Countdown,
    Fighting,
    Results,
}

/// In the order a player meets them. Keys 1-9 jump to the first nine.
const STAGES: [(Stage, &str); 11] = [
    (Stage::Connecting, "connecting"),
    (Stage::Join, "join"),
    (Stage::JoinRefused, "join refused"),
    (Stage::Entering, "entering"),
    (Stage::Prompting, "prompting"),
    (Stage::PromptFailed, "prompt failed"),
    (Stage::Generating, "agent building"),
    (Stage::Waiting, "waiting for others"),
    (Stage::Countdown, "countdown"),
    (Stage::Fighting, "fight"),
    (Stage::Results, "results"),
];

/// The made-up fight is this long, whatever the match length says.
const FIGHT_SECS: f32 = 12.0;
const TICKS_PER_SEC: u32 = 10;
const ROOM: u32 = 1;
const ROUND: u32 = 3;
const ME: u32 = 1;

#[derive(Resource)]
struct Sandbox {
    stage: usize,
    /// When the stage began, in app time (stops while paused).
    since: f64,
    /// Show the room as its owner (with the settings) or as a guest.
    owner: bool,
    match_secs: u32,
    house_bot: bool,
    teleporters: Vec<Teleporter>,
    help: bool,
    style_editor: bool,
}

impl Sandbox {
    fn stage(&self) -> Stage {
        STAGES[self.stage].0
    }

    fn go(&mut self, stage: Stage, now: f64) {
        self.stage = STAGES.iter().position(|(s, _)| *s == stage).unwrap();
        self.since = now;
    }
}

pub fn plugin(app: &mut App) {
    app.insert_resource(Sandbox {
        stage: 4,
        since: 0.0,
        owner: true,
        match_secs: 30,
        house_bot: true,
        teleporters: [(-6.0, -5.0), (6.0, 5.0), (-5.5, 6.0)]
            .into_iter()
            .zip(1..)
            .map(|((x, y), number)| pad(number, x, y))
            .collect(),
        help: false,
        style_editor: false,
    })
    .insert_resource(Screen::Entering)
    .add_systems(
        Update,
        (perform, advance, publish_screen)
            .chain()
            .before(arena::replay),
    )
    .add_systems(Update, fight.after(arena::replay).before(arena::sync_bots))
    .add_systems(EguiPrimaryContextPass, controls);

    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(dir) = std::env::var("PROMPTNITE_SHOTS") {
        std::fs::create_dir_all(&dir).expect("could not create the screenshot directory");
        app.insert_resource(Shots {
            dir: dir.into(),
            stage: 0,
            since: 0.0,
            taken: false,
        })
        .add_systems(Update, take_shots.after(publish_screen));
    }

    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(dir) = std::env::var("PROMPTNITE_VIDEO") {
        std::fs::create_dir_all(&dir).expect("could not create the video directory");
        app.insert_resource(Video(dir.into(), 0))
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f64(1.0 / VIDEO_FPS),
            ))
            .add_systems(Update, record_video.after(publish_screen));
    }
}

#[cfg(not(target_arch = "wasm32"))]
const VIDEO_FPS: f64 = 30.0;

/// Where the frames go, and how many have been saved.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource)]
struct Video(std::path::PathBuf, u32);

/// Every frame of the fight, then quit.
#[cfg(not(target_arch = "wasm32"))]
fn record_video(
    mut commands: Commands,
    mut video: ResMut<Video>,
    mut sandbox: ResMut<Sandbox>,
    time: Res<Time>,
    mut exit: MessageWriter<AppExit>,
) {
    use bevy::render::view::screenshot::{Screenshot, save_to_disk};

    let now = time.elapsed_secs_f64();
    sandbox.help = false;
    if video.1 == 0 && sandbox.stage() != Stage::Fighting {
        // A second of the opening frame first, for the window to settle.
        if now >= 1.0 {
            sandbox.go(Stage::Fighting, now);
        }
        return;
    }
    if sandbox.stage() != Stage::Fighting {
        // Frames left after the fight: wait for the last files to land.
        if now - sandbox.since > 1.0 {
            exit.write(AppExit::Success);
        }
        return;
    }
    video.1 += 1;
    let file = video.0.join(format!("{:04}.png", video.1));
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(file));
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource)]
struct Shots {
    dir: std::path::PathBuf,
    stage: usize,
    /// Real time the stage was entered at.
    since: f64,
    taken: bool,
}

/// One screenshot per stage, then quit. Each stage gets a moment to settle
/// (egui lays out over a couple of frames; the fight needs to get going).
#[cfg(not(target_arch = "wasm32"))]
fn take_shots(
    mut commands: Commands,
    mut shots: ResMut<Shots>,
    mut sandbox: ResMut<Sandbox>,
    real: Res<Time<Real>>,
    time: Res<Time>,
    mut exit: MessageWriter<AppExit>,
) {
    use bevy::render::view::screenshot::{Screenshot, save_to_disk};

    let now = real.elapsed_secs_f64();
    let Some(&(stage, name)) = STAGES.get(shots.stage) else {
        // Give the last file a moment to be written.
        if now - shots.since > 1.0 {
            exit.write(AppExit::Success);
        }
        return;
    };
    sandbox.help = false;
    if sandbox.stage() != stage {
        sandbox.go(stage, time.elapsed_secs_f64());
        shots.since = now;
    }
    let settle = match stage {
        // Bot 3 goes down at 10.25 s: catch its blast.
        Stage::Fighting => 10.4,
        Stage::Countdown => 2.0,
        _ => 3.0,
    };
    let waited = now - shots.since;
    if !shots.taken && waited >= settle {
        let file = format!("{:02}-{}.png", shots.stage + 1, name.replace(' ', "-"));
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(shots.dir.join(file)));
        shots.taken = true;
    } else if shots.taken && waited >= settle + 0.3 {
        shots.stage += 1;
        shots.since = now;
        shots.taken = false;
    }
}

fn pad(number: u32, x: f32, y: f32) -> Teleporter {
    Teleporter {
        key: number as u64,
        room_id: ROOM,
        number,
        x,
        y,
    }
}

/// What the UI asked for, played along with.
fn perform(mut actions: MessageReader<Action>, mut sandbox: ResMut<Sandbox>, time: Res<Time>) {
    let now = time.elapsed_secs_f64();
    for action in actions.read() {
        match action {
            Action::Join { .. } => sandbox.go(Stage::Prompting, now),
            Action::SubmitPrompt(_) => sandbox.go(Stage::Generating, now),
            Action::SetRoomSettings {
                match_secs,
                house_bot,
            } => {
                sandbox.match_secs = *match_secs;
                sandbox.house_bot = *house_bot;
            }
            Action::AddTeleporter { x, y } => {
                // The smallest number not in use, like the server.
                let pads = &mut sandbox.teleporters;
                let number = (1..)
                    .find(|n| pads.iter().all(|pad| pad.number != *n))
                    .unwrap();
                pads.push(pad(number, *x, *y));
                pads.sort_by_key(|pad| pad.number);
            }
            Action::RemoveTeleporter(number) => {
                sandbox.teleporters.retain(|pad| pad.number != *number);
            }
        }
    }
}

/// Countdown, fight and results follow each other like in a real match.
fn advance(mut sandbox: ResMut<Sandbox>, time: Res<Time>) {
    let now = time.elapsed_secs_f64();
    let next = match sandbox.stage() {
        Stage::Countdown => (3.0, Stage::Fighting),
        Stage::Fighting => (FIGHT_SECS as f64, Stage::Results),
        _ => return,
    };
    if now - sandbox.since >= next.0 {
        sandbox.go(next.1, now);
    }
}

fn publish_screen(sandbox: Res<Sandbox>, mut screen: ResMut<Screen>) {
    *screen = match sandbox.stage() {
        Stage::Connecting => Screen::Connecting("sandbox (no server)".into()),
        Stage::Join => Screen::Join {
            known_name: None,
            error: None,
        },
        Stage::JoinRefused => Screen::Join {
            known_name: Some("Ada".into()),
            error: Some("Room \"den\" is full (4 players). Try another room ID.".into()),
        },
        Stage::Entering => Screen::Entering,
        stage => Screen::Room(Box::new(snapshot(&sandbox, stage))),
    };
}

fn snapshot(sandbox: &Sandbox, stage: Stage) -> Snapshot {
    use SubmissionStatus::*;

    let player = |id, name: &str, edge| Player {
        identity: Identity::ZERO,
        id,
        name: name.into(),
        room_id: ROOM,
        online: true,
        edge,
    };
    let players = vec![player(ME, "Ada", 0), player(2, "Bo", 1), player(3, "Cy", 2)];

    let phase = match stage {
        Stage::Countdown => Phase::Countdown,
        Stage::Fighting => Phase::Fighting,
        Stage::Results => Phase::Results,
        _ => Phase::Prompting,
    };
    let submission = |player_id, status, error: &str| Submission {
        player_id,
        room_id: ROOM,
        round: ROUND,
        status,
        build: if status == Ready {
            Some(match player_id {
                1 => vec![1, 4, 1, 3, 1],
                2 => vec![5, 0, 3, 0, 2],
                _ => vec![2, 2, 2, 2, 2],
            })
        } else {
            None
        },
        max_hp: match player_id {
            1 => 80,
            2 => 160,
            _ => 100,
        },
        error: error.into(),
    };
    let bo = if phase == Phase::Prompting {
        Generating
    } else {
        Ready
    };
    let mut submissions = vec![submission(2, bo, ""), submission(3, Ready, "")];
    submissions.extend(match stage {
        Stage::Prompting => None,
        Stage::PromptFailed => Some(submission(
            ME,
            Failed,
            "The agent's script didn't load: [string \"bot\"]:12: 'end' expected near <eof>",
        )),
        Stage::Generating => Some(submission(ME, Generating, "")),
        _ => Some(submission(ME, Ready, "")),
    });

    Snapshot {
        game: Room {
            id: ROOM,
            code: "den".into(),
            owner: if sandbox.owner { ME } else { 2 },
            match_secs: sandbox.match_secs,
            house_bot: sandbox.house_bot,
            round: ROUND,
            phase,
            match_ticks: FIGHT_SECS as u32 * TICKS_PER_SEC,
            winner: (stage == Stage::Results).then_some(ME),
            ticks_per_sec: TICKS_PER_SEC,
            arena_half_size: 10.0,
            bot_radius: 0.5,
            attack_range: 1.5,
            max_hp: 100,
            phase_ends_at: None,
        },
        faults: vec![MatchFault {
            id: 1,
            room_id: ROOM,
            player_id: 3,
            tick: 6 * TICKS_PER_SEC,
            message: "bot:7: attempt to index a nil value (local 'target')".into(),
        }],
        me: players[0].clone(),
        teleporters: sandbox.teleporters.clone(),
        prompt_refusal: None,
        players,
        submissions,
    }
}

/// A made-up brawl: everyone walks in from their edge, circles the middle
/// trading punches, and two of them go down. Between rounds the last frame
/// stays up, like in the real game.
fn fight(sandbox: Res<Sandbox>, mut replay: ResMut<Replay>) {
    let t = match sandbox.stage() {
        Stage::Countdown => 0.0,
        Stage::Fighting => replay.phase_elapsed.min(FIGHT_SECS),
        Stage::Connecting | Stage::Join | Stage::JoinRefused | Stage::Entering => return,
        _ => FIGHT_SECS,
    };
    let counting_down = sandbox.stage() == Stage::Countdown;

    // (id, edge, hp lost per second once they are in the middle)
    let mut bots = vec![(ME, 0, 5.0), (2, 1, 9.0), (3, 2, 16.0)];
    if sandbox.house_bot {
        bots.push((0, 3, 13.0));
    }
    const WALK_IN: f32 = 4.0;
    for (i, (id, edge, burn)) in bots.into_iter().enumerate() {
        let max_hp = match id {
            1 => 80,
            2 => 160,
            _ => 100,
        };
        let hp = (max_hp as f32 - burn * (t - WALK_IN).max(0.0)).max(0.0);
        // Lose it 10 at a time, like real hits (`ATTACK_DAMAGE`).
        let hp = (hp / 10.0).ceil() * 10.0;
        // A knocked-out bot stays where it fell.
        let moving = if hp > 0.0 {
            t
        } else {
            WALK_IN + max_hp as f32 / burn
        };

        // Same edges as `edge_direction` in `sim/src/rules.rs`.
        let start = match edge {
            0 => FRAC_PI_2,
            1 => -FRAC_PI_2,
            2 => 0.0,
            _ => std::f32::consts::PI,
        };
        // Two short dash bursts on the way in. Integrate their extra speed
        // so the demo path stays continuous and dust can follow the feet.
        let dash_extra = (moving - 0.5).clamp(0.0, 0.35) + (moving - 1.5).clamp(0.0, 0.35);
        let walked = ((moving + dash_extra * 2.6) / (WALK_IN + 1.82)).clamp(0.0, 1.0);
        let closing = walked * walked * (3.0 - 2.0 * walked);
        let radius = 8.5 + (2.2 - 8.5) * closing;
        let spin = if i % 2 == 0 { 0.35 } else { -0.35 };
        let angle = start + spin * (moving - WALK_IN).max(0.0);

        let beat = (t * 1.5 + i as f32 * 0.37).fract();
        let action = if hp <= 0.0 {
            BotAction::Dead
        } else if counting_down {
            BotAction::Idle
        } else if (0.5..0.85).contains(&t) || (1.5..1.85).contains(&t) {
            BotAction::Dash
        } else if t < WALK_IN || beat >= 0.2 {
            BotAction::Walk
        } else if beat < 0.12 {
            BotAction::Attack
        } else if i % 2 == 1 {
            BotAction::Shield
        } else {
            BotAction::Miss
        };

        replay.bots.push(BotView {
            max_hp,
            id,
            position: Vec3::new(radius * angle.cos(), 0.5, radius * angle.sin()),
            radius: 0.5,
            hp: hp as i32,
            // Drained by the brawl, a different amount for everybody.
            stamina: if hp > 0.0 && t > WALK_IN {
                0.55 + 0.4 * (t * 0.9 + i as f32 * 1.7).sin()
            } else {
                1.0
            },
            action,
            // Nobody really jumps in the made-up fight, but every few seconds
            // the first two pads flare as if somebody had.
            teleport: match &sandbox.teleporters[..] {
                [from, to, ..] if id == ME && !counting_down && (t % 3.0) > 2.9 => {
                    Some((Vec3::new(from.x, 0.0, from.y), Vec3::new(to.x, 0.0, to.y)))
                }
                _ => None,
            },
        });
    }
}

/// Keys, the help box and the style editor.
fn controls(
    mut contexts: EguiContexts,
    mut sandbox: ResMut<Sandbox>,
    mut time: ResMut<Time<Virtual>>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    if !theme::fonts_ready(ctx) {
        return Ok(());
    }
    let now = time.elapsed_secs_f64();
    if crate::ui::compact_view(ctx.viewport_rect().width()) && !ctx.text_edit_focused() {
        // Touch-accessible stage picker for the phone testing harness.
        egui::Area::new(egui::Id::new("sandbox_stages"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -112.0))
            .show(ctx, |ui| {
                theme::box_frame().show(ui, |ui| {
                    ui.set_width(100.0);
                    ui.menu_button("Test screen", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(240.0)
                            .show(ui, |ui| {
                                for (index, (stage, name)) in STAGES.iter().enumerate() {
                                    if ui.selectable_label(sandbox.stage == index, *name).clicked()
                                    {
                                        sandbox.go(*stage, now);
                                        ui.close();
                                    }
                                }
                            });
                    });
                });
            });
    }

    if !ctx.egui_wants_keyboard_input() {
        let pressed = |key| ctx.input(|input| input.key_pressed(key));
        let last = STAGES.len() - 1;
        let mut stage = sandbox.stage;
        if pressed(egui::Key::ArrowRight) {
            stage = (stage + 1).min(last);
        }
        if pressed(egui::Key::ArrowLeft) {
            stage = stage.saturating_sub(1);
        }
        const DIGITS: [egui::Key; 9] = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
        ];
        if let Some(digit) = DIGITS.iter().position(|&key| pressed(key)) {
            stage = digit;
        }
        if stage != sandbox.stage {
            sandbox.go(STAGES[stage].0, now);
        }
        if pressed(egui::Key::O) {
            sandbox.owner = !sandbox.owner;
        }
        if pressed(egui::Key::Space) {
            if time.is_paused() {
                time.unpause();
            } else {
                time.pause();
            }
        }
        if pressed(egui::Key::E) {
            sandbox.style_editor = !sandbox.style_editor;
        }
        if pressed(egui::Key::H) {
            sandbox.help = !sandbox.help;
        }
    }

    theme::style_editor(ctx, &mut sandbox.style_editor);

    if sandbox.help {
        let (number, (_, name)) = (sandbox.stage + 1, STAGES[sandbox.stage]);
        let paused = if time.is_paused() { "  ·  paused" } else { "" };
        let role = if sandbox.owner { "owner" } else { "guest" };
        egui::Area::new(egui::Id::new("sandbox_help"))
            .anchor(egui::Align2::LEFT_CENTER, egui::vec2(theme::EDGE, 0.0))
            .interactable(false)
            .show(ctx, |ui| {
                theme::pill(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "SANDBOX  {number}/{}  {name}  ({role}){paused}",
                            STAGES.len()
                        ))
                        .font(theme::bold(theme::SIZE_SMALL))
                        .color(theme::INK),
                    );
                    for line in [
                        "← →  or  1-9   screens",
                        "O   owner / guest",
                        "Space   pause",
                        "E   style editor",
                        "H   hide this",
                        "Esc   leave a text box",
                    ] {
                        ui.label(theme::small(line));
                    }
                });
            });
    }
    Ok(())
}
