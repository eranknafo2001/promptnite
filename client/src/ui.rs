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
//!   | | prompt box                      | |   or a one-line status instead
//!   | +---------------------------------+ |
//!   +-------------------------------------+
//!
//! Narrow windows stack the room header and prompt above the arena. Detailed
//! player status and owner settings move into the room menu.
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
    clipboard, teleporter,
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
const JOIN_CONTROL_HEIGHT: f32 = 44.0;
const JOIN_CONTROL_GAP: f32 = 8.0;
const BOT_PANEL_WIDTH: f32 = 350.0;
pub const BOT_TAB_HEIGHT: f32 = 152.0;

/// A sharp launch followed by a long settle, staggered by bot order.
pub fn bot_tab_reveal(elapsed: f32, index: usize) -> f32 {
    let t = ((elapsed - index as f32 * 0.14) / 0.8).clamp(0.0, 1.0);
    if t >= 1.0 {
        1.0
    } else {
        1.0 - 2.0_f32.powf(-12.0 * t)
    }
}
/// Match lengths the owner can pick from, in seconds. The server accepts
/// anything within its own limits (`sim/src/rules.rs`).
const MATCH_LENGTHS: [u32; 4] = [30, 60, 90, 120];

/// Share of the window width taken by the prompt column on the right, and
/// its limits. The camera frames the arena in what is left (see
/// `arena::STRATEGY_VIEW`).
const PROMPT_COLUMN_SHARE: f32 = 0.34;
const PROMPT_COLUMN_WIDTH: std::ops::RangeInclusive<f32> = 360.0..=540.0;

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

/// Camera-facing UI state: placement uses the unobstructed arena view.
#[derive(Resource, Default)]
pub struct ArenaUi {
    pub placing: bool,
}

/// What the player asked for. Carried out by `net` (or `sandbox`).
#[derive(Message, Clone)]
pub enum Action {
    Join {
        name: String,
        room: String,
    },
    SubmitPrompt(String),
    SetRoomSettings {
        match_secs: u32,
        house_bot: bool,
    },
    /// Put a teleporter on the floor at sim `(x, y)`.
    AddTeleporter {
        x: f32,
        y: f32,
    },
    /// Take the teleporter with this number away.
    RemoveTeleporter(u32),
}

#[derive(Resource, Default)]
struct Form {
    name: String,
    room: String,
    prompt: String,
    /// The prompt we last sent, and the last one that came back as a bot.
    /// Sending that one again keeps the bot (the server skips the agent).
    sent: String,
    built: String,
    /// Whether `name` was already filled in from an earlier visit.
    prefilled: bool,
    /// The text box we last moved the keyboard focus to, so each screen
    /// grabs it once and typing works without clicking first.
    focused: Option<Field>,
    /// Decides which send shortcut the hint shows.
    mac: bool,
    /// The prompt help popup is open.
    help: bool,
    /// The owner is placing teleporters: clicks on the roof add and remove
    /// pads.
    placing: bool,
    touch: bool,
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
        .init_resource::<ArenaUi>()
        .insert_resource(Form {
            mac: is_mac(),
            touch: is_touch(),
            ..default()
        })
        .add_systems(Startup, setup)
        .add_systems(EguiPrimaryContextPass, draw);
    #[cfg(target_arch = "wasm32")]
    if is_touch() {
        app.add_systems(
            PostUpdate,
            on_screen_keyboard.after(bevy_egui::EguiPostUpdateSet::ProcessOutput),
        );
    }
}

#[cfg(target_arch = "wasm32")]
fn on_screen_keyboard(outputs: Query<&bevy_egui::EguiOutput>, mut open: Local<bool>) {
    let editing = outputs.iter().any(|output| {
        output.platform_output.ime.is_some() || output.platform_output.mutable_text_under_cursor
    });
    if editing != *open {
        *open = editing;
        bevy_egui::text_agent::update_text_agent(editing);
    }
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

pub fn is_mac() -> bool {
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

pub fn is_touch() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window().is_some_and(|window| {
            let navigator = window.navigator();
            let agent = navigator.user_agent().unwrap_or_default().to_lowercase();
            ["android", "iphone", "ipad", "ipod", "mobile"]
                .iter()
                .any(|device| agent.contains(device))
                // An iPad says it is a Mac, but a Mac has no touch screen.
                || (agent.contains("mac") && navigator.max_touch_points() > 1)
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
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
    /// The room's teleporter pads, by number.
    pub teleporters: Vec<Teleporter>,
    pub me: Player,
    /// Why the server would not take our last prompt at all (sent too soon
    /// after the one before, the agents are busy, ...).
    pub prompt_refusal: Option<String>,
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

    pub fn submission_of(&self, id: u32) -> Option<&Submission> {
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

/// Reserve typing space when a mobile iframe cannot report keyboard height.
fn clear_of_keyboard(ctx: &egui::Context, touch: bool) -> egui::Rect {
    let window = ctx.viewport_rect();
    let covered =
        touch && ctx.text_edit_focused() && window.width() < 760.0 && window.height() >= 520.0;
    if covered {
        window.with_max_y(window.top() + window.height() * 0.46)
    } else {
        window
    }
}

/// Narrow windows use a stacked layout, including phones rotated mid-round.
pub fn compact_view(width: f32) -> bool {
    width < 760.0
}

fn draw(
    mut contexts: EguiContexts,
    (mut form, mut arena_ui): (ResMut<Form>, ResMut<ArenaUi>),
    screen: Res<Screen>,
    mut actions: MessageWriter<Action>,
    replay: Res<Replay>,
    camera: Single<(&Camera, &GlobalTransform), With<ArenaCamera>>,
    mut fonts_set: Local<bool>,
) -> Result {
    arena_ui.placing = false;
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
    if !theme::fonts_ready(ctx) {
        return Ok(());
    }
    let screen_rect = clear_of_keyboard(ctx, form.touch);
    let compact = compact_view(screen_rect.width());
    let portrait = ctx.viewport_rect().height() > ctx.viewport_rect().width() || form.touch;
    let default_widgets = egui::Visuals::light().widgets;
    ctx.all_styles_mut(|style| {
        for (widget, normal) in [
            (
                &mut style.visuals.widgets.noninteractive,
                &default_widgets.noninteractive,
            ),
            (
                &mut style.visuals.widgets.inactive,
                &default_widgets.inactive,
            ),
            (&mut style.visuals.widgets.hovered, &default_widgets.hovered),
            (&mut style.visuals.widgets.active, &default_widgets.active),
            (&mut style.visuals.widgets.open, &default_widgets.open),
        ] {
            widget.corner_radius = if portrait {
                egui::CornerRadius::same(255)
            } else {
                normal.corner_radius
            };
        }
        style.spacing.interact_size.y = if compact || form.touch { 44.0 } else { 24.0 };
        style.spacing.button_padding = if compact || form.touch {
            egui::vec2(12.0, 10.0)
        } else {
            egui::vec2(4.0, 1.0)
        };
    });
    let mut background = ctx.layer_painter(egui::LayerId::new(
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
            join_screen(ctx, screen_rect, known_name, error, &mut form, &mut actions);
            return Ok(());
        }
        Screen::Room(snapshot) => snapshot,
    };
    let screen = screen_rect;

    // The help popup only belongs to prompting; everything else steps back
    // while it is up.
    form.help &= snapshot.game.phase == Phase::Prompting;
    let help = help_shown(ctx, form.help);
    let dim = 1.0 - help.clamp(0.0, 1.0);
    background.multiply_opacity(dim);

    let (camera, camera_transform) = *camera;
    // Only the owner moves the pads, and only while the next match is still
    // to be set up.
    let game = &snapshot.game;
    form.placing &= game.phase == Phase::Prompting && snapshot.i_own_the_room() && !form.help;
    if form.placing && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        form.placing = false;
    }
    arena_ui.placing = form.placing;
    let view = (camera, camera_transform);
    teleporters(ctx, &background, snapshot, view, form.placing, &mut actions);
    bot_markers(&background, &replay, camera, camera_transform);
    if !compact {
        bot_cards(&background, screen, snapshot, &replay);
    }
    banner(&background, screen, snapshot, &replay);
    let clock = if compact {
        None
    } else {
        match_timer(&background, screen, &snapshot.game, &replay)
    };
    if !compact {
        faults(ctx, snapshot, &replay, clock);
    }
    let typing = compact && screen.height() < 520.0 && ctx.text_edit_focused();
    let roster = if typing {
        egui::Rect::from_min_size(screen.min, egui::Vec2::ZERO)
    } else if compact {
        compact_roster(
            ctx,
            screen,
            snapshot,
            &replay,
            dim,
            &mut form.placing,
            &mut actions,
        )
    } else {
        roster(&background, screen, snapshot, &replay)
    };

    if game.phase == Phase::Prompting && snapshot.i_own_the_room() && !compact {
        room_settings(ctx, snapshot, dim, &mut form.placing, &mut actions);
    }
    let submission = snapshot.submission_of(snapshot.me.id);
    // Whatever the phase: the last bot ready starts the countdown at once.
    if submission.is_some_and(|s| s.status == SubmissionStatus::Ready) && form.built != form.sent {
        form.built = form.sent.clone();
    }
    match (game.phase, submission.map(|s| s.status)) {
        (Phase::Prompting, None | Some(SubmissionStatus::Failed)) => {
            let error = submission.map(|failed| failed.error.as_str());
            let top = roster.bottom() + 12.0;
            if compact && form.placing {
                status_line(
                    ctx,
                    false,
                    "Tap the roof to add a pad; tap a pad to remove it.",
                    0.0,
                );
            } else {
                prompt_box(ctx, screen, top, snapshot, error, &mut form, &mut actions);
            }
        }
        (Phase::Prompting, Some(SubmissionStatus::Generating)) => {
            status_line(ctx, true, "Choosing build…", 0.0);
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
            status_line(ctx, false, &text, 0.0);
        }
        // The big countdown and the fight speak for themselves.
        (Phase::Countdown | Phase::Fighting, _) => {}
        (Phase::Results, _) => status_line(
            ctx,
            false,
            "Next round starts in a moment.",
            if compact { 0.0 } else { BOT_TAB_HEIGHT },
        ),
    }
    if help != 0.0 {
        prompt_help(ctx, screen, help, &mut form);
    }
    Ok(())
}

/// First screen, on every launch: who are you, and which room?
fn join_screen(
    ctx: &egui::Context,
    screen: egui::Rect,
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

    let compact = compact_view(ctx.viewport_rect().width());
    egui::Area::new(egui::Id::new("join_screen"))
        .anchor(
            if compact {
                egui::Align2::CENTER_TOP
            } else {
                egui::Align2::CENTER_CENTER
            },
            if compact {
                egui::vec2(0.0, EDGE)
            } else {
                egui::Vec2::ZERO
            },
        )
        .show(ctx, |ui| {
            box_frame().show(ui, |ui| {
                ui.set_width(380.0_f32.min((ctx.viewport_rect().width() - 4.0 * EDGE).max(100.0)));
                let max_height =
                    (screen.height() - 2.0 * EDGE - box_frame().total_margin().sum().y).max(80.0);
                ui.set_max_height(max_height);
                egui::ScrollArea::vertical()
                    .max_height(max_height)
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("Who's playing, and where?")
                                    .font(theme::bold(theme::SIZE_TITLE))
                                    .color(theme::INK),
                            );
                            ui.add_space(4.0);
                            ui.label(small("Players with the same room ID play together."));
                            ui.add_space(14.0);

                            ui.spacing_mut().item_spacing.y = JOIN_CONTROL_GAP;
                            let name = text_field(ui, &mut form.name, "Your name", MAX_NAME_CHARS);
                            let room = text_field(ui, &mut form.room, "Room ID", MAX_ROOM_CHARS);
                            if form.focused != Some(Field::Join) {
                                form.focused = Some(Field::Join);
                                if !compact && !form.touch {
                                    name.request_focus();
                                }
                            }

                            let play = full_width_button(
                                ui,
                                "Play",
                                !form.name.trim().is_empty() && !form.room.trim().is_empty(),
                                JOIN_CONTROL_HEIGHT,
                            )
                            .clicked();
                            // Why the server turned the last attempt away (room full).
                            if let Some(error) = error {
                                ui.add_space(12.0);
                                theme::error_panel(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(theme::error_heading("Can't join"));
                                    ui.label(theme::error_body(error));
                                });
                            }

                            // A single-line box gives up the focus on Enter. Whichever box
                            // is still empty gets it next; with both filled in, we join.
                            let entered = ui.input(|input| input.key_pressed(egui::Key::Enter));
                            if play || (entered && (name.lost_focus() || room.lost_focus())) {
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
        });
}

/// Wide controls need explicit alignment: egui otherwise inherits the
/// surrounding vertical layout's left/top text alignment.
fn full_width_button(ui: &mut egui::Ui, text: &str, enabled: bool, height: f32) -> egui::Response {
    let size = egui::vec2(ui.available_width(), height);
    ui.allocate_ui_with_layout(
        size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add_enabled(
                enabled,
                egui::Button::new((egui::Atom::grow(), text, egui::Atom::grow())).min_size(size),
            )
        },
    )
    .inner
}

/// One single-line box of the join screen.
fn text_field(ui: &mut egui::Ui, text: &mut String, hint: &str, limit: usize) -> egui::Response {
    let field = clipboard::show(
        ui,
        egui::TextEdit::singleline(text)
            .hint_text(hint.to_owned())
            .char_limit(limit)
            .font(egui::FontId::proportional(theme::SIZE_FIELD))
            .text_color(theme::INK)
            .horizontal_align(egui::Align::Center)
            .vertical_align(egui::Align::Center)
            .min_size(egui::vec2(0.0, JOIN_CONTROL_HEIGHT))
            .desired_width(f32::INFINITY)
            .frame(theme::field_frame()),
    );
    // On the desktop, text pasted from several lines keeps its line breaks.
    if text.contains(['\r', '\n']) {
        *text = text.replace(['\r', '\n'], " ");
    }
    field
}

/// Second screen, every round: the prompt, in a column down the right side
/// from `top` (under the roster). What went wrong last time, if anything,
/// sits at the head of it. Enter adds a line; Command+Enter (or Ctrl+Enter)
/// sends.
fn prompt_box(
    ctx: &egui::Context,
    screen: egui::Rect,
    top: f32,
    snapshot: &Snapshot,
    error: Option<&str>,
    form: &mut Form,
    actions: &mut MessageWriter<Action>,
) {
    let compact = compact_view(screen.width());
    let width = if compact {
        screen.width() - 2.0 * EDGE
    } else {
        (screen.width() * PROMPT_COLUMN_SHARE)
            .clamp(*PROMPT_COLUMN_WIDTH.start(), *PROMPT_COLUMN_WIDTH.end())
    };
    let outer = egui::Rect::from_min_max(
        egui::pos2(screen.right() - EDGE - width, top),
        egui::pos2(
            screen.right() - EDGE,
            if compact && !(ctx.text_edit_focused() && screen.height() < 520.0) {
                (top + screen.height() * 0.48).min(screen.bottom() - EDGE)
            } else {
                screen.bottom() - EDGE
            },
        ),
    );
    let frame = box_frame();
    let inner = outer.size() - frame.total_margin().sum();

    egui::Area::new(egui::Id::new("prompt_box"))
        .fixed_pos(outer.min)
        .show(ctx, |ui| {
            ui.multiply_opacity(1.0 - help_shown(ctx, form.help).clamp(0.0, 1.0));
            if form.help {
                // Nothing here takes typing or clicks while the help is up.
                ui.disable();
            }
            frame.show(ui, |ui| {
                ui.set_min_size(inner);
                ui.set_max_size(inner);

                // Take the shortcut out of the input before the text box sees
                // it. Command is Ctrl outside macOS; Ctrl also works on a Mac.
                let shortcut = !form.help
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
                            | input.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
                    });

                ui.horizontal(|ui| {
                    ui.label(theme::heading("Your prompt"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let help_size = if compact { 44.0 } else { 24.0 };
                        let button = egui::Button::new(
                            egui::RichText::new("?").font(theme::bold(theme::SIZE_BODY)),
                        )
                        .corner_radius(help_size / 2.0)
                        .min_size(egui::Vec2::splat(help_size));
                        if ui
                            .add_sized(egui::Vec2::splat(help_size), button)
                            .on_hover_text("What your bot knows")
                            .clicked()
                        {
                            form.help = true;
                        }
                        ui.label(small(format!("Round {}", snapshot.game.round)));
                    });
                });
                let unchanged = !form.built.is_empty() && form.prompt.trim() == form.built;
                if !compact {
                    ui.label(small(if unchanged {
                        "Send it as it is to keep the same bot, or change it for a new one."
                    } else {
                        "Tell your agent how your bot should fight."
                    }));
                }
                ui.add_space(8.0);
                // A prompt the server would not take at all comes before one
                // whose agent failed.
                let refused = snapshot.prompt_refusal.as_deref();
                let problem = refused
                    .map(|why| ("The server didn't take your prompt", why))
                    .or(error.map(|why| ("Your agent couldn't build a bot", why)))
                    .filter(|(_, why)| !why.is_empty());
                if let Some((heading, why)) = problem {
                    theme::error_panel(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(theme::error_heading(heading));
                        ui.label(theme::error_body(why));
                    });
                    ui.add_space(10.0);
                }

                let footer_height = if compact { 44.0 } else { 28.0 };
                let send_gap = 12.0;
                let text_height =
                    ui.available_height() - footer_height - ui.spacing().item_spacing.y - send_gap;
                let field_box = theme::field_frame().show(ui, |ui| {
                    let text_height = (text_height - 16.0).max(48.0);
                    egui::ScrollArea::vertical()
                        .max_height(text_height)
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let field = clipboard::show(
                                ui,
                                egui::TextEdit::multiline(&mut form.prompt)
                                    .hint_text("Rush the weakest bot, then circle the edge…")
                                    .char_limit(MAX_PROMPT_CHARS)
                                    .font(egui::FontId::proportional(if compact {
                                        16.0
                                    } else {
                                        theme::SIZE_BODY
                                    }))
                                    .text_color(theme::INK)
                                    .desired_width(f32::INFINITY)
                                    .margin(egui::Margin {
                                        bottom: if compact { 48 } else { 24 },
                                        ..egui::Margin::ZERO
                                    })
                                    .min_size(egui::vec2(0.0, text_height))
                                    // The box around it is already drawn.
                                    .frame(egui::Frame::NONE),
                            );
                            let this = Field::Prompt(snapshot.game.id, snapshot.game.round);
                            if form.focused != Some(this) {
                                form.focused = Some(this);
                                if !compact && !form.touch {
                                    field.request_focus();
                                }
                            }
                        });
                });
                if dice_button(ui, field_box.response.rect) {
                    let seed = (ui.input(|input| input.time) * 1000.0) as u32;
                    let mut pick = (crate::fx::rand(seed) * EXAMPLE_PROMPTS.len() as f32) as usize
                        % EXAMPLE_PROMPTS.len();
                    // Never roll the prompt that is already there.
                    if form.prompt == EXAMPLE_PROMPTS[pick] {
                        pick = (pick + 1) % EXAMPLE_PROMPTS.len();
                    }
                    form.prompt = EXAMPLE_PROMPTS[pick].to_string();
                }

                ui.add_space(send_gap);
                let clicked = if compact {
                    full_width_button(
                        ui,
                        "Send prompt",
                        !form.prompt.trim().is_empty(),
                        footer_height,
                    )
                    .clicked()
                } else {
                    let mut clicked = false;
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), footer_height),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let ready = !form.prompt.trim().is_empty();
                            clicked = ui
                                .add_enabled(
                                    ready,
                                    egui::Button::new(
                                        egui::RichText::new("Send")
                                            .font(theme::bold(theme::SIZE_BODY)),
                                    ),
                                )
                                .clicked();
                            ui.label(small(if form.mac {
                                "⌘ Enter to send"
                            } else {
                                "Ctrl + Enter to send"
                            }));
                        },
                    );
                    clicked
                };
                let prompt = form.prompt.trim();
                if (shortcut || clicked) && !prompt.is_empty() {
                    actions.write(Action::SubmitPrompt(prompt.to_string()));
                    form.sent = prompt.to_string();
                    if compact {
                        ui.memory_mut(|memory| {
                            if let Some(id) = memory.focused() {
                                memory.surrender_focus(id);
                            }
                        });
                    }
                }
            });
        });
}

/// What the dice in the prompt box fills it with.
const EXAMPLE_PROMPTS: [&str; 5] = [
    "Walk to the enemy with the lowest hp and punch it whenever your cooldown is ready. \
     When it dies, pick the next lowest.",
    "Hit and run: walk in and punch the nearest enemy, then back straight away from it \
     until your cooldown is ready again.",
    "Stay in a corner and wait. Punch any enemy that comes within range. \
     In the last 10 seconds, chase whoever has the least hp.",
    "Keep away from every enemy until only one is left or 20 seconds have passed, \
     then go after the closest one and punch nonstop.",
    "Punch the nearest enemy while your hp is above 40. Below 40, run to the far side \
     of the arena from the closest enemy and only fight back if it catches you.",
];

/// A round button with a die on it, in the bottom right corner of `field`,
/// styled like the help button. It goes in a child ui so it floats over the
/// text without taking space from it. True when clicked.
fn dice_button(ui: &mut egui::Ui, field: egui::Rect) -> bool {
    let size = if compact_view(ui.ctx().viewport_rect().width()) {
        44.0
    } else {
        24.0
    };
    let rect = egui::Rect::from_min_size(
        field.right_bottom() - egui::Vec2::splat(size + 8.0),
        egui::Vec2::splat(size),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    // A shade darker than the help button, so it shows on the grey field.
    let widgets = &mut child.visuals_mut().widgets;
    for state in [
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
    ] {
        let [r, g, b, a] = state.weak_bg_fill.to_array();
        state.weak_bg_fill = egui::Color32::from_rgba_premultiplied(
            r.saturating_sub(40),
            g.saturating_sub(40),
            b.saturating_sub(40),
            a,
        );
    }
    let response = child
        .add(
            egui::Button::new("")
                .corner_radius(
                    if ui.ctx().viewport_rect().height() > ui.ctx().viewport_rect().width() {
                        255
                    } else {
                        theme::PILL_RADIUS
                    },
                )
                .min_size(egui::Vec2::splat(size)),
        )
        .on_hover_text("Roll a random prompt");
    // The same colour the "?" gets.
    let ink = ui.visuals().widgets.style(&response).text_color();
    let centre = response.rect.center();
    let painter = ui.painter();
    painter.rect_stroke(
        egui::Rect::from_center_size(centre, egui::Vec2::splat(12.0)),
        2.5,
        egui::Stroke::new(1.5, ink),
        egui::StrokeKind::Middle,
    );
    // The five face.
    for (x, y) in [
        (-1.0, -1.0),
        (1.0, -1.0),
        (0.0, 0.0),
        (-1.0, 1.0),
        (1.0, 1.0),
    ] {
        painter.circle_filled(centre + egui::vec2(x, y) * 3.0, 1.2, ink);
    }
    response.clicked()
}

/// The room's settings, top left. Only the owner gets them, and only while
/// prompting: that is when the next match is still to be set up.
fn room_settings(
    ctx: &egui::Context,
    snapshot: &Snapshot,
    dim: f32,
    placing: &mut bool,
    actions: &mut MessageWriter<Action>,
) {
    let game = &snapshot.game;
    egui::Area::new(egui::Id::new("room_settings"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(EDGE, EDGE))
        .interactable(dim == 1.0)
        .show(ctx, |ui| {
            ui.multiply_opacity(dim);
            box_frame().show(ui, |ui| {
                ui.label(theme::heading("Room settings"));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(small("Match length"));
                    for secs in MATCH_LENGTHS {
                        let chosen = game.match_secs == secs;
                        if ui.selectable_label(chosen, format!("{secs}s")).clicked() && !chosen {
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
                ui.horizontal(|ui| {
                    let count = snapshot.teleporters.len();
                    ui.label(small(format!("Teleporters {count}/{}", teleporter::MAX)));
                    let label = if *placing { "Done" } else { "Place" };
                    if ui.selectable_label(*placing, label).clicked() {
                        *placing = !*placing;
                    }
                });
                if *placing {
                    ui.label(small("Click the roof to add a pad."));
                    ui.label(small("Click a pad to take it away."));
                }
            });
        });
}

/// How far the help popup is in, 0 to 1, overshooting a little on the way
/// (the spring). Same value however often it is asked for in a frame.
fn help_shown(ctx: &egui::Context, open: bool) -> f32 {
    ctx.animate_bool_with_time_and_easing(
        egui::Id::new("prompt_help"),
        open,
        0.35,
        egui::emath::easing::back_out,
    )
}

/// What a bot knows during the match and how to write a prompt for it,
/// over a dark overlay. `shown` runs 0 to 1 with a springy overshoot; the
/// card scales and fades with it. A click outside, Esc or "Got it" closes it.
fn prompt_help(ctx: &egui::Context, screen: egui::Rect, shown: f32, form: &mut Form) {
    const WIDTH: f32 = 440.0;
    let alpha = shown.clamp(0.0, 1.0);

    let overlay = egui::Area::new(egui::Id::new("prompt_help_overlay"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter().rect_filled(
                screen,
                0.0,
                egui::Color32::from_black_alpha((110.0 * alpha) as u8),
            );
            ui.allocate_rect(screen, egui::Sense::click())
        });
    if overlay.inner.clicked() || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        form.help = false;
    }

    let card =
        egui::Area::new(egui::Id::new("prompt_help_card"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.multiply_opacity(alpha);
                box_frame().show(ui, |ui| {
                    ui.set_width(WIDTH.min((screen.width() - 4.0 * EDGE).max(100.0)));
                    let max_height = (screen.height() - 100.0).max(100.0);
                    // Areas start with a 400px height budget; let desktop help use
                    // the screen's available space before introducing scrolling.
                    ui.set_max_height(max_height);
                    egui::ScrollArea::vertical()
                    .max_height(max_height)
                    .show(ui, |ui| {
                        ui.label(theme::heading("What your bot knows"));
                        ui.add_space(4.0);
                        ui.label(small(
                            "Your agent turns your prompt into a script. Ten times a second, \
                     the script looks at the arena and picks one move.",
                ));
                ui.add_space(10.0);
                for line in [
                    "Where it is, its hp and stamina, and how long until it can punch again",
                    "Where every living enemy is, their hp, stamina, how far away they are and whether their shield is up",
                    "Where the numbered teleporters are and where each one leads",
                    "The arena's size, the time, and the rules: speed, punch range, damage",
                    "Anything it chose to remember from earlier in the match",
                ] {
                    bullet(ui, line);
                }
                ui.add_space(4.0);
                ui.label(small(
                    "It can walk, dash, punch the nearest enemy in range, hold up a shield, or \
                     wait: one per tick. Dashes, punches and the shield cost stamina, which \
                     comes back on ticks it does none of them. A shield stops every hit but \
                     slows the bot to half speed. A teleporter takes any bot that gets onto \
                     it to the next number, and the last one back to 1. It can't see other prompts or when enemies can punch next.",
                ));

                ui.add_space(14.0);
                ui.label(theme::heading("Writing a good prompt"));
                ui.add_space(4.0);
                for line in [
                    "Choose a build: health, speed, damage, reach, or faster punches",
                    "Give numbers and conditions: \"under 30 hp, back off to a wall\"",
                    "Say who to go after when there's more than one enemy",
                    "At the time limit the most hp wins, so plan for the clock",
                    "A short, clear plan beats a clever, vague one",
                ] {
                    bullet(ui, line);
                }

                        ui.add_space(12.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                            let got_it = egui::Button::new(
                                egui::RichText::new("Got it").font(theme::bold(theme::SIZE_BODY)),
                            );
                            if ui.add(got_it).clicked() {
                                form.help = false;
                            }
                        });
                    });
                });
            });

    // Above the overlay, scaled about its own centre.
    ctx.move_to_top(card.response.layer_id);
    let scale = 0.9 + 0.1 * shown;
    let centre = card.response.rect.center().to_vec2();
    ctx.set_transform_layer(
        card.response.layer_id,
        egui::emath::TSTransform::new(centre * (1.0 - scale), scale),
    );
    // Hand the keyboard back to the prompt once the help goes away.
    if !form.help {
        form.focused = None;
    }
}

/// A list item whose wrapped lines stay clear of the dot.
fn bullet(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.label(theme::body("•"));
        ui.add(egui::Label::new(theme::body(text)).wrap());
    });
}

/// One line at the bottom while there is nothing to type.
fn status_line(ctx: &egui::Context, busy: bool, text: &str, lift: f32) {
    egui::Area::new(egui::Id::new("status_line"))
        .anchor(
            egui::Align2::CENTER_BOTTOM,
            egui::vec2(0.0, -EDGE - 6.0 - lift),
        )
        .interactable(false)
        .show(ctx, |ui| {
            pill(ui, |ui| {
                ui.set_max_width((ctx.viewport_rect().width() - 64.0).max(100.0));
                ui.horizontal_wrapped(|ui| {
                    if busy {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                    ui.label(theme::body(text));
                });
            });
        });
}

/// Everyone in the room and what they are up to, top right. Returns the
/// box it took up, so the prompt column can start below it.
///
/// Painted directly instead of laid out by egui: an anchored `Area` is placed
/// using its size from the previous frame, so it would jump for a frame
/// whenever a state changed length.
fn roster(
    painter: &egui::Painter,
    screen: egui::Rect,
    snapshot: &Snapshot,
    replay: &Replay,
) -> egui::Rect {
    const DOT: f32 = 9.0;
    const GAP: f32 = 6.0;
    const COLUMN_GAP: f32 = 14.0;
    const ROW_GAP: f32 = 6.0;
    /// Extra room under the heading row.
    const HEADER_GAP: f32 = 6.0;
    const PADDING: egui::Vec2 = egui::vec2(16.0, 12.0);

    let game = &snapshot.game;
    let muted = theme::INK_MUTED;
    let left = |text: String| {
        painter.layout_no_wrap(
            text,
            egui::FontId::proportional(theme::SIZE_BODY),
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
        painter.layout_no_wrap(
            format!("Room {}", game.code),
            theme::bold(theme::SIZE_HEADING),
            theme::INK,
        ),
        right(format!("{}s matches", game.match_secs), muted),
    )];
    for player in snapshot.players.iter().filter(|player| player.online) {
        let mut name = player.name.clone();
        if player.id == snapshot.me.id {
            name += " (you)";
        }
        if player.id == game.owner {
            name += "  ·  Owner";
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
        rows.len() as f32 * (row_height + ROW_GAP) - ROW_GAP + HEADER_GAP,
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
        if color.is_none() {
            y += HEADER_GAP;
        }
    }
    outer
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
        (Phase::Prompting, None) => ("Writing a Prompt", muted),
        (Phase::Prompting, Some(SubmissionStatus::Generating)) => ("Choosing build…", muted),
        (Phase::Prompting, Some(SubmissionStatus::Failed)) => ("Agent Failed", bad),
        (Phase::Prompting, Some(SubmissionStatus::Ready)) => ("Ready", good),
        // Joined while a match was already set up: no bot in it.
        (Phase::Countdown | Phase::Fighting | Phase::Results, _) if in_match.is_none() => {
            ("Watching", muted)
        }
        (Phase::Countdown, _) => ("Ready", good),
        (Phase::Results, _) if game.winner == Some(id) => ("Won", good),
        (Phase::Fighting | Phase::Results, _) if down => ("Knocked Out", bad),
        (Phase::Fighting, _) => ("Fighting", theme::INK),
        (Phase::Results, _) => ("Still Standing", muted),
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
            ui.set_max_width(360.0);
            theme::error_panel(ui, |ui| {
                for (i, fault) in faults.into_iter().enumerate() {
                    if i > 0 {
                        ui.add_space(8.0);
                    }
                    let whose = if fault.player_id == snapshot.me.id {
                        "Your bot".to_string()
                    } else {
                        snapshot.name_of(fault.player_id)
                    };
                    ui.label(theme::error_heading(format!("{whose} broke down")));
                    ui.label(theme::error_body(&fault.message));
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
    let font = theme::bold(size);
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

/// The number on every teleporter and, while the owner is placing them,
/// what a click under the cursor would do: a ring where a new pad would
/// land, or around the pad it would take away.
fn teleporters(
    ctx: &egui::Context,
    painter: &egui::Painter,
    snapshot: &Snapshot,
    (camera, camera_transform): (&Camera, &GlobalTransform),
    placing: bool,
    actions: &mut MessageWriter<Action>,
) {
    // The arena camera covers the whole window, so its viewport coordinates
    // are egui coordinates. Sim (x, y) is the ground plane; Bevy's up is Y.
    let on_screen = |x: f32, y: f32| {
        camera
            .world_to_viewport(camera_transform, Vec3::new(x, 0.1, y))
            .ok()
            .map(|at| egui::pos2(at.x, at.y))
    };
    let pads = &snapshot.teleporters;
    for pad in pads {
        if let Some(at) = on_screen(pad.x, pad.y) {
            let number = pad.number.to_string();
            shadowed_text(painter, at, number, theme::SIZE_HEADING, theme::TEXT);
        }
    }

    if !placing || ctx.is_pointer_over_egui() {
        return;
    }
    // A finger is gone from the screen in the very frame its tap counts, so
    // a click is taken where the pointer last was.
    let clicked = ctx.input(|input| input.pointer.primary_clicked());
    let cursor = if clicked {
        ctx.pointer_interact_pos()
    } else {
        ctx.pointer_hover_pos()
    };
    let Some(cursor) = cursor else {
        return;
    };
    let Ok(ray) = camera.viewport_to_world(camera_transform, Vec2::new(cursor.x, cursor.y)) else {
        return;
    };
    let Some(distance) = ray.intersect_plane(Vec3::ZERO, InfinitePlane3d::new(Vec3::Y)) else {
        return;
    };
    let ground = ray.get_point(distance);
    let (x, y) = (ground.x, ground.z);
    let half = snapshot.game.arena_half_size;
    if x.abs() > half || y.abs() > half {
        return; // not over the roof
    }

    let hovered = pads
        .iter()
        .find(|pad| (pad.x - x).hypot(pad.y - y) < teleporter::RADIUS);
    let (centre, hint, action) = match hovered {
        Some(pad) => (
            (pad.x, pad.y),
            Some("Take away"),
            Some(Action::RemoveTeleporter(pad.number)),
        ),
        None => match teleporter::refusal(pads, x, y) {
            Some(reason) => ((x, y), Some(reason), None),
            None => ((x, y), None, Some(Action::AddTeleporter { x, y })),
        },
    };
    let adding = hovered.is_none() && action.is_some();
    let colour = if adding {
        theme::TELEPORTER_RING
    } else {
        theme::BAD
    };

    const CORNERS: usize = 40;
    let ring: Vec<egui::Pos2> = (0..CORNERS)
        .filter_map(|i| {
            let angle = std::f32::consts::TAU * i as f32 / CORNERS as f32;
            let (sin, cos) = angle.sin_cos();
            on_screen(
                centre.0 + teleporter::RADIUS * cos,
                centre.1 + teleporter::RADIUS * sin,
            )
        })
        .collect();
    if ring.len() == CORNERS {
        painter.add(egui::Shape::convex_polygon(
            ring,
            colour.gamma_multiply(0.25),
            egui::Stroke::new(2.5, colour),
        ));
    }
    if let Some(hint) = hint {
        let at = cursor + egui::vec2(0.0, -28.0);
        shadowed_text(painter, at, hint.into(), theme::SIZE_SMALL, theme::TEXT);
    }
    ctx.set_cursor_icon(if action.is_some() {
        egui::CursorIcon::PointingHand
    } else {
        egui::CursorIcon::NotAllowed
    });
    if let Some(action) = action
        && clicked
    {
        actions.write(action);
    }
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
        let above = bot.position + Vec3::Y * (bot.radius * 2.8);
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

/// Individual tabs docked to the bottom edge during combat and results.
/// Health and the thin stamina strip share one rounded wrapper.
fn bot_cards(painter: &egui::Painter, screen: egui::Rect, snapshot: &Snapshot, replay: &Replay) {
    if matches!(snapshot.game.phase, Phase::Prompting | Phase::Countdown) {
        loadout_cards(painter, screen, snapshot);
        return;
    }
    if replay.bots.is_empty() {
        return;
    }
    const GAP: f32 = 12.0;
    const PADDING: f32 = 14.0;
    let count = replay.bots.len();
    let width =
        ((screen.width() - EDGE * 2.0 - GAP * (count - 1) as f32) / count as f32).min(220.0);
    let total = width * count as f32 + GAP * (count - 1) as f32;
    let left = screen.center().x - total / 2.0;
    let corners = egui::CornerRadius {
        nw: theme::BOX_RADIUS,
        ne: theme::BOX_RADIUS,
        sw: 0,
        se: 0,
    };

    for (index, bot) in replay.bots.iter().enumerate() {
        let reveal = if snapshot.game.phase == Phase::Results {
            1.0
        } else {
            bot_tab_reveal(replay.phase_elapsed, index)
        };
        let top = screen.bottom() + 4.0 - (BOT_TAB_HEIGHT + 4.0) * reveal;
        let outer = egui::Rect::from_min_size(
            egui::pos2(left + index as f32 * (width + GAP), top),
            egui::vec2(width, BOT_TAB_HEIGHT + 16.0),
        );
        painter.rect(
            outer,
            corners,
            theme::PANEL_FILL,
            egui::Stroke::new(theme::PANEL_STROKE_WIDTH, theme::PANEL_STROKE),
            egui::StrokeKind::Inside,
        );
        let inner = outer.shrink(PADDING);
        let alive = bot.action != BotAction::Dead;
        let ink = if alive { theme::INK } else { theme::INK_MUTED };
        let color = fill_of(bot);
        let name_y = inner.top() + 8.0;
        painter.circle_filled(egui::pos2(inner.left() + 4.0, name_y), 4.0, color);
        // Wrap nowhere and elide long names so adjacent tabs remain separate.
        let mut job = egui::text::LayoutJob::simple(
            snapshot.name_of(bot.id),
            theme::bold(theme::SIZE_HEADING),
            ink,
            (inner.width() - 20.0).max(1.0),
        );
        job.wrap.max_rows = 1;
        let name = painter.layout_job(job);
        painter.galley(
            egui::pos2(inner.left() + 16.0, name_y - name.size().y / 2.0),
            name,
            ink,
        );
        painter.text(
            inner.left_top() + egui::vec2(0.0, 28.0),
            egui::Align2::LEFT_TOP,
            if alive {
                format!("{} / {} HP", bot.hp.max(0), bot.max_hp)
            } else {
                "Knocked out".into()
            },
            egui::FontId::proportional(theme::SIZE_SMALL),
            theme::INK_MUTED,
        );
        const BAR: f32 = 10.0;
        const STAMINA_BAR: f32 = 2.0;
        let bar = egui::Rect::from_min_size(
            inner.left_top() + egui::vec2(0.0, 49.0),
            egui::vec2(inner.width(), BAR),
        );
        painter.rect_filled(bar, 3.0, theme::HP_TRACK);
        let health = (bot.hp as f32 / bot.max_hp.max(1) as f32).clamp(0.0, 1.0);
        if health > 0.0 {
            let filled = egui::Rect::from_min_size(
                bar.min + egui::vec2(0.0, STAMINA_BAR),
                egui::vec2(bar.width() * health, BAR - STAMINA_BAR),
            );
            // Square depletion edges inside a shared rounded silhouette.
            painter.with_clip_rect(filled).rect_filled(bar, 3.0, color);
        }
        if alive && bot.stamina > 0.0 {
            let filled = egui::Rect::from_min_size(
                bar.min,
                egui::vec2(bar.width() * bot.stamina.clamp(0.0, 1.0), STAMINA_BAR),
            );
            painter
                .with_clip_rect(filled)
                .rect_filled(bar, 3.0, theme::STAMINA);
        }
        let balanced = [2, 2, 2, 2, 2];
        let points = snapshot
            .submission_of(bot.id)
            .and_then(|s| s.build.as_deref());
        if let Some(points) = points
            .filter(|p| p.len() == 5)
            .or_else(|| (bot.id == HOUSE_BOT_ID).then_some(balanced.as_slice()))
        {
            stat_strip(
                painter,
                inner.left_top() + egui::vec2(0.0, 77.0),
                inner.width(),
                points,
                ink,
                3,
            );
        }
    }
}

/// Percentage changes in actual stats from the balanced default build.
fn stat_strip(
    painter: &egui::Painter,
    at: egui::Pos2,
    width: f32,
    points: &[u32],
    color: egui::Color32,
    columns: usize,
) {
    let stroke = egui::Stroke::new(1.1, color);
    const GAP: f32 = 6.0;
    let columns = columns.clamp(1, 5);
    let pill_width = (width - GAP * (columns - 1) as f32) / columns as f32;
    for (i, point) in points.iter().take(5).enumerate() {
        let point = *point as f32;
        let (value, default) = match i {
            0 => (60.0 + 20.0 * point, 100.0),
            1 => (2.2 + 0.4 * point, 3.0),
            2 => (6.0 + 2.0 * point, 10.0),
            3 => (1.2 + 0.15 * point, 1.5),
            // Clock represents cooldown duration: negative is less waiting.
            _ => (7.0 - point, 5.0),
        };
        let percent = ((value / default - 1.0) * 100.0).round() as i32;
        let text = painter.layout_no_wrap(
            format!("{percent:+}%"),
            egui::FontId::proportional(if pill_width < 56.0 {
                11.0
            } else {
                theme::SIZE_SMALL
            }),
            color,
        );
        let row = i / columns;
        let in_row = (points.len().min(5) - row * columns).min(columns);
        let row_width = in_row as f32 * pill_width + GAP * (in_row - 1) as f32;
        let pill = egui::Rect::from_center_size(
            at + egui::vec2(
                (width - row_width) / 2.0
                    + (i % columns) as f32 * (pill_width + GAP)
                    + pill_width / 2.0,
                row as f32 * 30.0,
            ),
            egui::vec2(pill_width, 24.0),
        );
        painter.rect_filled(pill, 12.0, theme::FIELD_FILL);
        // Center the combined icon + gap + visible text, using glyph ink bounds.
        let content_width = 9.0 + 5.0 + text.mesh_bounds.width();
        let c = egui::pos2(pill.center().x - content_width / 2.0 + 4.5, pill.center().y);
        let p = |x, y| c + egui::vec2(x, y) * 0.75;
        match i {
            0 => {
                // Heart: health.
                painter.circle_filled(p(-2.5, -2.0), 2.625, color);
                painter.circle_filled(p(2.5, -2.0), 2.625, color);
                painter.add(egui::Shape::convex_polygon(
                    vec![p(-5.5, -1.0), p(5.5, -1.0), p(0.0, 6.0)],
                    color,
                    egui::Stroke::NONE,
                ));
            }
            1 => {
                // Lightning: movement speed.
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        p(1.0, -6.0),
                        p(-5.0, 1.0),
                        p(0.0, 1.0),
                        p(-1.0, 6.0),
                        p(5.0, -1.0),
                        p(0.0, -1.0),
                    ],
                    color,
                    egui::Stroke::NONE,
                ));
            }
            2 => {
                // Sword: attack power.
                painter.line_segment([p(-4.0, 5.0), p(4.0, -5.0)], stroke);
                painter.line_segment([p(-5.0, 0.0), p(0.0, 5.0)], stroke);
                painter.line_segment([p(1.0, -5.0), p(4.0, -5.0)], stroke);
            }
            3 => {
                // Outward arrows: attack reach.
                painter.line_segment([p(-6.0, 0.0), p(6.0, 0.0)], stroke);
                for sign in [-1.0, 1.0] {
                    painter.line_segment([p(sign * 3.0, -3.0), p(sign * 6.0, 0.0)], stroke);
                    painter.line_segment([p(sign * 3.0, 3.0), p(sign * 6.0, 0.0)], stroke);
                }
            }
            _ => {
                // Clock: attack recovery.
                painter.circle_stroke(c, 4.125, stroke);
                painter.line_segment([p(0.0, -3.5), c], stroke);
                painter.line_segment([c, p(3.0, 1.0)], stroke);
            }
        }
        let text_at = egui::pos2(
            c.x + 9.5 - text.mesh_bounds.left(),
            pill.center().y - text.mesh_bounds.center().y,
        );
        painter.galley(text_at, text, color);
    }
}

/// During prompting and countdown, only the caller's build is visible.
fn loadout_cards(painter: &egui::Painter, screen: egui::Rect, snapshot: &Snapshot) {
    let sub = snapshot.submission_of(snapshot.me.id);
    let ready = sub.filter(|s| {
        s.status == SubmissionStatus::Ready
            && s.build.as_ref().is_some_and(|build| build.len() == 5)
    });
    let top = if snapshot.game.phase == Phase::Prompting && snapshot.i_own_the_room() {
        (screen.center().y - 65.0).max(screen.top() + 220.0)
    } else {
        screen.center().y - 65.0
    };
    let outer = egui::Rect::from_min_size(
        egui::pos2(screen.left() + EDGE, top),
        egui::vec2(BOT_PANEL_WIDTH, 140.0),
    );
    theme::panel(painter, outer);
    let inner = outer.shrink(16.0);
    painter.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        "Your build",
        theme::bold(theme::SIZE_HEADING),
        theme::INK,
    );
    if let Some(sub) = ready {
        painter.text(
            inner.left_top() + egui::vec2(0.0, 30.0),
            egui::Align2::LEFT_TOP,
            format!("{} / {} HP", sub.max_hp, sub.max_hp),
            egui::FontId::proportional(theme::SIZE_SMALL),
            theme::INK,
        );
        let [r, g, b] = bot_color(snapshot.me.id);
        let color = egui::Color32::from_rgb(r, g, b);
        painter.rect_filled(
            egui::Rect::from_min_size(
                inner.left_top() + egui::vec2(0.0, 51.0),
                egui::vec2(inner.width(), 6.0),
            ),
            3.0,
            color,
        );
        stat_strip(
            painter,
            inner.left_top() + egui::vec2(0.0, 81.0),
            inner.width(),
            sub.build.as_deref().unwrap_or_default(),
            theme::INK,
            5,
        );
    } else {
        let text = match sub.map(|s| s.status) {
            Some(SubmissionStatus::Generating) => "Choosing build…",
            Some(SubmissionStatus::Failed) => "Build failed · try another prompt",
            _ => "Waiting for your prompt",
        };
        painter.text(
            inner.left_top() + egui::vec2(0.0, 40.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::proportional(theme::SIZE_SMALL),
            theme::INK_MUTED,
        );
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
    let size = if compact_view(screen.width()) {
        (screen.width() / (text.chars().count().max(4) as f32 * 0.65)).min(38.0)
    } else {
        theme::SIZE_BANNER
    };
    let at = if compact_view(screen.width()) {
        screen.center_top() + egui::vec2(0.0, 145.0)
    } else {
        at
    };
    shadowed_text(painter, at, text, size, theme::TEXT);
}

/// A single row leaves room for writing and watching on a phone.
fn compact_roster(
    ctx: &egui::Context,
    screen: egui::Rect,
    snapshot: &Snapshot,
    replay: &Replay,
    dim: f32,
    placing: &mut bool,
    actions: &mut MessageWriter<Action>,
) -> egui::Rect {
    egui::Area::new(egui::Id::new("compact_room"))
        .fixed_pos(screen.min + egui::Vec2::splat(EDGE))
        .show(ctx, |ui| {
            ui.multiply_opacity(dim);
            if dim < 1.0 {
                ui.disable();
            }
            box_frame().show(ui, |ui| {
                ui.set_width(
                    (screen.width() - 2.0 * EDGE - box_frame().total_margin().sum().x).max(100.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("Players / room", |ui| {
                        ui.set_max_width((screen.width() - 64.0).max(100.0));
                        for player in snapshot.players.iter().filter(|p| p.online) {
                            let (state, _) = bot_state(
                                &snapshot.game,
                                replay,
                                player.id,
                                snapshot.submission_of(player.id).map(|s| s.status),
                            );
                            if let Some(bot) = replay.bots.iter().find(|bot| bot.id == player.id) {
                                ui.label(format!(
                                    "{} · {} HP · {:.0}% stamina · {state}",
                                    player.name,
                                    bot.hp.max(0),
                                    bot.stamina.clamp(0.0, 1.0) * 100.0
                                ));
                            } else {
                                ui.label(format!("{} · {state}", player.name));
                            }
                        }
                        let house_present = if snapshot.game.phase == Phase::Prompting {
                            snapshot.game.house_bot
                                && snapshot.players.iter().filter(|p| p.online).count()
                                    < MAX_PLAYERS
                        } else {
                            replay.bots.iter().any(|bot| bot.id == HOUSE_BOT_ID)
                        };
                        if house_present {
                            if let Some(bot) = replay.bots.iter().find(|bot| bot.id == HOUSE_BOT_ID)
                            {
                                let (state, _) = bot_state(
                                    &snapshot.game,
                                    replay,
                                    HOUSE_BOT_ID,
                                    Some(SubmissionStatus::Ready),
                                );
                                ui.label(format!(
                                    "House bot · {} HP · {:.0}% stamina · {state}",
                                    bot.hp.max(0),
                                    bot.stamina.clamp(0.0, 1.0) * 100.0
                                ));
                            } else {
                                ui.label("House bot · Ready");
                            }
                        }
                        let shown_tick =
                            (replay.phase_elapsed * snapshot.game.ticks_per_sec as f32) as u32;
                        for fault in snapshot.faults.iter().filter(|fault| {
                            snapshot.game.phase == Phase::Results
                                || (snapshot.game.phase == Phase::Fighting
                                    && fault.tick <= shown_tick)
                        }) {
                            ui.label(theme::error_body(format!(
                                "{}: {}",
                                snapshot.name_of(fault.player_id),
                                fault.message
                            )));
                        }
                        if snapshot.i_own_the_room() && snapshot.game.phase == Phase::Prompting {
                            ui.separator();
                            ui.label("Match length");
                            ui.horizontal_wrapped(|ui| {
                                for secs in MATCH_LENGTHS {
                                    if ui
                                        .selectable_label(
                                            snapshot.game.match_secs == secs,
                                            format!("{secs}s"),
                                        )
                                        .clicked()
                                    {
                                        actions.write(Action::SetRoomSettings {
                                            match_secs: secs,
                                            house_bot: snapshot.game.house_bot,
                                        });
                                    }
                                }
                            });
                            let mut house_bot = snapshot.game.house_bot;
                            if ui.checkbox(&mut house_bot, "House bot").changed() {
                                actions.write(Action::SetRoomSettings {
                                    match_secs: snapshot.game.match_secs,
                                    house_bot,
                                });
                            }
                            ui.separator();
                            ui.label(small(format!(
                                "Teleporters {}/{}",
                                snapshot.teleporters.len(),
                                teleporter::MAX
                            )));
                            if ui
                                .selectable_label(
                                    *placing,
                                    if *placing {
                                        "Done placing"
                                    } else {
                                        "Place teleporters"
                                    },
                                )
                                .clicked()
                            {
                                *placing = !*placing;
                                ui.close();
                            }
                        }
                    });
                    if snapshot.game.phase == Phase::Fighting {
                        let elapsed = replay.phase_elapsed.min(
                            snapshot.game.match_ticks as f32
                                / snapshot.game.ticks_per_sec.max(1) as f32,
                        );
                        ui.label(format!(
                            "{}s",
                            (snapshot.game.match_secs as f32 - elapsed).ceil().max(0.0) as u32
                        ));
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(theme::heading("Room"));
                        ui.add(
                            egui::Label::new(
                                theme::heading(&snapshot.game.code).color(theme::INK_MUTED),
                            )
                            .truncate(),
                        );
                    });
                });
            });
        })
        .response
        .rect
}
