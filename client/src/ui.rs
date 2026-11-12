//! The 2D interface, drawn with egui on top of the arena.
//!
//! The arena fills the whole window and everything here floats over it:
//!
//!   +-------------------------------------+
//!   | settings     banner        players  |   countdown / winner; the room and who is in it;
//!   |                                     |   its settings, for the owner, or
//!   |                                     |   the match timer during a fight
//!   |               arena                 |   join screen: boxes in the middle
//!   |                                     |
//!   | +---------------------------------+ |
//!   | | prompt box (bottom 20%)         | |   or a one-line status instead
//!   | +---------------------------------+ |
//!   +-------------------------------------+
//!
//! The player goes through two screens: first their name and room, then
//! (every round) the prompt. Text boxes are 80% opaque so the arena shows through faintly.

use bevy::{
    camera::{CameraOutputMode, ClearColorConfig, visibility::RenderLayers},
    prelude::*,
    render::render_resource::BlendState,
};
use bevy_egui::{
    EguiContexts, EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass, PrimaryEguiContext, egui,
};
use promptnite_bindings::*;

use crate::{
    arena::{ArenaCamera, BotView, Replay},
    theme::{self, EDGE, bot_color, box_frame, pill, small},
};

/// Must match `COUNTDOWN` in the server module. Only used for the big
/// 3-2-1 on screen; the server decides when the fight really starts.
const COUNTDOWN_SECS: f32 = 3.0;
/// The house bot fights in rooms that have it switched on.
const HOUSE_BOT_ID: u32 = 0;
/// Players a room holds, one per edge. Must match `MAX_BOTS` in
/// `sim/src/rules.rs`.
const MAX_PLAYERS: usize = 4;
/// Same limits as the server module, so the boxes simply stop taking input
/// instead of the reducer rejecting it.
const MAX_NAME_CHARS: usize = 24;
const MAX_ROOM_CHARS: usize = 24;
const MAX_PROMPT_CHARS: usize = 2_000;
/// Match lengths the owner can pick from, in seconds. The server accepts
/// anything within its own limits (`sim/src/rules.rs`).
const MATCH_LENGTHS: [u32; 4] = [30, 60, 90, 120];

/// Share of the window height taken by the prompt box.
const PROMPT_BOX_SHARE: f32 = 0.2;
/// ...but never so small that a line of text and the footer don't fit.
const PROMPT_BOX_MIN_HEIGHT: f32 = 110.0;

/// What there is to show. Filled in every frame by `net` from the server, or
/// by `sandbox` from made-up data; the UI only reads it.
#[derive(Resource)]
pub enum Screen {
    Connecting(String),
    /// Name and room. `known_name` is what this player called themselves last
    /// time, `error` why the server turned the last attempt down.
    Join {
        known_name: Option<String>,
        error: Option<String>,
    },
    /// Joined, waiting for the server to put us in the room.
    Entering,
    Room(Box<Snapshot>),
}

/// What the player asked for. Carried out by `net` (or `sandbox`).
#[derive(Message, Clone)]
pub enum Action {
    Join { name: String, room: String },
    SubmitPrompt(String),
    SetRoomSettings { match_secs: u32, house_bot: bool },
}

#[derive(Resource, Default)]
struct Form {
    name: String,
    room: String,
    prompt: String,
    /// Whether `name` was already filled in from an earlier visit.
    prefilled: bool,
    /// The text box we last moved the keyboard focus to, so each screen
    /// grabs it once and typing works without clicking first.
    focused: Option<Field>,
    /// Decides which send shortcut the hint shows.
    mac: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Join,
    /// Room and round are part of it so every new round focuses the box again.
    Prompt(u32, u32),
}

pub fn plugin(app: &mut App) {
    app.add_plugins(EguiPlugin::default())
        .add_message::<Action>()
        .insert_resource(Form {
            mac: is_mac(),
            ..default()
        })
        .add_systems(Startup, setup)
        .add_systems(EguiPrimaryContextPass, draw);
}

fn setup(mut commands: Commands, mut settings: ResMut<EguiGlobalSettings>) {
    // egui gets its own camera, drawn after the arena camera and blended over
    // it, so the translucent boxes show the arena behind them.
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

fn is_mac() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.navigator().user_agent().ok())
            .is_some_and(|agent| agent.to_ascii_lowercase().contains("mac"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        cfg!(target_os = "macos")
    }
}

/// The room we are in and everything in it.
pub struct Snapshot {
    pub game: Room,
    /// Everyone in the room, online or not, by id.
    pub players: Vec<Player>,
    /// This round's.
    pub submissions: Vec<Submission>,
    pub faults: Vec<MatchFault>,
    pub me: Player,
}

impl Snapshot {
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

    /// Names of the other online players the round is still waiting for.
    fn waiting_on(&self) -> Vec<&str> {
        self.players
            .iter()
            .filter(|player| player.online && player.id != self.me.id)
            .filter(|player| {
                self.submission_of(player.id)
                    .is_none_or(|s| s.status != SubmissionStatus::Ready)
            })
            .map(|player| player.name.as_str())
            .collect()
    }
}

fn draw(
    mut contexts: EguiContexts,
    mut form: ResMut<Form>,
    screen: Res<Screen>,
    mut actions: MessageWriter<Action>,
    replay: Res<Replay>,
    camera: Single<(&Camera, &GlobalTransform), With<ArenaCamera>>,
    mut fonts_set: Local<bool>,
) -> Result {
    let ctx = &*contexts.ctx_mut()?;
    if !*fonts_set {
        theme::set_fonts(ctx);
        // Light widgets (buttons, checkboxes) to sit on the white panels.
        ctx.set_visuals(egui::Visuals::light());
        // No fade at the ends of scroll areas: egui paints it in a grey, not
        // in the white of the panel.
        ctx.all_styles_mut(|style| style.spacing.scroll.fade.strength = 0.0);
        *fonts_set = true;
    }
    let screen_rect = ctx.viewport_rect();
    let background = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("arena_overlay"),
    ));
    let centred = |text: String| {
        shadowed_text(
            &background,
            screen_rect.center(),
            text,
            theme::SIZE_BODY,
            theme::TEXT,
        )
    };

    let snapshot = match &*screen {
        Screen::Connecting(uri) => {
            centred(format!("Connecting to {uri} …"));
            return Ok(());
        }
        Screen::Entering => {
            centred("Entering the room…".into());
            return Ok(());
        }
        Screen::Join { known_name, error } => {
            join_screen(ctx, known_name, error, &mut form, &mut actions);
            return Ok(());
        }
        Screen::Room(snapshot) => snapshot,
    };
    let screen = screen_rect;

    let (camera, camera_transform) = *camera;
    bot_markers(&background, &replay, camera, camera_transform);
    bot_cards(&background, screen, snapshot, &replay);
    banner(&background, screen, snapshot, &replay);
    let clock = match_timer(&background, screen, &snapshot.game, &replay);
    faults(ctx, snapshot, &replay, clock);
    roster(&background, screen, snapshot, &replay);

    let game = &snapshot.game;
    if game.phase == Phase::Prompting && snapshot.i_own_the_room() {
        room_settings(ctx, game, &mut actions);
    }
    let submission = snapshot.submission_of(snapshot.me.id);
    match (game.phase, submission.map(|s| s.status)) {
        (Phase::Prompting, None | Some(SubmissionStatus::Failed)) => {
            let error = submission.map(|failed| failed.error.as_str());
            prompt_box(ctx, screen, snapshot, error, &mut form, &mut actions);
        }
        (Phase::Prompting, Some(SubmissionStatus::Generating)) => {
            status_line(ctx, true, "Your agent is building your bot…");
        }
        (Phase::Prompting, Some(SubmissionStatus::Ready)) => {
            let waiting = snapshot.waiting_on();
            let alone = snapshot.players.iter().filter(|p| p.online).count() < 2;
            let text = if !waiting.is_empty() {
                format!("Your bot is ready. Waiting for {}…", waiting.join(", "))
            } else if alone && !game.house_bot {
                "Your bot is ready. Waiting for somebody else to join the room…".into()
            } else {
                "Your bot is ready.".into()
            };
            status_line(ctx, false, &text);
        }
        // The big countdown and the fight speak for themselves.
        (Phase::Countdown | Phase::Fighting, _) => {}
        (Phase::Results, _) => status_line(ctx, false, "Next round starts in a moment."),
    }
    Ok(())
}

/// First screen, on every launch: who are you, and which room?
fn join_screen(
    ctx: &egui::Context,
    known_name: &Option<String>,
    error: &Option<String>,
    form: &mut Form,
    actions: &mut MessageWriter<Action>,
) {
    // Returning players get their old name suggested.
    if !form.prefilled
        && let Some(known) = known_name
    {
        form.prefilled = true;
        if form.name.is_empty() {
            form.name = known.clone();
        }
    }

    egui::Area::new(egui::Id::new("join_screen"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            box_frame().show(ui, |ui| {
                ui.set_width(320.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Who's playing, and where?")
                            .size(theme::SIZE_TITLE)
                            .color(theme::INK),
                    );
                    ui.add_space(12.0);

                    let name = text_field(ui, &mut form.name, "Your name", MAX_NAME_CHARS);
                    ui.add_space(4.0);
                    let room = text_field(ui, &mut form.room, "Room id", MAX_ROOM_CHARS);
                    if form.focused != Some(Field::Join) {
                        form.focused = Some(Field::Join);
                        name.request_focus();
                    }

                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Everyone who enters the same room id plays together.\n\
                         Press Enter to play",
                        )
                        .size(theme::SIZE_LABEL)
                        .color(theme::INK_MUTED),
                    );
                    // Why the server turned the last attempt away (room full).
                    if let Some(error) = error {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(error)
                                .size(theme::SIZE_LABEL)
                                .color(theme::BAD),
                        );
                    }

                    // A single-line box gives up the focus on Enter. Whichever box
                    // is still empty gets it next; with both filled in, we join.
                    let entered = ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if entered && (name.lost_focus() || room.lost_focus()) {
                        let (who, at) = (form.name.trim(), form.room.trim());
                        if who.is_empty() {
                            name.request_focus();
                        } else if at.is_empty() {
                            room.request_focus();
                        } else {
                            actions.write(Action::Join {
                                name: who.to_string(),
                                room: at.to_string(),
                            });
                        }
                    }
                });
            });
        });
}

/// One single-line box of the join screen.
fn text_field(ui: &mut egui::Ui, text: &mut String, hint: &str, limit: usize) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(hint.to_owned())
            .char_limit(limit)
            .font(egui::FontId::proportional(theme::SIZE_FIELD))
            .text_color(theme::INK)
            .horizontal_align(egui::Align::Center)
            .desired_width(f32::INFINITY)
            .frame(theme::field_frame()),
    )
}

/// Second screen, every round: the prompt, in the bottom fifth of the window.
/// Enter adds a line; Command+Enter (or Ctrl+Enter) sends.
fn prompt_box(
    ctx: &egui::Context,
    screen: egui::Rect,
    snapshot: &Snapshot,
    error: Option<&str>,
    form: &mut Form,
    actions: &mut MessageWriter<Action>,
) {
    let height = (screen.height() * PROMPT_BOX_SHARE).max(PROMPT_BOX_MIN_HEIGHT);
    let outer = egui::Rect::from_min_max(
        egui::pos2(screen.left(), screen.bottom() - height),
        screen.max,
    )
    .shrink(EDGE);
    let frame = box_frame();
    let inner = outer.size() - frame.total_margin().sum();

    egui::Area::new(egui::Id::new("prompt_box"))
        .fixed_pos(outer.min)
        .show(ctx, |ui| {
            frame.show(ui, |ui| {
                ui.set_min_size(inner);
                ui.set_max_size(inner);

                // Take the shortcut out of the input before the text box sees
                // it. Command is Ctrl outside macOS; Ctrl also works on a Mac.
                let shortcut = ui.input_mut(|input| {
                    input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
                        | input.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
                });

                let footer_height = 24.0;
                let text_height = inner.y - footer_height - ui.spacing().item_spacing.y;
                egui::ScrollArea::vertical()
                    .max_height(text_height)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let field = ui.add(
                            egui::TextEdit::multiline(&mut form.prompt)
                                .hint_text("Tell your agent how your bot should fight…")
                                .char_limit(MAX_PROMPT_CHARS)
                                .font(egui::FontId::proportional(theme::SIZE_BODY))
                                .text_color(theme::INK)
                                .desired_width(f32::INFINITY)
                                .min_size(egui::vec2(0.0, text_height))
                                // The box around it is already drawn.
                                .frame(egui::Frame::NONE),
                        );
                        let this = Field::Prompt(snapshot.game.id, snapshot.game.round);
                        if form.focused != Some(this) {
                            form.focused = Some(this);
                            field.request_focus();
                        }
                    });

                let mut clicked = false;
                ui.allocate_ui_with_layout(
                    egui::vec2(inner.x, footer_height),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let ready = !form.prompt.trim().is_empty();
                        clicked = ui.add_enabled(ready, egui::Button::new("Send")).clicked();
                        ui.label(small(if form.mac {
                            "⌘ Enter to send"
                        } else {
                            "Ctrl + Enter to send"
                        }));
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.label(small(&format!("Round {}", snapshot.game.round)));
                        });
                    },
                );

                let prompt = form.prompt.trim();
                if (shortcut || clicked) && !prompt.is_empty() {
                    actions.write(Action::SubmitPrompt(prompt.to_string()));
                }
            });
        });

    // Why the last prompt didn't work out, right above the box.
    if let Some(error) = error.filter(|error| !error.is_empty()) {
        egui::Area::new(egui::Id::new("prompt_error"))
            .anchor(
                egui::Align2::LEFT_BOTTOM,
                egui::vec2(EDGE, -(height - EDGE) - 6.0),
            )
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(screen.width() - 2.0 * EDGE);
                pill(ui, |ui| {
                    ui.label(
                        egui::RichText::new(error)
                            .size(theme::SIZE_LABEL)
                            .color(theme::BAD),
                    );
                });
            });
    }
}

/// The room's settings, top left. Only the owner gets them, and only while
/// prompting: that is when the next match is still to be set up.
fn room_settings(ctx: &egui::Context, game: &Room, actions: &mut MessageWriter<Action>) {
    egui::Area::new(egui::Id::new("room_settings"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(EDGE, EDGE))
        .show(ctx, |ui| {
            box_frame().show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Room settings")
                        .size(theme::SIZE_LABEL)
                        .color(theme::INK),
                );
                ui.horizontal(|ui| {
                    ui.label(small("Match length"));
                    for secs in MATCH_LENGTHS {
                        let chosen = game.match_secs == secs;
                        if ui.selectable_label(chosen, format!("{secs} s")).clicked() && !chosen {
                            actions.write(Action::SetRoomSettings {
                                match_secs: secs,
                                house_bot: game.house_bot,
                            });
                        }
                    }
                });
                let mut house_bot = game.house_bot;
                if ui
                    .checkbox(&mut house_bot, "House bot joins the fight")
                    .changed()
                {
                    actions.write(Action::SetRoomSettings {
                        match_secs: game.match_secs,
                        house_bot,
                    });
                }
            });
        });
}

/// One line at the bottom while there is nothing to type.
fn status_line(ctx: &egui::Context, busy: bool, text: &str) {
    egui::Area::new(egui::Id::new("status_line"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -EDGE - 6.0))
        .interactable(false)
        .show(ctx, |ui| {
            pill(ui, |ui| {
                ui.horizontal(|ui| {
                    if busy {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                    ui.label(
                        egui::RichText::new(text)
                            .size(theme::SIZE_STATUS)
                            .color(theme::INK),
                    );
                });
            });
        });
}

/// Everyone in the room and what they are up to, top right.
///
/// Painted directly instead of laid out by egui: an anchored `Area` is placed
/// using its size from the previous frame, so it would jump for a frame
/// whenever a state changed length.
fn roster(painter: &egui::Painter, screen: egui::Rect, snapshot: &Snapshot, replay: &Replay) {
    const DOT: f32 = 9.0;
    const GAP: f32 = 6.0;
    const COLUMN_GAP: f32 = 14.0;
    const ROW_GAP: f32 = 6.0;
    const PADDING: egui::Vec2 = egui::vec2(16.0, 12.0);

    let game = &snapshot.game;
    let muted = theme::INK_MUTED;
    let left = |text: String| {
        painter.layout_no_wrap(
            text,
            egui::FontId::proportional(theme::SIZE_LABEL),
            theme::INK,
        )
    };
    let right = |text: String, color| {
        painter.layout_no_wrap(text, egui::FontId::proportional(theme::SIZE_SMALL), color)
    };

    // First the room and its settings, then one row per player. A row
    // without a colour has no dot and starts at the left edge.
    let mut rows = vec![(
        None,
        left(format!("Room {}", game.code)),
        right(format!("{} s matches", game.match_secs), muted),
    )];
    for player in snapshot.players.iter().filter(|player| player.online) {
        let mut name = player.name.clone();
        if player.id == snapshot.me.id {
            name += " (you)";
        }
        if player.id == game.owner {
            name += "  ·  owner";
        }
        let status = snapshot.submission_of(player.id).map(|s| s.status);
        let (state, color) = bot_state(game, replay, player.id, status);
        rows.push((
            Some(bot_color(player.id)),
            left(name),
            right(state.into(), color),
        ));
    }
    // The house bot is listed while it is part of the game: switched on for
    // the match being prepared and with an edge left for it, or fighting in
    // the one on screen.
    let house_bot = match game.phase {
        Phase::Prompting => {
            let players = snapshot.players.iter().filter(|p| p.online).count();
            game.house_bot && players < MAX_PLAYERS
        }
        _ => replay.bots.iter().any(|bot| bot.id == HOUSE_BOT_ID),
    };
    if house_bot {
        // It never has to write anything.
        let ready = Some(SubmissionStatus::Ready);
        let (state, color) = bot_state(game, replay, HOUSE_BOT_ID, ready);
        rows.push((
            Some(bot_color(HOUSE_BOT_ID)),
            left(snapshot.name_of(HOUSE_BOT_ID)),
            right(state.into(), color),
        ));
    }

    let widest = |pick: fn(&(_, _, _)) -> f32| rows.iter().map(pick).fold(0.0, f32::max);
    let name_width = widest(|row| {
        // The room's row also spans the dot's column.
        row.1.size().x - if row.0.is_none() { DOT + GAP } else { 0.0 }
    });
    let state_width = widest(|row| row.2.size().x);
    let row_height = widest(|row| row.1.size().y.max(row.2.size().y));
    let size = egui::vec2(
        DOT + GAP + name_width + COLUMN_GAP + state_width,
        rows.len() as f32 * (row_height + ROW_GAP) - ROW_GAP,
    ) + PADDING * 2.0;
    let outer = egui::Rect::from_min_size(
        egui::pos2(screen.right() - EDGE - size.x, screen.top() + EDGE),
        size,
    );
    theme::panel(painter, outer);

    let mut y = outer.top() + PADDING.y;
    for (color, name, state) in rows {
        let x = outer.left() + PADDING.x;
        let middle = y + row_height / 2.0;
        let mut name_at = egui::pos2(x, middle - name.size().y / 2.0);
        if let Some([r, g, b]) = color {
            painter.circle_filled(
                egui::pos2(x + DOT / 2.0, middle),
                DOT / 2.0,
                egui::Color32::from_rgb(r, g, b),
            );
            name_at.x += DOT + GAP;
        }
        let state_at = egui::pos2(
            x + DOT + GAP + name_width + COLUMN_GAP,
            middle - state.size().y / 2.0,
        );
        painter.galley(name_at, name, theme::INK);
        painter.galley(state_at, state, theme::INK);
        y += row_height + ROW_GAP;
    }
}

/// What one player (or the house bot) is doing right now, in a word or two,
/// and its colour. `status` is where their prompt for this round stands.
fn bot_state(
    game: &Room,
    replay: &Replay,
    id: u32,
    status: Option<SubmissionStatus>,
) -> (&'static str, egui::Color32) {
    let (muted, good, bad) = (theme::INK_MUTED, theme::GOOD, theme::BAD);
    let in_match = replay.bots.iter().find(|bot| bot.id == id);
    let down = in_match.is_some_and(|bot| bot.action == BotAction::Dead);

    match (game.phase, status) {
        (Phase::Prompting, None) => ("writing a prompt", muted),
        (Phase::Prompting, Some(SubmissionStatus::Generating)) => ("agent is building", muted),
        (Phase::Prompting, Some(SubmissionStatus::Failed)) => ("agent failed", bad),
        (Phase::Prompting, Some(SubmissionStatus::Ready)) => ("ready", good),
        // Joined while a match was already set up: no bot in it.
        (Phase::Countdown | Phase::Fighting | Phase::Results, _) if in_match.is_none() => {
            ("watching", muted)
        }
        (Phase::Countdown, _) => ("ready", good),
        (Phase::Results, _) if game.winner == Some(id) => ("won", good),
        (Phase::Fighting | Phase::Results, _) if down => ("knocked out", bad),
        (Phase::Fighting, _) => ("fighting", theme::INK),
        (Phase::Results, _) => ("still standing", muted),
    }
}

/// Time left in the match, top left, while the fight is on. Returns the box
/// it took up, so whatever else lives in that corner can go below it.
///
/// It counts down from the room's match length. A fight that is decided
/// early ends with time still on the clock.
fn match_timer(
    painter: &egui::Painter,
    screen: egui::Rect,
    game: &Room,
    replay: &Replay,
) -> Option<egui::Rect> {
    const PADDING: egui::Vec2 = egui::vec2(14.0, 8.0);

    if game.phase != Phase::Fighting {
        return None;
    }
    // The replay stops at the last frame, and so does the clock.
    let fought = game.match_ticks as f32 / game.ticks_per_sec.max(1) as f32;
    let elapsed = replay.phase_elapsed.min(fought);
    let left = (game.match_secs as f32 - elapsed).ceil().max(0.0) as u32;

    // Monospace, so the box doesn't change width as the digits do.
    let text = painter.layout_no_wrap(
        format!("{}:{:02}", left / 60, left % 60),
        egui::FontId::monospace(theme::SIZE_TIMER),
        theme::INK,
    );
    let outer = egui::Rect::from_min_size(
        screen.min + egui::Vec2::splat(EDGE),
        text.size() + PADDING * 2.0,
    );
    theme::panel(painter, outer);
    painter.galley(outer.min + PADDING, text, theme::INK);
    Some(outer)
}

/// Scripts that broke during the match, top left (below the match timer),
/// from the moment the replay reaches the tick where it happened.
fn faults(ctx: &egui::Context, snapshot: &Snapshot, replay: &Replay, clock: Option<egui::Rect>) {
    let game = &snapshot.game;
    let shown_tick = match game.phase {
        Phase::Fighting => (replay.phase_elapsed * game.ticks_per_sec as f32) as u32,
        Phase::Results => u32::MAX,
        Phase::Prompting | Phase::Countdown => return,
    };
    let faults: Vec<&MatchFault> = snapshot
        .faults
        .iter()
        .filter(|fault| fault.tick <= shown_tick)
        .collect();
    if faults.is_empty() {
        return;
    }
    let top = clock.map_or(EDGE, |clock| {
        clock.bottom() - ctx.viewport_rect().top() + 6.0
    });
    egui::Area::new(egui::Id::new("faults"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(EDGE, top))
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_max_width(320.0);
            pill(ui, |ui| {
                for fault in faults {
                    let whose = if fault.player_id == snapshot.me.id {
                        "Your bot".to_string()
                    } else {
                        snapshot.name_of(fault.player_id)
                    };
                    ui.label(
                        egui::RichText::new(format!("{whose} broke down: {}", fault.message))
                            .size(theme::SIZE_SMALL)
                            .color(theme::BAD),
                    );
                }
            });
        });
}

fn shadowed_text(
    painter: &egui::Painter,
    at: egui::Pos2,
    text: String,
    size: f32,
    color: egui::Color32,
) {
    let font = egui::FontId::proportional(size);
    let drop = egui::vec2(size / 32.0, size / 21.0).max(egui::Vec2::splat(1.0));
    painter.text(
        at + drop,
        egui::Align2::CENTER_CENTER,
        &text,
        font.clone(),
        theme::TEXT_SHADOW,
    );
    painter.text(at, egui::Align2::CENTER_CENTER, text, font, color);
}

/// A down-pointing triangle in the bot's colour floating above every bot
/// still standing.
fn bot_markers(
    painter: &egui::Painter,
    replay: &Replay,
    camera: &Camera,
    camera_transform: &GlobalTransform,
) {
    const WIDTH: f32 = 16.0;
    const HEIGHT: f32 = 12.0;

    for bot in replay
        .bots
        .iter()
        .filter(|bot| bot.action != BotAction::Dead)
    {
        let above = bot.position + Vec3::Y * (bot.radius * 2.2);
        // The arena camera covers the whole window, so its viewport
        // coordinates are egui coordinates.
        let Ok(tip) = camera.world_to_viewport(camera_transform, above) else {
            continue;
        };
        let tip = egui::pos2(tip.x, tip.y);
        painter.add(egui::Shape::convex_polygon(
            vec![
                tip,
                tip + egui::vec2(-WIDTH / 2.0, -HEIGHT),
                tip + egui::vec2(WIDTH / 2.0, -HEIGHT),
            ],
            fill_of(bot),
            egui::Stroke::new(1.5, theme::TEXT_SHADOW),
        ));
    }
}

fn fill_of(bot: &BotView) -> egui::Color32 {
    if bot.action == BotAction::Dead {
        theme::TEXT_DEAD
    } else {
        let [r, g, b] = bot_color(bot.id);
        egui::Color32::from_rgb(r, g, b)
    }
}

/// One card per bot in the match down the left side, vertically centred:
/// colour dot and name, health bar underneath.
///
/// Painted directly, like the roster, so the cards don't jump a frame when
/// a name changes length.
fn bot_cards(painter: &egui::Painter, screen: egui::Rect, snapshot: &Snapshot, replay: &Replay) {
    const WIDTH: f32 = 196.0;
    const HEIGHT: f32 = 54.0;
    const GAP: f32 = 10.0;
    const DOT: f32 = 10.0;
    const PADDING: egui::Vec2 = egui::vec2(14.0, 12.0);

    let count = replay.bots.len() as f32;
    let mut top = screen.center().y - (count * (HEIGHT + GAP) - GAP) / 2.0;
    for bot in &replay.bots {
        let card = egui::Rect::from_min_size(
            egui::pos2(screen.left() + EDGE, top),
            egui::vec2(WIDTH, HEIGHT),
        );
        top += HEIGHT + GAP;
        theme::panel(painter, card);

        let inner = card.shrink2(PADDING);
        let alive = bot.action != BotAction::Dead;
        let name_y = inner.top() + 7.0;
        painter.circle_filled(
            egui::pos2(inner.left() + DOT / 2.0, name_y),
            DOT / 2.0,
            fill_of(bot),
        );
        painter.text(
            egui::pos2(inner.left() + DOT + 6.0, name_y),
            egui::Align2::LEFT_CENTER,
            snapshot.name_of(bot.id),
            egui::FontId::proportional(theme::SIZE_BOT_NAME),
            if alive { theme::INK } else { theme::INK_MUTED },
        );

        let bar = egui::Rect::from_min_max(
            egui::pos2(inner.left(), inner.bottom() - 6.0),
            inner.right_bottom(),
        );
        painter.rect_filled(bar, 3.0, theme::HP_TRACK);
        let health = (bot.hp as f32 / snapshot.game.max_hp.max(1) as f32).clamp(0.0, 1.0);
        if health > 0.0 {
            let filled =
                egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * health, bar.height()));
            painter.rect_filled(filled, 3.0, fill_of(bot));
        }
    }
}

/// The big text over the arena: countdown and result.
fn banner(painter: &egui::Painter, screen: egui::Rect, snapshot: &Snapshot, replay: &Replay) {
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
    // Top centre, clear of the arena.
    let at = screen.center_top() + egui::vec2(0.0, EDGE + theme::SIZE_BANNER / 2.0);
    shadowed_text(painter, at, text, theme::SIZE_BANNER, theme::TEXT);
}
