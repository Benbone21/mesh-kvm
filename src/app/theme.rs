//! The look of the window: colour tokens, sizes, and the egui style built from them.

use eframe::egui::{
    self, style::HandleShape, Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle,
    Vec2,
};

pub const DEEP: Color32 = Color32::from_rgb(0x15, 0x24, 0x2a);
pub const PANEL: Color32 = Color32::from_rgb(0x20, 0x35, 0x3c);
pub const RAISED: Color32 = Color32::from_rgb(0x2b, 0x46, 0x4e);
pub const EDGE: Color32 = Color32::from_rgb(0x41, 0x63, 0x6c);
pub const INK: Color32 = Color32::from_rgb(0xee, 0xe9, 0xda);
pub const DIM: Color32 = Color32::from_rgb(0xa5, 0xbc, 0xbc);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xb2, 0x2e);
pub const GREEN: Color32 = Color32::from_rgb(0x63, 0xe0, 0x8e);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x7a, 0x66);
pub const DEBIAN: Color32 = Color32::from_rgb(0x3f, 0x8c, 0x8c);
/// The grooves in the slide switch's knob.
pub const AMBER_GROOVE: Color32 = Color32::from_rgb(0xe7, 0x9b, 0x1c);
/// A status light that is off.
pub const UNLIT: Color32 = Color32::from_rgb(0x0e, 0x18, 0x1c);

pub const RADIUS: u8 = 6;
pub const SMALL_RADIUS: u8 = 4;
pub const CARD_PADDING: i8 = 16;
pub const CARD_GAP: f32 = 12.0;
pub const ROW_HEIGHT: f32 = 32.0;
pub const BUTTON_HEIGHT: f32 = 38.0;
pub const TITLE_SIZE: f32 = 19.0;
pub const HEADING_SIZE: f32 = 17.0;
pub const SMALL_SIZE: f32 = 13.0;

pub fn line(width: f32, colour: Color32) -> Stroke {
    Stroke { width, color: colour }
}

/// `a` moved towards `b` by `amount`, from 0 to 1.
pub fn mix(a: Color32, b: Color32, amount: f32) -> Color32 {
    let channel = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * amount).round() as u8;
    Color32::from_rgb(channel(a.r(), b.r()), channel(a.g(), b.g()), channel(a.b(), b.b()))
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, visuals());
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(SMALL_SIZE, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(HEADING_SIZE, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.interact_size.y = 24.0;
        style.spacing.combo_height = 320.0;
        style.spacing.menu_margin = Margin::same(6);
        style.animation_time = 0.12;
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.override_text_color = None;
    v.panel_fill = DEEP;
    v.window_fill = PANEL;
    v.window_stroke = line(1.0, EDGE);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.extreme_bg_color = DEEP;
    v.faint_bg_color = RAISED;
    v.code_bg_color = DEEP;
    v.warn_fg_color = AMBER;
    v.error_fg_color = RED;
    v.hyperlink_color = AMBER;
    v.selection.bg_fill = AMBER;
    v.selection.stroke = line(1.0, DEEP);
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let radius = CornerRadius::same(SMALL_RADIUS);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = line(1.0, EDGE);
    w.noninteractive.fg_stroke = line(1.0, INK);
    w.noninteractive.corner_radius = radius;
    w.inactive.bg_fill = RAISED;
    w.inactive.weak_bg_fill = RAISED;
    w.inactive.bg_stroke = line(1.0, EDGE);
    w.inactive.fg_stroke = line(1.0, INK);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = RAISED;
    w.hovered.weak_bg_fill = RAISED;
    w.hovered.bg_stroke = line(1.0, INK);
    w.hovered.fg_stroke = line(1.0, INK);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = EDGE;
    w.active.weak_bg_fill = EDGE;
    w.active.bg_stroke = line(1.0, AMBER);
    w.active.fg_stroke = line(1.0, INK);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;
    w.open = w.active;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_goes_from_one_colour_to_the_other() {
        assert_eq!(mix(DEEP, AMBER, 0.0), DEEP);
        assert_eq!(mix(DEEP, AMBER, 1.0), AMBER);
        assert_eq!(mix(Color32::BLACK, Color32::WHITE, 0.5), Color32::from_rgb(128, 128, 128));
    }
}
