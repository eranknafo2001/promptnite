//! The look of the game in one place: colours, type sizes and the boxes the
//! interface is built from. Restyling starts here; `make sandbox` shows every
//! screen without a server (see `sandbox.rs`).

use bevy::{
    color::Color,
    render::view::{ColorGrading, ColorGradingGlobal, ColorGradingSection},
};
use bevy_egui::egui::{self, Color32};

// The arena.

/// The sky at the bottom of the screen.
/// Both sky colours share the rooftop floor's teal-blue hue, so the scene
/// reads as one palette.
pub const BACKGROUND: Color = Color::srgb(0.60, 0.86, 0.95);
/// The sky at the top of the screen.
pub const SKY_TOP: Color = Color::srgb(0.0, 0.30, 0.52);
/// The building under the rooftop: walls, and the ledge at each floor.
pub const BUILDING: Color = Color::srgb(0.52, 0.54, 0.57);
pub const BUILDING_LEDGE: Color = Color::srgb(0.66, 0.67, 0.69);
/// The other buildings around the arena; their roof clutter uses
/// `BUILDING_LEDGE`.
/// Street level, a park's grass and its trees.
pub const STREET: Color = Color::srgb(0.36, 0.38, 0.41);
pub const PARK_GRASS: Color = Color::srgb(0.42, 0.66, 0.32);
pub const TREE_LEAVES: Color = Color::srgb(0.27, 0.55, 0.25);
pub const TREE_TRUNK: Color = Color::srgb(0.42, 0.30, 0.20);
/// Cool, light greys so the city sits back behind the arena.
pub const NEIGHBOURS: [Color; 4] = [
    Color::srgb(0.60, 0.65, 0.70),
    Color::srgb(0.68, 0.72, 0.76),
    Color::srgb(0.54, 0.60, 0.66),
    Color::srgb(0.72, 0.72, 0.72),
];
/// Sunlight: a warm white, and how strong it is in lux.
pub const SUN: Color = Color::srgb(1.0, 0.94, 0.82);
pub const SUN_LUX: f32 = 4_500.0;
/// Light from the sky that fills the shadows with a cool blue.
pub const SKY_LIGHT: Color = Color::srgb(0.65, 0.8, 1.0);
pub const SKY_LIGHT_BRIGHTNESS: f32 = 250.0;
/// The final grade on the arena camera: a touch warmer, more saturated and
/// a little more contrast, for a bright, sunny look.
pub fn color_grading() -> ColorGrading {
    let section = |saturation, contrast| ColorGradingSection {
        saturation,
        contrast,
        ..Default::default()
    };
    ColorGrading {
        global: ColorGradingGlobal {
            temperature: 0.04,
            post_saturation: 1.15,
            ..Default::default()
        },
        shadows: section(1.1, 1.0),
        midtones: section(1.05, 1.1),
        highlights: section(1.0, 1.0),
    }
}

pub const BOT_DEAD: Color = Color::srgb(0.25, 0.25, 0.27);

/// Colours are assigned by bot id, so a player keeps theirs between rounds.
/// The spheres, health bars and roster dots all use them.
pub const PALETTE: [[u8; 3]; 8] = [
    [239, 83, 80],
    [66, 165, 245],
    [102, 187, 106],
    [255, 202, 40],
    [171, 71, 188],
    [255, 112, 67],
    [38, 198, 218],
    [236, 64, 122],
];

pub fn bot_color(id: u32) -> [u8; 3] {
    PALETTE[id as usize % PALETTE.len()]
}

// The interface.

/// Text straight on the arena (banner, join screen title), with a shadow.
pub const TEXT: Color32 = Color32::WHITE;
/// Text on a panel: a warm black, and a warm grey for hints, states, footers.
pub const INK: Color32 = Color32::from_rgb(0x2a, 0x22, 0x1c);
pub const INK_MUTED: Color32 = Color32::from_rgb(0x8c, 0x82, 0x78);
/// A knocked-out bot's name.
pub const TEXT_DEAD: Color32 = Color32::GRAY;
/// Dark enough to read on a white panel.
pub const GOOD: Color32 = Color32::from_rgb(0x1f, 0x8a, 0x3d);
pub const BAD: Color32 = Color32::from_rgb(0xcf, 0x3a, 0x2a);
/// Error panels: a soft brick red, so it reads as a problem without shouting.
pub const ERROR_FILL: Color32 = Color32::from_rgb(0xd2, 0x5c, 0x4d);
pub const ERROR_TEXT: Color32 = Color32::WHITE;

/// Panels (text boxes, pills, cards): white, with a bold warm-black outline
/// and well rounded corners.
pub const PANEL_FILL: Color32 = Color32::WHITE;
pub const PANEL_STROKE: Color32 = INK;
pub const PANEL_STROKE_WIDTH: f32 = 3.0;
pub const BOX_RADIUS: u8 = 18;
pub const BOX_MARGIN: i8 = 18;
/// A crisp drop shadow under every panel: straight down, no blur.
pub const PANEL_SHADOW: egui::Shadow = egui::Shadow {
    offset: [0, 4],
    blur: 0,
    spread: 0,
    color: Color32::from_rgba_premultiplied(0x0b, 0x09, 0x07, 0x48),
};
/// The smaller panels: single lines, the roster, the timer, the bot cards.
pub const PILL_RADIUS: u8 = 14;

pub const TEXT_SHADOW: Color32 = Color32::from_black_alpha(200);
pub const HP_TRACK: Color32 = Color32::from_rgb(0xe8, 0xe2, 0xdc);

/// Space between the window edge and anything placed along it.
pub const EDGE: f32 = 12.0;

// Type sizes, largest first.

pub const SIZE_BANNER: f32 = 64.0;
/// The join screen's question.
pub const SIZE_TITLE: f32 = 26.0;
/// The match timer, in monospace.
pub const SIZE_TIMER: f32 = 20.0;
/// What the player types.
pub const SIZE_FIELD: f32 = 17.0;
/// The first line of a panel or an error.
pub const SIZE_HEADING: f32 = 15.0;
/// Names, messages, the status line.
pub const SIZE_BODY: f32 = 14.0;
/// States, hints, footers.
pub const SIZE_SMALL: f32 = 12.5;

/// Lexend for all proportional text: Regular by default, Bold as the
/// `bold` family for titles and headings (see [`bold`]). egui's own fonts
/// sit behind it for any glyph Lexend lacks. Compiled in, so the browser
/// needs no extra download.
pub fn set_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        ("lexend", &include_bytes!("../fonts/Lexend-Regular.ttf")[..]),
        (
            "lexend_bold",
            &include_bytes!("../fonts/Lexend-Bold.ttf")[..],
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), egui::FontData::from_static(bytes).into());
    }
    let fallback = fonts.families[&egui::FontFamily::Proportional].clone();
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "lexend".into());
    let mut bold = vec!["lexend_bold".to_string()];
    bold.extend(fallback);
    fonts
        .families
        .insert(egui::FontFamily::Name(BOLD.into()), bold);
    ctx.set_fonts(fonts);
}

const BOLD: &str = "bold";

/// egui takes new fonts from the next frame on, and panics on a family it
/// doesn't know yet: nothing using [`bold`] may draw before this is true.
pub fn fonts_ready(ctx: &egui::Context) -> bool {
    ctx.fonts(|fonts| {
        fonts
            .definitions()
            .families
            .contains_key(&egui::FontFamily::Name(BOLD.into()))
    })
}

/// Lexend Bold at `size`.
pub fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(BOLD.into()))
}

/// The first line of a panel.
pub fn heading(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .font(bold(SIZE_HEADING))
        .color(INK)
}

/// Ordinary text on a panel.
pub fn body(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).size(SIZE_BODY).color(INK)
}

pub fn box_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL_FILL)
        .stroke(panel_stroke())
        .shadow(PANEL_SHADOW)
        .corner_radius(BOX_RADIUS)
        .inner_margin(BOX_MARGIN)
}

fn panel_stroke() -> egui::Stroke {
    egui::Stroke::new(PANEL_STROKE_WIDTH, PANEL_STROKE)
}

/// A panel painted straight onto `painter`, for the parts of the interface
/// laid out by hand.
pub fn panel(painter: &egui::Painter, rect: egui::Rect) {
    painter.add(PANEL_SHADOW.as_shape(rect, PILL_RADIUS));
    painter.rect(
        rect,
        PILL_RADIUS,
        PANEL_FILL,
        panel_stroke(),
        egui::StrokeKind::Inside,
    );
}

/// A text box inside a panel: outlined like one, without its shadow.
pub fn field_frame() -> egui::Frame {
    box_frame()
        .shadow(egui::Shadow::NONE)
        .corner_radius(PILL_RADIUS)
        .inner_margin(egui::Margin::symmetric(12, 8))
}

/// A panel for something that went wrong: soft red, white text. Fill it
/// with [`error_heading`] and [`error_body`].
pub fn error_panel(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(ERROR_FILL)
        .stroke(panel_stroke())
        .shadow(PANEL_SHADOW)
        .corner_radius(PILL_RADIUS)
        .inner_margin(egui::Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            content(ui);
        });
}

pub fn error_heading(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .font(bold(SIZE_HEADING))
        .color(ERROR_TEXT)
}

pub fn error_body(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .size(SIZE_SMALL)
        .color(ERROR_TEXT)
}

/// A small panel around a line of text.
pub fn pill(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PANEL_FILL)
        .stroke(panel_stroke())
        .shadow(PANEL_SHADOW)
        .corner_radius(PILL_RADIUS)
        .inner_margin(egui::Margin::symmetric(16, 10))
        .show(ui, content);
}

pub fn small(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .size(SIZE_SMALL)
        .color(INK_MUTED)
}

/// egui's own style editor (widget colours, spacing, rounding), live. What
/// looks right there still has to be copied into code.
pub fn style_editor(ctx: &egui::Context, open: &mut bool) {
    egui::Window::new("Style")
        .open(open)
        .vscroll(true)
        .default_width(360.0)
        .show(ctx, |ui| ctx.style_ui(ui, egui::Theme::Dark));
}

// Effects (`fx.rs`): a knocked-out bot blows up.

/// Explosion puffs, hottest first: core, flame, outer flame, smoke.
pub const FX_PUFF: [Color; 4] = [
    Color::srgb(1.0, 0.98, 0.9),
    Color::srgb(1.0, 0.85, 0.2),
    Color::srgb(1.0, 0.45, 0.1),
    Color::srgb(0.3, 0.3, 0.32),
];
/// Camera shake: largest offset in metres and roll in radians at full
/// trauma, and how long full trauma takes to die down.
pub const FX_SHAKE_OFFSET: f32 = 0.45;
pub const FX_SHAKE_ROLL: f32 = 0.035;
pub const FX_SHAKE_SECS: f32 = 0.4;
/// The camera's constant idle sway: how far it drifts in metres and how
/// much it tilts in radians.
pub const CAMERA_SWAY: f32 = 0.25;
pub const CAMERA_SWAY_TILT: f32 = 0.006;
