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
    net::{Conn, Server},
};

/// Must match `COUNTDOWN` in the server module. Only used for the big
/// 3-2-1 on screen; the server decides when the fight really starts.
const COUNTDOWN_SECS: f32 = 3.0;
const SIDE_PANEL_WIDTH: f32 = 300.0;
/// The house bot fills the arena when a player is alone.
const HOUSE_BOT_ID: u32 = 0;

#[derive(Resource, Default)]
struct Form {
    name: String,
    prompt: String,
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
struct Snapshot {
    game: Game,
    players: Vec<Player>,
    submissions: Vec<Submission>,
    faults: Vec<MatchFault>,
    me: Option<Player>,
    my_script: Option<BotScript>,
}

impl Snapshot {
    fn read(conn: &Conn) -> Option<Self> {
        let db = conn.db();
        let game = db.game().id().find(&0)?;
        let mut players: Vec<Player> = db.player().iter().collect();
        players.sort_by_key(|player| player.id);
        let me = conn
            .try_identity()
            .and_then(|identity| players.iter().find(|p| p.identity == identity).cloned());
        Some(Self {
            submissions: db
                .submission()
                .iter()
                .filter(|s| s.round == game.round)
                .collect(),
            faults: db.match_fault().iter().collect(),
            my_script: db.my_script().iter().find(|s| s.round == game.round),
            game,
            players,
            me,
        })
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
        .show(&mut root, |ui| side_panel(ui, snapshot.as_ref(), &server))
        .response
        .rect
        .width();

    let bottom_height = egui::Panel::bottom("prompt")
        .resizable(false)
        .show(&mut root, |ui| {
            ui.add_space(6.0);
            prompt_bar(ui, conn, snapshot.as_ref(), &mut form, &server);
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

fn side_panel(ui: &mut egui::Ui, snapshot: Option<&Snapshot>, server: &Server) {
    ui.add_space(8.0);
    ui.heading("promptnite");
    let Some(snapshot) = snapshot else {
        ui.label(format!("Connecting to {} …", server.uri));
        return;
    };
    let game = &snapshot.game;
    ui.label(format!(
        "Round {}  ·  {}",
        game.round,
        match game.phase {
            Phase::Prompting => "writing prompts",
            Phase::Countdown => "get ready",
            Phase::Fighting => "fight!",
            Phase::Results => "results",
        }
    ));

    ui.separator();
    ui.strong("Players");
    for player in snapshot.players.iter().filter(|player| player.online) {
        ui.horizontal(|ui| {
            let [r, g, b] = bot_color(player.id);
            ui.colored_label(egui::Color32::from_rgb(r, g, b), "⏺");
            let you = snapshot.me.as_ref().is_some_and(|me| me.id == player.id);
            ui.label(if you {
                format!("{} (you)", player.name)
            } else {
                player.name.clone()
            });
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

fn prompt_bar(
    ui: &mut egui::Ui,
    conn: Option<&Conn>,
    snapshot: Option<&Snapshot>,
    form: &mut Form,
    server: &Server,
) {
    let (Some(conn), Some(snapshot)) = (conn, snapshot) else {
        ui.label(format!(
            "Connecting to database \"{}\" at {} …",
            server.database, server.uri
        ));
        return;
    };

    let Some(me) = snapshot.me.as_ref().filter(|me| !me.name.is_empty()) else {
        ui.horizontal(|ui| {
            ui.label("Your name:");
            let field = ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(220.0));
            let name = form.name.trim();
            let clicked = ui
                .add_enabled(!name.is_empty(), egui::Button::new("Join"))
                .clicked();
            if (clicked || pressed_enter(ui, &field)) && !name.is_empty() {
                report(conn.reducers().join(name.to_string()));
            }
        });
        return;
    };

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
            ui.label("Your bot is ready. Waiting for the other players…");
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
