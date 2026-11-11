//! The 2D interface, drawn with egui on top of the arena.
//!
//! Layout, following the original sketch:
//!
//!   +---------------------------+---------+
//!   |                           | round   |
//!   |          arena            | players |
//!   |                           | script  |
//!   +---------------------------+         |
//!   | prompt bar                |         |
//!   +---------------------------+---------+
//!
//! Until the player has picked a room, the prompt bar is the "name + room"
//! form and the rest is empty.

use bevy::{
    camera::{CameraOutputMode, ClearColorConfig, Viewport, visibility::RenderLayers},
    prelude::*,
    render::render_resource::BlendState,
    window::PrimaryWindow,
};
use bevy_egui::{
    EguiContexts, EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass, PrimaryEguiContext, egui,
};
use promptnite_bindings::{sdk::Table, *};

use crate::{
    arena::{ArenaCamera, Replay, bot_color},
    net::{self, Conn, Server, Session},
};

/// Must match `COUNTDOWN` in the server module. Only used for the big
/// 3-2-1 on screen; the server decides when the fight really starts.
const COUNTDOWN_SECS: f32 = 3.0;
const SIDE_PANEL_WIDTH: f32 = 300.0;
/// The house bot fights in rooms that have it switched on.
const HOUSE_BOT_ID: u32 = 0;
/// Must match `MAX_NAME_CHARS` / `MAX_ROOM_CHARS` in the server module.
const MAX_NAME_CHARS: usize = 24;
const MAX_ROOM_CHARS: usize = 24;
/// Match lengths the owner can pick from, in seconds. The server accepts
/// anything within its own limits (`sim/src/rules.rs`).
const MATCH_LENGTHS: [u32; 4] = [30, 60, 90, 120];

#[derive(Resource, Default)]
struct Form {
    name: String,
    room: String,
    prompt: String,
    /// Whether `name` was already filled in from an earlier visit.
    prefilled: bool,
}

pub fn plugin(app: &mut App) {
    app.add_plugins(EguiPlugin::default())
        .init_resource::<Form>()
        .add_systems(Startup, setup)
        .add_systems(EguiPrimaryContextPass, draw);
}

fn setup(mut commands: Commands, mut settings: ResMut<EguiGlobalSettings>) {
    // egui gets its own full-window camera so the arena camera can be shrunk
    // to the area the panels leave free.
    settings.auto_create_primary_context = false;
    commands.spawn((
        PrimaryEguiContext,
        Camera2d,
        RenderLayers::none(),
        Camera {
            order: 1,
            output_mode: CameraOutputMode::Write {
                blend_state: Some(BlendState::ALPHA_BLENDING),
                clear_color: ClearColorConfig::None,
            },
            clear_color: ClearColorConfig::Custom(Color::NONE),
            ..default()
        },
    ));
}

/// Everything the panels need, read from the SDK cache once per frame.
/// `None` until the player is in a room.
struct Snapshot {
    game: Room,
    /// Everyone in the room, online or not.
    players: Vec<Player>,
    submissions: Vec<Submission>,
    faults: Vec<MatchFault>,
    me: Player,
    my_script: Option<BotScript>,
}

impl Snapshot {
    fn read(conn: &Conn) -> Option<Self> {
        let db = conn.db();
        let (me, game) = net::my_room(conn)?;
        // The cache can briefly hold rows of a room we just left.
        let mut players: Vec<Player> = db
            .player()
            .iter()
            .filter(|player| player.room_id == game.id)
            .collect();
        players.sort_by_key(|player| player.id);
        Some(Self {
            submissions: db
                .submission()
                .iter()
                .filter(|s| s.room_id == game.id && s.round == game.round)
                .collect(),
            faults: db
                .match_fault()
                .iter()
                .filter(|fault| fault.room_id == game.id)
                .collect(),
            my_script: db.my_script().iter().find(|s| s.round == game.round),
            game,
            players,
            me,
        })
    }

    fn i_own_the_room(&self) -> bool {
        self.game.owner == self.me.id
    }

    fn name_of(&self, id: u32) -> String {
        if id == HOUSE_BOT_ID {
            return "House bot".into();
        }
        self.players
            .iter()
            .find(|player| player.id == id)
            .map_or_else(|| format!("Player {id}"), |player| player.name.clone())
    }

    fn submission_of(&self, id: u32) -> Option<&Submission> {
        self.submissions.iter().find(|s| s.player_id == id)
    }
}

fn draw(
    mut contexts: EguiContexts,
    mut form: ResMut<Form>,
    mut session: ResMut<Session>,
    conn: Option<Res<Conn>>,
    server: Res<Server>,
    replay: Res<Replay>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&mut Camera, &GlobalTransform), With<ArenaCamera>>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let conn = conn.as_deref().filter(|conn| conn.is_active());
    let snapshot = conn.and_then(Snapshot::read);

    let mut root = egui::Ui::new(
        ctx.clone(),
        "root".into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );

    let side_width = egui::Panel::right("side")
        .exact_size(SIDE_PANEL_WIDTH)
        .resizable(false)
        .show(&mut root, |ui| {
            side_panel(ui, conn, snapshot.as_ref(), &server)
        })
        .response
        .rect
        .width();

    let bottom_height = egui::Panel::bottom("prompt")
        .resizable(false)
        .show(&mut root, |ui| {
            ui.add_space(6.0);
            prompt_bar(
                ui,
                conn,
                snapshot.as_ref(),
                &mut form,
                &mut session,
                &server,
            );
            ui.add_space(6.0);
        })
        .response
        .rect
        .height();

    // Give the arena camera whatever the panels left over.
    let (mut camera, camera_transform) = camera.into_inner();
    let free = Vec2::new(window.width() - side_width, window.height() - bottom_height);
    let physical = (free * window.scale_factor()).as_uvec2();
    if physical.x > 0 && physical.y > 0 {
        let viewport = Viewport {
            physical_position: UVec2::ZERO,
            physical_size: physical.min(window.physical_size()),
            ..default()
        };
        // Avoid touching the camera every frame; that would re-trigger
        // change detection for nothing.
        let unchanged = camera.viewport.as_ref().is_some_and(|current| {
            current.physical_size == viewport.physical_size
                && current.physical_position == viewport.physical_position
        });
        if !unchanged {
            camera.viewport = Some(viewport);
        }
    }

    if let Some(snapshot) = &snapshot {
        let arena = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(free.x, free.y));
        let painter = ctx
            .layer_painter(egui::LayerId::new(
                egui::Order::Background,
                egui::Id::new("arena_overlay"),
            ))
            .with_clip_rect(arena);
        bot_labels(&painter, snapshot, &replay, &camera, camera_transform);
        banner(&painter, arena, snapshot, &replay);
    }
    Ok(())
}

fn side_panel(
    ui: &mut egui::Ui,
    conn: Option<&Conn>,
    snapshot: Option<&Snapshot>,
    server: &Server,
) {
    ui.add_space(8.0);
    ui.heading("promptnite");
    let (Some(conn), Some(snapshot)) = (conn, snapshot) else {
        if conn.is_some() {
            ui.label("Pick a room to play in.");
            ui.weak("Everyone who enters the same room id plays together.");
        } else {
            ui.label(format!("Connecting to {} …", server.uri));
        }
        return;
    };
    let game = &snapshot.game;
    ui.label(format!(
        "Room {}  ·  round {}  ·  {}",
        game.code,
        game.round,
        match game.phase {
            Phase::Prompting => "writing prompts",
            Phase::Countdown => "get ready",
            Phase::Fighting => "fight!",
            Phase::Results => "results",
        }
    ));

    ui.separator();
    room_settings(ui, conn, snapshot);

    ui.separator();
    ui.strong("Players");
    for player in snapshot.players.iter().filter(|player| player.online) {
        ui.horizontal(|ui| {
            let [r, g, b] = bot_color(player.id);
            ui.colored_label(egui::Color32::from_rgb(r, g, b), "⏺");
            let mut name = player.name.clone();
            if player.id == snapshot.me.id {
                name += " (you)";
            }
            if player.id == game.owner {
                name += " · owner";
            }
            ui.label(name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match snapshot.submission_of(player.id).map(|s| s.status) {
                    None => ui.weak("thinking of a prompt"),
                    Some(SubmissionStatus::Generating) => ui.weak("agent is coding…"),
                    Some(SubmissionStatus::Ready) => {
                        ui.colored_label(egui::Color32::LIGHT_GREEN, "ready")
                    }
                    Some(SubmissionStatus::Failed) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, "failed")
                    }
                };
            });
        });
    }

    if game.house_bot {
        ui.horizontal(|ui| {
            let [r, g, b] = bot_color(HOUSE_BOT_ID);
            ui.colored_label(egui::Color32::from_rgb(r, g, b), "⏺");
            ui.label(snapshot.name_of(HOUSE_BOT_ID));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(egui::Color32::LIGHT_GREEN, "ready")
            });
        });
    }

    if !snapshot.faults.is_empty() && game.phase != Phase::Countdown {
        ui.separator();
        ui.strong("Script errors");
        for fault in &snapshot.faults {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!(
                    "{} (tick {}): {}",
                    snapshot.name_of(fault.player_id),
                    fault.tick,
                    fault.message
                ),
            );
        }
    }

    ui.separator();
    ui.strong("Your bot's script");
    match &snapshot.my_script {
        Some(script) if !script.script.is_empty() => {
            ui.add(
                egui::Label::new(egui::RichText::new(format!("Prompt: {}", script.prompt)).weak())
                    .wrap(),
            );
            // Scrolls both ways so long lines can't widen the panel.
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&script.script).monospace().size(11.0))
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
        }
        _ => {
            ui.weak("Your agent's Lua will show up here once it is written.");
        }
    }
}

/// The room's settings: controls for the owner, plain text for everyone else.
fn room_settings(ui: &mut egui::Ui, conn: &Conn, snapshot: &Snapshot) {
    let game = &snapshot.game;
    ui.strong("Room settings");
    if !snapshot.i_own_the_room() {
        ui.label(format!(
            "{} s matches, {} the house bot",
            game.match_secs,
            if game.house_bot { "with" } else { "without" }
        ));
        ui.weak(format!("Chosen by {}", snapshot.name_of(game.owner)));
        return;
    }

    ui.horizontal(|ui| {
        ui.label("Match length");
        for secs in MATCH_LENGTHS {
            let chosen = game.match_secs == secs;
            if ui.selectable_label(chosen, format!("{secs} s")).clicked() && !chosen {
                report(conn.reducers().set_room_settings(secs, game.house_bot));
            }
        }
    });
    let mut house_bot = game.house_bot;
    if ui
        .checkbox(&mut house_bot, "House bot joins the fight")
        .changed()
    {
        report(
            conn.reducers()
                .set_room_settings(game.match_secs, house_bot),
        );
    }
    ui.weak("Applies from the next match.");
}

/// Name and room id. Shown on every launch.
fn join_form(ui: &mut egui::Ui, conn: &Conn, form: &mut Form, session: &mut Session) {
    // Returning players get their old name suggested.
    if !form.prefilled
        && let Some(me) = net::me(conn)
    {
        form.prefilled = true;
        if form.name.is_empty() {
            form.name = me.name;
        }
    }

    ui.horizontal(|ui| {
        ui.label("Your name:");
        let name_field = ui.add(
            egui::TextEdit::singleline(&mut form.name)
                .char_limit(MAX_NAME_CHARS)
                .desired_width(180.0),
        );
        ui.label("Room id:");
        let room_field = ui.add(
            egui::TextEdit::singleline(&mut form.room)
                .char_limit(MAX_ROOM_CHARS)
                .hint_text("same id = same room")
                .desired_width(180.0),
        );
        let (name, room) = (form.name.trim(), form.room.trim());
        let filled = !name.is_empty() && !room.is_empty();
        let clicked = ui.add_enabled(filled, egui::Button::new("Join")).clicked();
        let entered = pressed_enter(ui, &name_field) || pressed_enter(ui, &room_field);
        if (clicked || entered) && filled {
            session.join(name.to_string(), room.to_string());
        }
    });
}

fn prompt_bar(
    ui: &mut egui::Ui,
    conn: Option<&Conn>,
    snapshot: Option<&Snapshot>,
    form: &mut Form,
    session: &mut Session,
    server: &Server,
) {
    let Some(conn) = conn else {
        ui.label(format!(
            "Connecting to database \"{}\" at {} …",
            server.database, server.uri
        ));
        return;
    };
    let Some(snapshot) = snapshot else {
        if session.has_joined() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Entering the room…");
            });
        } else {
            join_form(ui, conn, form, session);
        }
        return;
    };

    let me = &snapshot.me;
    let submission = snapshot.submission_of(me.id);
    match (snapshot.game.phase, submission.map(|s| s.status)) {
        (Phase::Prompting, None | Some(SubmissionStatus::Failed)) => {
            if let Some(failed) = submission {
                ui.colored_label(egui::Color32::LIGHT_RED, &failed.error);
            }
            ui.horizontal(|ui| {
                let button_width = 130.0;
                let field = ui.add(
                    egui::TextEdit::singleline(&mut form.prompt)
                        .hint_text("Tell your agent how the bot should fight…")
                        .desired_width(ui.available_width() - button_width),
                );
                let prompt = form.prompt.trim();
                let clicked = ui
                    .add_enabled(!prompt.is_empty(), egui::Button::new("Send to agent"))
                    .clicked();
                if (clicked || pressed_enter(ui, &field)) && !prompt.is_empty() {
                    report(conn.reducers().submit_prompt(prompt.to_string()));
                }
            });
        }
        (Phase::Prompting, Some(SubmissionStatus::Generating)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Your agent is writing the bot's Lua…");
            });
        }
        (Phase::Prompting, Some(SubmissionStatus::Ready)) => {
            let alone = snapshot.players.iter().filter(|p| p.online).count() < 2;
            ui.label(if alone && !snapshot.game.house_bot {
                "Your bot is ready. Waiting for somebody else to join the room…"
            } else {
                "Your bot is ready. Waiting for the other players…"
            });
        }
        (Phase::Countdown, _) => {
            ui.label("All bots are ready. The fight is about to start.");
        }
        (Phase::Fighting, _) => {
            ui.label("Hands off! Your bot is running the script your agent wrote.");
        }
        (Phase::Results, _) => {
            ui.label("Next round starts in a moment.");
        }
    }
}

fn pressed_enter(ui: &egui::Ui, field: &egui::Response) -> bool {
    field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
}

fn report<E: std::fmt::Display>(result: Result<(), E>) {
    if let Err(err) = result {
        warn!("could not call reducer: {err}");
    }
}

/// Name and health bar floating above every bot.
fn bot_labels(
    painter: &egui::Painter,
    snapshot: &Snapshot,
    replay: &Replay,
    camera: &Camera,
    camera_transform: &GlobalTransform,
) {
    for bot in &replay.bots {
        let above = bot.position + Vec3::Y * (bot.radius * 2.2);
        // The arena viewport starts at the window's top-left corner, so
        // viewport coordinates are egui coordinates.
        let Ok(at) = camera.world_to_viewport(camera_transform, above) else {
            continue;
        };
        let at = egui::pos2(at.x, at.y);
        let alive = bot.action != BotAction::Dead;

        painter.text(
            at - egui::vec2(0.0, 8.0),
            egui::Align2::CENTER_BOTTOM,
            snapshot.name_of(bot.id),
            egui::FontId::proportional(14.0),
            if alive {
                egui::Color32::WHITE
            } else {
                egui::Color32::GRAY
            },
        );

        let bar = egui::Rect::from_center_size(at, egui::vec2(46.0, 5.0));
        painter.rect_filled(bar, 2.0, egui::Color32::from_black_alpha(160));
        let health = (bot.hp as f32 / snapshot.game.max_hp.max(1) as f32).clamp(0.0, 1.0);
        if health > 0.0 {
            let [r, g, b] = bot_color(bot.id);
            let filled =
                egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * health, bar.height()));
            painter.rect_filled(filled, 2.0, egui::Color32::from_rgb(r, g, b));
        }
    }
}

/// The big text in the middle: countdown and result.
fn banner(painter: &egui::Painter, arena: egui::Rect, snapshot: &Snapshot, replay: &Replay) {
    let game = &snapshot.game;
    let text = match game.phase {
        Phase::Countdown => {
            let left = (COUNTDOWN_SECS - replay.phase_elapsed).ceil().max(1.0);
            format!("{left:.0}")
        }
        Phase::Results => match game.winner {
            Some(id) => format!("{} wins!", snapshot.name_of(id)),
            None => "Draw!".into(),
        },
        Phase::Fighting | Phase::Prompting => return,
    };
    let at = arena.center_top() + egui::vec2(0.0, arena.height() * 0.18);
    let font = egui::FontId::proportional(64.0);
    painter.text(
        at + egui::vec2(2.0, 3.0),
        egui::Align2::CENTER_CENTER,
        &text,
        font.clone(),
        egui::Color32::from_black_alpha(200),
    );
    painter.text(
        at,
        egui::Align2::CENTER_CENTER,
        text,
        font,
        egui::Color32::WHITE,
    );
}
