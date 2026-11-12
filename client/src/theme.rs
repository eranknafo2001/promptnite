//! The look of the game in one place: colours, type sizes and the boxes the
//! interface is built from. Restyling starts here; `make sandbox` shows every
//! screen without a server (see `sandbox.rs`).

use bevy::color::Color;
use bevy_egui::egui::{self, Color32};

// The arena.

pub const BACKGROUND: Color = Color::srgb(0.07, 0.08, 0.10);
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

pub const TEXT: Color32 = Color32::WHITE;
/// Secondary text: hints, states, footers.
pub const TEXT_MUTED: Color32 = Color32::from_rgba_premultiplied(150, 150, 150, 150);
/// The explanation under the join form.
pub const TEXT_HINT: Color32 = Color32::from_rgba_premultiplied(170, 170, 170, 170);
/// A knocked-out bot's name.
pub const TEXT_DEAD: Color32 = Color32::GRAY;
pub const GOOD: Color32 = Color32::LIGHT_GREEN;
pub const BAD: Color32 = Color32::LIGHT_RED;

/// Text boxes: dark, 80% opaque so the arena shows through, thin outline.
pub const BOX_FILL: Color32 = Color32::from_rgba_unmultiplied_const(16, 18, 23, 204);
pub const BOX_STROKE: Color32 = Color32::from_rgba_premultiplied(45, 45, 45, 45);
pub const BOX_RADIUS: u8 = 10;
pub const BOX_MARGIN: i8 = 10;

/// The darker backdrop behind single lines and the roster.
pub const PILL_FILL: Color32 = Color32::from_black_alpha(150);
pub const PILL_RADIUS: u8 = 8;

pub const TEXT_SHADOW: Color32 = Color32::from_black_alpha(200);
pub const HP_TRACK: Color32 = Color32::from_black_alpha(160);

/// `assets/icon.svg`: an orange rounded square with a white "P".
pub const ICON_ORANGE: Color32 = Color32::from_rgb(0xd9, 0x77, 0x06);

/// Space between the window edge and anything placed along it.
pub const EDGE: f32 = 12.0;

// Type sizes, largest first.

pub const SIZE_BANNER: f32 = 64.0;
pub const SIZE_TITLE: f32 = 24.0;
/// The match timer, in monospace.
pub const SIZE_TIMER: f32 = 20.0;
pub const SIZE_FIELD: f32 = 18.0;
pub const SIZE_BODY: f32 = 16.0;
pub const SIZE_STATUS: f32 = 14.0;
pub const SIZE_BOT_NAME: f32 = 14.0;
pub const SIZE_LABEL: f32 = 13.0;
pub const SIZE_SMALL: f32 = 12.5;

/// Lexend Bold for all proportional text, egui's own fonts behind it for any
/// glyph Lexend lacks. Compiled in, so the browser needs no extra download.
pub fn set_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "lexend_bold".into(),
        egui::FontData::from_static(include_bytes!("../fonts/Lexend-Bold.ttf")).into(),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "lexend_bold".into());
    ctx.set_fonts(fonts);
}

pub fn box_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BOX_FILL)
        .stroke(egui::Stroke::new(1.0, BOX_STROKE))
        .corner_radius(BOX_RADIUS)
        .inner_margin(BOX_MARGIN)
}

/// A dark rounded backdrop that keeps a line of text readable on the arena.
pub fn pill(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PILL_FILL)
        .corner_radius(PILL_RADIUS)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, content);
}

pub fn small(text: &str) -> egui::RichText {
    egui::RichText::new(text).size(SIZE_SMALL).color(TEXT_MUTED)
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
