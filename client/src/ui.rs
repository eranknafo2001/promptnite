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
//! A narrow window (a phone held upright, see `COMPACT_WIDTH`) stacks them
//! instead: the players across the top while a prompt is due, the prompt in
//! a sheet along the bottom, the bots' health in a strip there once it is
//! sent.
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
/// Match lengths the owner can pick from, in seconds. The server accepts
/// anything within its own limits (`sim/src/rules.rs`).
const MATCH_LENGTHS: [u32; 4] = [30, 60, 90, 120];

/// Share of the window width taken by the prompt column on the right, and
/// its limits. The camera frames the arena in what is left (see
/// `arena::STRATEGY_VIEW`).
const PROMPT_COLUMN_SHARE: f32 = 0.34;
const PROMPT_COLUMN_WIDTH: std::ops::RangeInclusive<f32> = 360.0..=540.0;

/// Below this width the interface switches to its stacked layout.
pub const COMPACT_WIDTH: f32 = 640.0;
/// A compact window this low has the on-screen keyboard open: the prompt
/// takes all of it.
const KEYBOARD_OPEN_HEIGHT: f32 = 520.0;
/// How much of a phone's screen, from the top, an on-screen keyboard leaves
/// free. On the low side, so nothing ends up behind a tall keyboard.
const KEYBOARD_FREE_SHARE: f32 = 0.46;
/// How tall a button is on a touch screen.
const TOUCH_BUTTON: f32 = 44.0;
/// How tall the "Your build" card is.
const LOADOUT_HEIGHT: f32 = 140.0;

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
    /// A phone or tablet: buttons instead of keyboard shortcuts, and no
    /// focus on a text box before the player taps it (that would bring the
    /// on-screen keyboard up over the game every round).
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

/// Brings the on-screen keyboard up the moment a text box takes the focus
/// and puts it away when none has it. bevy_egui only decides when a finger
/// lifts, from the frame before the tap, so a first tap on a box did nothing.
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

/// Whether the game runs on a phone or tablet. Only the browser build does.
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
    if !theme::fonts_ready(ctx) {
        return Ok(());
    }
    let screen_rect = clear_of_keyboard(ctx, form.touch);
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

    let game = &snapshot.game;
    let compact = screen.width() < COMPACT_WIDTH;
    let prompting = game.phase == Phase::Prompting;
    let submission = snapshot.submission_of(snapshot.me.id);
    let writing = prompting
        && submission.is_none_or(|submission| submission.status == SubmissionStatus::Failed);
    // With the keyboard up on a phone there is only room for the prompt.
    let typing = compact && writing && screen.height() < KEYBOARD_OPEN_HEIGHT;

    let (camera, camera_transform) = *camera;
    // Only the owner moves the pads, and only while the next match is still
    // to be set up.
    form.placing &= prompting && snapshot.i_own_the_room() && !form.help;
    if form.placing && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        form.placing = false;
    }
    let view = (camera, camera_transform);
    teleporters(ctx, &background, snapshot, view, form.placing, &mut actions);
    bot_markers(&background, &replay, camera, camera_transform);
    let banner = banner(&background, screen, snapshot, &replay);
    let clock = match_timer(&background, screen, game, &replay);
    // Compact: the result is as wide as the screen, so faults go under it.
    let above_faults = if compact { banner.or(clock) } else { clock };
    faults(ctx, screen, snapshot, &replay, above_faults);
    // Compact: the players only while a prompt is due. The bots' health
    // strip says who is who during the match.
    let roster = (!typing && (!compact || prompting))
        .then(|| roster(&background, screen, snapshot, &replay, compact));
    let mut top = roster.map_or(screen.top(), |roster| roster.bottom()) + EDGE;

    if prompting && snapshot.i_own_the_room() && !typing {
        let below = compact.then_some(top);
        let settings = room_settings(
            ctx,
            screen,
            below,
            snapshot,
            dim,
            &mut form.placing,
            &mut actions,
        );
        if compact {
            top = settings.bottom() + EDGE;
        }
    }
    // Whatever the phase: the last bot ready starts the countdown at once.
    if submission.is_some_and(|s| s.status == SubmissionStatus::Ready) && form.built != form.sent {
        form.built = form.sent.clone();
    }
    let status = match (game.phase, submission.map(|s| s.status)) {
        (Phase::Prompting, None | Some(SubmissionStatus::Failed)) => {
            let error = submission.map(|failed| failed.error.as_str());
            let outer = if typing {
                screen.shrink(EDGE)
            } else if compact {
                // A sheet along the bottom, under whatever is stacked above.
                let height = (screen.height() * 0.38).clamp(190.0, 340.0);
                let top = top.max(screen.bottom() - EDGE - height);
                egui::Rect::from_min_max(
                    egui::pos2(screen.left() + EDGE, top),
                    screen.max - egui::Vec2::splat(EDGE),
                )
            } else {
                let width = (screen.width() * PROMPT_COLUMN_SHARE)
                    .clamp(*PROMPT_COLUMN_WIDTH.start(), *PROMPT_COLUMN_WIDTH.end());
                egui::Rect::from_min_max(
                    egui::pos2(screen.right() - EDGE - width, top),
                    screen.max - egui::Vec2::splat(EDGE),
                )
            };
            prompt_box(ctx, outer, snapshot, error, &mut form, &mut actions);
            None
        }
        (Phase::Prompting, Some(SubmissionStatus::Generating)) => {
            Some((true, "Choosing build…".to_string()))
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
            Some((false, text))
        }
        // The big countdown and the fight speak for themselves.
        (Phase::Countdown | Phase::Fighting, _) => None,
        (Phase::Results, _) => Some((false, "Next round starts in a moment.".to_string())),
    };
    let status = status.map(|(busy, text)| status_line(ctx, screen, busy, &text));
    if compact {
        // The prompt sheet covers where the strip goes.
        if !writing {
            let bottom = status.map_or(screen.bottom(), |status| status.top()) - EDGE;
            if matches!(game.phase, Phase::Prompting | Phase::Countdown) {
                let outer = egui::Rect::from_min_max(
                    egui::pos2(screen.left() + EDGE, bottom - LOADOUT_HEIGHT),
                    egui::pos2(screen.right() - EDGE, bottom),
                );
                loadout_card(&background, outer, snapshot);
            } else {
                bot_strip(&background, screen, bottom, snapshot, &replay);
            }
        }
    } else {
        bot_cards(&background, screen, snapshot, &replay);
    }
    if help != 0.0 {
        prompt_help(ctx, screen, help, &mut form);
    }
    Ok(())
}

/// The part of the window the on-screen keyboard leaves free. Opened as its
/// own page, the window gets smaller when the keyboard comes up. Inside
/// another page (itch.io) it does not, and the game cannot tell how big the
/// keyboard is: so while a text box is being edited in a phone-sized window
/// that is still its full height, count the lower part as covered.
fn clear_of_keyboard(ctx: &egui::Context, touch: bool) -> egui::Rect {
    let window = ctx.viewport_rect();
    let covered = touch
        && ctx.text_edit_focused()
        && window.width() < COMPACT_WIDTH
        && window.height() >= KEYBOARD_OPEN_HEIGHT;
    if covered {
        window.with_max_y(window.top() + window.height() * KEYBOARD_FREE_SHARE)
    } else {
        window
    }
}

/// First screen, on every launch: who are you, and which room? Centred in
/// `screen`.
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

    egui::Area::new(egui::Id::new("join_screen"))
        .anchor(
            egui::Align2::CENTER_CENTER,
            screen.center() - ctx.viewport_rect().center(),
        )
        .show(ctx, |ui| {
            box_frame().show(ui, |ui| {
                let room_for_it = screen.width()
                    - 2.0 * (EDGE + f32::from(theme::BOX_MARGIN) + theme::PANEL_STROKE_WIDTH);
                ui.set_width(room_for_it.min(380.0));
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Who's playing, and where?")
                            .font(theme::bold(theme::SIZE_TITLE))
                            .color(theme::INK),
                    );
                    ui.add_space(4.0);
                    ui.label(small("Players with the same room id play together."));
                    ui.add_space(14.0);

                    let name = text_field(ui, &mut form.name, "Your name", MAX_NAME_CHARS);
                    ui.add_space(4.0);
                    let room = text_field(ui, &mut form.room, "Room id", MAX_ROOM_CHARS);
                    if form.focused != Some(Field::Join) && !form.touch {
                        form.focused = Some(Field::Join);
                        name.request_focus();
                    }

                    ui.add_space(10.0);
                    let mut tapped = false;
                    if form.touch {
                        // Tapped too early, it sends the player to the empty box.
                        let size = egui::vec2(ui.available_width(), TOUCH_BUTTON);
                        tapped = ui
                            .add_sized(size, theme::button("Play", theme::SIZE_FIELD, true))
                            .clicked();
                    } else {
                        ui.label(small("Press Enter to play"));
                    }
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
                    if tapped || entered && (name.lost_focus() || room.lost_focus()) {
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
    let field = clipboard::show(
        ui,
        egui::TextEdit::singleline(text)
            .hint_text(hint.to_owned())
            .char_limit(limit)
            .font(egui::FontId::proportional(theme::SIZE_FIELD))
            .text_color(theme::INK)
            .horizontal_align(egui::Align::Center)
            .desired_width(f32::INFINITY)
            .frame(theme::field_frame()),
    );
    // On the desktop, text pasted from several lines keeps its line breaks.
    if text.contains(['\r', '\n']) {
        *text = text.replace(['\r', '\n'], " ");
    }
    field
}

/// Second screen, every round: the prompt, filling `outer` (a column down
/// the right side, under the roster; a sheet along the bottom when compact).
/// What went wrong last time, if anything, sits at the head of it. Enter adds a line; Command+Enter (or Ctrl+Enter)
/// sends.
fn prompt_box(
    ctx: &egui::Context,
    outer: egui::Rect,
    snapshot: &Snapshot,
    error: Option<&str>,
    form: &mut Form,
    actions: &mut MessageWriter<Action>,
) {
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
                        let button = egui::Button::new(
                            egui::RichText::new("?").font(theme::bold(theme::SIZE_BODY)),
                        )
                        .corner_radius(theme::PILL_RADIUS)
                        .min_size(egui::Vec2::splat(if form.touch { 36.0 } else { 24.0 }));
                        if ui
                            .add(button)
                            .on_hover_text("What your bot knows")
                            .clicked()
                        {
                            form.help = true;
                        }
                        ui.label(small(format!("Round {}", snapshot.game.round)));
                    });
                });
                // Dropped when space is short (a phone with its keyboard up).
                if inner.y >= 240.0 {
                    let unchanged = !form.built.is_empty() && form.prompt.trim() == form.built;
                    ui.label(small(if unchanged {
                        "Send it as it is to keep the same bot, or change it for a new one."
                    } else {
                        "Tell your agent how your bot should fight."
                    }));
                }
                ui.add_space(8.0);
                let footer_height = if form.touch { TOUCH_BUTTON } else { 28.0 };
                // A little air between the text and the button under it.
                let footer_gap = if form.touch { 6.0 } else { 0.0 };
                // A prompt the server would not take at all comes before one
                // whose agent failed.
                let refused = snapshot.prompt_refusal.as_deref();
                let problem = refused
                    .map(|why| ("The server didn't take your prompt", why))
                    .or(error.map(|why| ("Your agent couldn't build a bot", why)))
                    .filter(|(_, why)| !why.is_empty());
                if let Some((heading, why)) = problem {
                    // A long one scrolls, so the prompt and Send keep their room:
                    // a few lines of prompt, and the panel's own heading.
                    let most = (ui.available_height() - footer_height - footer_gap - 150.0)
                        .max(theme::SIZE_BODY * 1.5);
                    theme::error_panel(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(theme::error_heading(heading));
                        egui::ScrollArea::vertical()
                            .id_salt("prompt_problem")
                            .max_height(most)
                            .min_scrolled_height(0.0)
                            .show(ui, |ui| ui.label(theme::error_body(why)));
                    });
                    ui.add_space(10.0);
                }

                let text_height = ui.available_height()
                    - footer_height
                    - footer_gap
                    - ui.spacing().item_spacing.y;
                let field_box = theme::field_frame().show(ui, |ui| {
                    let text_height = text_height - 16.0;
                    egui::ScrollArea::vertical()
                        .max_height(text_height)
                        .min_scrolled_height(0.0)
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let field = clipboard::show(
                                ui,
                                egui::TextEdit::multiline(&mut form.prompt)
                                    .hint_text("Rush the weakest bot, then circle the edge…")
                                    .char_limit(MAX_PROMPT_CHARS)
                                    .font(egui::FontId::proportional(theme::SIZE_BODY))
                                    .text_color(theme::INK)
                                    .desired_width(f32::INFINITY)
                                    .min_size(egui::vec2(0.0, text_height))
                                    // The box around it is already drawn.
                                    .frame(egui::Frame::NONE),
                            );
                            let this = Field::Prompt(snapshot.game.id, snapshot.game.round);
                            if form.focused != Some(this) && !form.touch {
                                form.focused = Some(this);
                                field.request_focus();
                            }
                        });
                });
                if dice_button(ui, field_box.response.rect, form.touch) {
                    let seed = (ui.input(|input| input.time) * 1000.0) as u32;
                    let mut pick = (crate::fx::rand(seed) * EXAMPLE_PROMPTS.len() as f32) as usize
                        % EXAMPLE_PROMPTS.len();
                    // Never roll the prompt that is already there.
                    if form.prompt == EXAMPLE_PROMPTS[pick] {
                        pick = (pick + 1) % EXAMPLE_PROMPTS.len();
                    }
                    form.prompt = EXAMPLE_PROMPTS[pick].to_string();
                }

                let mut clicked = false;
                ui.add_space(footer_gap);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), footer_height),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let ready = !form.prompt.trim().is_empty();
                        if form.touch {
                            let size = egui::vec2(96.0, TOUCH_BUTTON);
                            clicked = ui
                                .add_sized(size, theme::button("Send", theme::SIZE_BODY, ready))
                                .clicked();
                            return;
                        }
                        clicked = ui
                            .add_enabled(
                                ready,
                                egui::Button::new(
                                    egui::RichText::new("Send").font(theme::bold(theme::SIZE_BODY)),
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

                let prompt = form.prompt.trim();
                if (shortcut || clicked) && !prompt.is_empty() {
                    actions.write(Action::SubmitPrompt(prompt.to_string()));
                    form.sent = prompt.to_string();
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
fn dice_button(ui: &mut egui::Ui, field: egui::Rect, touch: bool) -> bool {
    // Finger-sized on a phone, unless the keyboard has squeezed the field.
    let size = if touch && field.height() >= 90.0 {
        36.0
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
                .corner_radius(theme::PILL_RADIUS)
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
/// prompting: that is when the next match is still to be set up. With
/// `below` they span the screen from that height instead (compact). Returns
/// the box they took up.
fn room_settings(
    ctx: &egui::Context,
    screen: egui::Rect,
    below: Option<f32>,
    snapshot: &Snapshot,
    dim: f32,
    placing: &mut bool,
    actions: &mut MessageWriter<Action>,
) -> egui::Rect {
    let game = &snapshot.game;
    let frame = box_frame();
    egui::Area::new(egui::Id::new("room_settings"))
        .fixed_pos(egui::pos2(
            screen.left() + EDGE,
            below.unwrap_or(screen.top() + EDGE),
        ))
        .interactable(dim == 1.0)
        .show(ctx, |ui| {
            ui.multiply_opacity(dim);
            frame.show(ui, |ui| {
                if below.is_some() {
                    ui.set_width(screen.width() - 2.0 * EDGE - frame.total_margin().sum().x);
                    // Room for a fingertip.
                    ui.spacing_mut().button_padding = egui::vec2(8.0, 7.0);
                    ui.spacing_mut().interact_size.y = 34.0;
                    ui.spacing_mut().icon_width = 20.0;
                }
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
        })
        .response
        .rect
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

    let card = egui::Area::new(egui::Id::new("prompt_help_card"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.multiply_opacity(alpha);
            box_frame().show(ui, |ui| {
                ui.set_width(WIDTH.min(screen.width() - 4.0 * EDGE));
                // On a short screen the text scrolls and the button stays put.
                egui::ScrollArea::vertical()
                    .max_height((screen.height() - 150.0).max(120.0))
                    .show(ui, help_text);

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clicked = if form.touch {
                        let size = egui::vec2(96.0, TOUCH_BUTTON);
                        ui.add_sized(size, theme::button("Got it", theme::SIZE_BODY, true))
                            .clicked()
                    } else {
                        ui.button(egui::RichText::new("Got it").font(theme::bold(theme::SIZE_BODY)))
                            .clicked()
                    };
                    if clicked {
                        form.help = false;
                    }
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

fn help_text(ui: &mut egui::Ui) {
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
         it to the next number, and the last one back to 1. It can't see other \
         prompts or when enemies can punch next.",
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
}

/// A list item whose wrapped lines stay clear of the dot.
fn bullet(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.label(theme::body("•"));
        ui.add(egui::Label::new(theme::body(text)).wrap());
    });
}

/// One line at the bottom while there is nothing to type (more than one on
/// a narrow screen). Returns the box it took up.
fn status_line(ctx: &egui::Context, screen: egui::Rect, busy: bool, text: &str) -> egui::Rect {
    egui::Area::new(egui::Id::new("status_line"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -EDGE - 6.0))
        .interactable(false)
        .show(ctx, |ui| {
            // Leave the pill's own margins and the spinner out of the width.
            ui.set_max_width(screen.width() - 2.0 * EDGE - 60.0);
            pill(ui, |ui| {
                ui.horizontal(|ui| {
                    if busy {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                    ui.label(theme::body(text));
                });
            });
        })
        .response
        .rect
}

/// Everyone in the room and what they are up to, top right (across the top
/// when `compact`). Returns the box it took up, so the prompt column can
/// start below it.
///
/// Painted directly instead of laid out by egui: an anchored `Area` is placed
/// using its size from the previous frame, so it would jump for a frame
/// whenever a state changed length.
fn roster(
    painter: &egui::Painter,
    screen: egui::Rect,
    snapshot: &Snapshot,
    replay: &Replay,
    compact: bool,
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
    let mut name_width = widest(|row| {
        // The room's row also spans the dot's column.
        row.1.size().x - if row.0.is_none() { DOT + GAP } else { 0.0 }
    });
    let state_width = widest(|row| row.2.size().x);
    if compact {
        // As wide as the screen, whatever the names: long ones are cut off.
        name_width =
            screen.width() - 2.0 * (EDGE + PADDING.x) - DOT - GAP - COLUMN_GAP - state_width;
    }
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
        let name_column = egui::Rect::from_x_y_ranges(x..=state_at.x - COLUMN_GAP, outer.y_range());
        painter
            .with_clip_rect(name_column)
            .galley(name_at, name, theme::INK);
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
fn faults(
    ctx: &egui::Context,
    screen: egui::Rect,
    snapshot: &Snapshot,
    replay: &Replay,
    clock: Option<egui::Rect>,
) {
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
    let top = clock.map_or(EDGE, |clock| clock.bottom() - screen.top() + 6.0);
    egui::Area::new(egui::Id::new("faults"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(EDGE, top))
        .interactable(false)
        .show(ctx, |ui| {
            // The panel's own margins come on top of this.
            ui.set_max_width((screen.width() - 2.0 * EDGE - 40.0).min(360.0));
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
    let Some(cursor) = ctx.pointer_hover_pos() else {
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
        && ctx.input(|input| input.pointer.primary_clicked())
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

/// One panel down the left side, vertically centred, with a row per bot in
/// the match: colour dot, name and health, the health bar underneath and the
/// stamina bar under that.
///
/// Painted directly, like the roster, so the panel doesn't jump a frame when
/// a name changes length.
fn bot_cards(painter: &egui::Painter, screen: egui::Rect, snapshot: &Snapshot, replay: &Replay) {
    if matches!(snapshot.game.phase, Phase::Prompting | Phase::Countdown) {
        let outer = egui::Rect::from_min_size(
            egui::pos2(screen.left() + EDGE, screen.center().y - 65.0),
            egui::vec2(350.0, LOADOUT_HEIGHT),
        );
        loadout_card(painter, outer, snapshot);
        return;
    }
    const WIDTH: f32 = 350.0;
    const PADDING: egui::Vec2 = egui::vec2(16.0, 14.0);
    const HEADING: f32 = 30.0;
    const ROW: f32 = 86.0;
    /// The last row ends at its bar, not at the next row.
    const LAST_ROW: f32 = 74.0;
    const DOT: f32 = 10.0;
    const BAR: f32 = 6.0;
    /// The stamina bar, right under the health bar and thinner.
    const STAMINA_BAR: f32 = 4.0;

    if replay.bots.is_empty() {
        return;
    }
    let height = PADDING.y * 2.0 + HEADING + ROW * (replay.bots.len() - 1) as f32 + LAST_ROW;
    let outer = egui::Rect::from_min_size(
        egui::pos2(screen.left() + EDGE, screen.center().y - height / 2.0),
        egui::vec2(WIDTH, height),
    );
    theme::panel(painter, outer);
    let inner = outer.shrink2(PADDING);
    painter.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        "Bots",
        theme::bold(theme::SIZE_HEADING),
        theme::INK,
    );

    for (i, bot) in replay.bots.iter().enumerate() {
        let top = inner.top() + HEADING + i as f32 * ROW;
        let alive = bot.action != BotAction::Dead;
        let ink = if alive { theme::INK } else { theme::INK_MUTED };
        let name_y = top + 10.0;
        painter.circle_filled(
            egui::pos2(inner.left() + DOT / 2.0, name_y),
            DOT / 2.0,
            fill_of(bot),
        );
        painter.text(
            egui::pos2(inner.left() + DOT + 8.0, name_y),
            egui::Align2::LEFT_CENTER,
            snapshot.name_of(bot.id),
            egui::FontId::proportional(theme::SIZE_BODY),
            ink,
        );
        painter.text(
            egui::pos2(inner.right(), name_y),
            egui::Align2::RIGHT_CENTER,
            if alive {
                format!("{} / {} HP", bot.hp.max(0), bot.max_hp)
            } else {
                "Out".into()
            },
            egui::FontId::proportional(theme::SIZE_SMALL),
            theme::INK_MUTED,
        );

        let bar = egui::Rect::from_min_size(
            egui::pos2(inner.left(), top + 22.0),
            egui::vec2(inner.width(), BAR),
        );
        painter.rect_filled(bar, BAR / 2.0, theme::HP_TRACK);
        let health = (bot.hp as f32 / bot.max_hp.max(1) as f32).clamp(0.0, 1.0);
        if health > 0.0 {
            let filled = egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * health, BAR));
            painter.rect_filled(filled, BAR / 2.0, fill_of(bot));
        }
        let bar = egui::Rect::from_min_size(
            egui::pos2(inner.left(), top + 32.0),
            egui::vec2(inner.width(), STAMINA_BAR),
        );
        painter.rect_filled(bar, STAMINA_BAR / 2.0, theme::HP_TRACK);
        if alive && bot.stamina > 0.0 {
            let filled = egui::Rect::from_min_size(
                bar.min,
                egui::vec2(bar.width() * bot.stamina, STAMINA_BAR),
            );
            painter.rect_filled(filled, STAMINA_BAR / 2.0, theme::STAMINA);
        }
        let points = snapshot
            .submission_of(bot.id)
            .and_then(|s| s.build.as_deref());
        let balanced = [2, 2, 2, 2, 2];
        if let Some(points) = points
            .filter(|p| p.len() == 5)
            .or_else(|| (bot.id == HOUSE_BOT_ID).then_some(balanced.as_slice()))
        {
            stat_strip(
                painter,
                egui::pos2(inner.left(), top + 58.0),
                inner.width(),
                points,
                ink,
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
) {
    let stroke = egui::Stroke::new(1.1, color);
    const GAP: f32 = 6.0;
    let pill_width = (width - GAP * 4.0) / 5.0;
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
            egui::FontId::proportional(theme::SIZE_SMALL),
            color,
        );
        let pill = egui::Rect::from_center_size(
            at + egui::vec2(i as f32 * (pill_width + GAP) + pill_width / 2.0, 0.0),
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

/// During prompting and countdown, only the caller's build is visible. Drawn
/// in `outer`, [`LOADOUT_HEIGHT`] tall.
fn loadout_card(painter: &egui::Painter, outer: egui::Rect, snapshot: &Snapshot) {
    let sub = snapshot.submission_of(snapshot.me.id);
    let ready = sub.filter(|s| {
        s.status == SubmissionStatus::Ready
            && s.build.as_ref().is_some_and(|build| build.len() == 5)
    });
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

/// The same on a narrow screen: a strip across the bottom, ending at
/// `bottom`, the bots two to a line.
fn bot_strip(
    painter: &egui::Painter,
    screen: egui::Rect,
    bottom: f32,
    snapshot: &Snapshot,
    replay: &Replay,
) {
    const PADDING: egui::Vec2 = egui::vec2(16.0, 12.0);
    const ROW: f32 = 38.0;
    const LAST_ROW: f32 = 28.0;
    const COLUMN_GAP: f32 = 20.0;

    if replay.bots.is_empty() {
        return;
    }
    let lines = replay.bots.len().div_ceil(2);
    let height = PADDING.y * 2.0 + ROW * (lines - 1) as f32 + LAST_ROW;
    let outer = egui::Rect::from_min_max(
        egui::pos2(screen.left() + EDGE, bottom - height),
        egui::pos2(screen.right() - EDGE, bottom),
    );
    theme::panel(painter, outer);
    let inner = outer.shrink2(PADDING);
    let width = (inner.width() - COLUMN_GAP) / 2.0;
    for (i, bot) in replay.bots.iter().enumerate() {
        let at = egui::pos2(
            inner.left() + (i % 2) as f32 * (width + COLUMN_GAP),
            inner.top() + (i / 2) as f32 * ROW,
        );
        bot_row(painter, at, width, snapshot, bot);
    }
}

/// One bot of the strip, `width` wide from `at`: colour dot,
/// name (cut off if too long) and health, the health bar underneath.
fn bot_row(
    painter: &egui::Painter,
    at: egui::Pos2,
    width: f32,
    snapshot: &Snapshot,
    bot: &BotView,
) {
    const DOT: f32 = 10.0;
    const BAR: f32 = 6.0;

    let right = at.x + width;
    let alive = bot.action != BotAction::Dead;
    let ink = if alive { theme::INK } else { theme::INK_MUTED };
    let name_y = at.y + 10.0;
    painter.circle_filled(
        egui::pos2(at.x + DOT / 2.0, name_y),
        DOT / 2.0,
        fill_of(bot),
    );
    let health = painter.text(
        egui::pos2(right, name_y),
        egui::Align2::RIGHT_CENTER,
        if alive {
            format!("{} / {} HP", bot.hp.max(0), bot.max_hp)
        } else {
            "Out".into()
        },
        egui::FontId::proportional(theme::SIZE_SMALL),
        theme::INK_MUTED,
    );
    let name_column =
        egui::Rect::from_x_y_ranges(at.x..=health.left() - 6.0, name_y - 12.0..=name_y + 12.0);
    painter.with_clip_rect(name_column).text(
        egui::pos2(at.x + DOT + 8.0, name_y),
        egui::Align2::LEFT_CENTER,
        snapshot.name_of(bot.id),
        egui::FontId::proportional(theme::SIZE_BODY),
        ink,
    );

    let bar = egui::Rect::from_min_size(egui::pos2(at.x, at.y + 22.0), egui::vec2(width, BAR));
    painter.rect_filled(bar, BAR / 2.0, theme::HP_TRACK);
    let left = (bot.hp as f32 / bot.max_hp.max(1) as f32).clamp(0.0, 1.0);
    if left > 0.0 {
        let filled = egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * left, BAR));
        painter.rect_filled(filled, BAR / 2.0, fill_of(bot));
    }
}

/// The big text over the arena: countdown and result, shrunk to fit a
/// narrow screen. Returns the box it took up.
fn banner(
    painter: &egui::Painter,
    screen: egui::Rect,
    snapshot: &Snapshot,
    replay: &Replay,
) -> Option<egui::Rect> {
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
        Phase::Fighting | Phase::Prompting => return None,
    };
    let full = painter
        .layout_no_wrap(text.clone(), theme::bold(theme::SIZE_BANNER), theme::TEXT)
        .size();
    let fit = ((screen.width() - 2.0 * EDGE) / full.x).min(1.0);
    let size = theme::SIZE_BANNER * fit;
    // Top centre, clear of the arena.
    let at = screen.center_top() + egui::vec2(0.0, EDGE + size / 2.0);
    shadowed_text(painter, at, text, size, theme::TEXT);
    Some(egui::Rect::from_center_size(at, full * fit))
}
