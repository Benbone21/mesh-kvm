//! The look of the window: colour tokens, sizes, fonts, and the egui style built from them.

use std::sync::Arc;

use eframe::egui::{
    self,
    text::{LayoutJob, TextFormat},
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle,
    Vec2,
};

pub const BG: Color32 = Color32::from_rgb(0x0f, 0x11, 0x13);
pub const SURFACE: Color32 = Color32::from_rgb(0x17, 0x1a, 0x1d);
pub const RAISED: Color32 = Color32::from_rgb(0x1d, 0x21, 0x25);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x34);
pub const LINE_FAINT: Color32 = Color32::from_rgb(0x23, 0x27, 0x2b);
pub const LINE_STRONG: Color32 = Color32::from_rgb(0x6b, 0x74, 0x7c);
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xea);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0xa3, 0xab);
pub const ACCENT: Color32 = Color32::from_rgb(0xc6, 0xf1, 0x35);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0f, 0x11, 0x13);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0xb8, 0x6b);
pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x8a, 0x7a);

/// The colours text and squares are drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Text,
    Dim,
    Accent,
    Warn,
    Danger,
}

pub fn tone(t: Tone) -> Color32 {
    match t {
        Tone::Text => TEXT,
        Tone::Dim => TEXT_DIM,
        Tone::Accent => ACCENT,
        Tone::Warn => WARN,
        Tone::Danger => DANGER,
    }
}

pub const SPACE_1: f32 = 4.0;
pub const SPACE_2: f32 = 8.0;
pub const SPACE_3: f32 = 12.0;
pub const SPACE_4: f32 = 16.0;
pub const SPACE_5: f32 = 20.0;
pub const SPACE_6: f32 = 24.0;

pub const ROW_MIN_HEIGHT: f32 = 26.0;
pub const STATUS_BUTTON_HEIGHT: f32 = 36.0;
pub const DIALOG_BUTTON_HEIGHT: f32 = 40.0;
pub const DIALOG_BUTTON_MIN_WIDTH: f32 = 96.0;
pub const STATUS_SQUARE: f32 = 10.0;
pub const CROSSING_EDGE: f32 = 4.0;
pub const COUNTDOWN_HEIGHT: f32 = 4.0;
pub const SETTINGS_WIDTH: f32 = 700.0;
pub const DEVICE_COLUMN: f32 = 130.0;
pub const DIALOG_WIDTH: f32 = 460.0;
pub const SPARKLINE_SIZE: [f32; 2] = [210.0, 36.0];
pub const SPARKLINE_STROKE: f32 = 1.5;

/// The family names of the heavier weights, which egui picks by name.
pub const INTER_SEMIBOLD: &str = "Inter SemiBold";
pub const MONO_MEDIUM: &str = "JetBrains Mono Medium";

/// What each device's screens are tinted with on the canvas, muted into `RAISED` by `tile_fill`.
pub const SCREEN_FIRST: Color32 = Color32::from_rgb(0xb9, 0xea, 0xdd);
pub const SCREEN_SECOND: Color32 = Color32::from_rgb(0xc8, 0xd2, 0xff);

pub fn line(width: f32, colour: Color32) -> Stroke {
    Stroke { width, color: colour }
}

/// `a` moved towards `b` by `amount`, from 0 to 1.
pub fn mix(a: Color32, b: Color32, amount: f32) -> Color32 {
    let channel = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * amount).round() as u8;
    Color32::from_rgb(channel(a.r(), b.r()), channel(a.g(), b.g()), channel(a.b(), b.b()))
}

/// The colour of `device`'s lit screens.
pub fn screen_colour(first_device: bool) -> Color32 {
    if first_device {
        SCREEN_FIRST
    } else {
        SCREEN_SECOND
    }
}

/// The WCAG contrast ratio between two colours, which the tests hold the tokens to.
#[cfg(test)]
fn contrast(a: Color32, b: Color32) -> f32 {
    let luminance = |c: Color32| {
        let lin = |x: u8| {
            let x = x as f32 / 255.0;
            if x <= 0.03928 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// A filled control under the pointer: 8% towards white.
pub fn hovered(fill: Color32) -> Color32 {
    mix(fill, Color32::WHITE, 0.08)
}

/// A filled control being pressed: 8% towards black.
pub fn pressed(fill: Color32) -> Color32 {
    mix(fill, Color32::BLACK, 0.08)
}

/// `base` with `amount` of `border` in it, for a faint fill in a border's colour.
pub fn tint(base: Color32, border: Color32, amount: f32) -> Color32 {
    mix(base, border, amount)
}

/// `colour` at `opacity`, from 0 to 1, over whatever is behind it.
pub fn faded(colour: Color32, opacity: f32) -> Color32 {
    colour.gamma_multiply(opacity)
}

/// A screen tile's fill: `RAISED` with a muted tint of its device's colour.
pub fn tile_fill(first_device: bool) -> Color32 {
    mix(RAISED, screen_colour(first_device), 0.12)
}

/// Inter and JetBrains Mono, with egui's own fonts behind them for the few symbols they lack.
pub fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let files: [(&str, &'static [u8]); 4] = [
        ("Inter", include_bytes!("../../third_party/fonts/Inter-Regular.ttf")),
        (INTER_SEMIBOLD, include_bytes!("../../third_party/fonts/Inter-SemiBold.ttf")),
        ("JetBrains Mono", include_bytes!("../../third_party/fonts/JetBrainsMono-Regular.ttf")),
        (MONO_MEDIUM, include_bytes!("../../third_party/fonts/JetBrainsMono-Medium.ttf")),
    ];
    for (name, bytes) in files {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    let proportional = fonts.families[&FontFamily::Proportional].clone();
    let monospace = fonts.families[&FontFamily::Monospace].clone();
    let with_behind = |first: &str, behind: &[String]| {
        let mut names = vec![first.to_owned()];
        names.extend(behind.iter().cloned());
        names
    };
    fonts.families.insert(FontFamily::Proportional, with_behind("Inter", &proportional));
    fonts.families.insert(FontFamily::Monospace, with_behind("JetBrains Mono", &monospace));
    fonts.families.insert(FontFamily::Name(INTER_SEMIBOLD.into()), with_behind(INTER_SEMIBOLD, &proportional));
    fonts.families.insert(FontFamily::Name(MONO_MEDIUM.into()), with_behind(MONO_MEDIUM, &monospace));
    fonts
}

/// The type scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Body,
    Status,
    StatusStrong,
    Caption,
    Label,
    Head,
    /// The time left and who is being waited for, under a pairing code's countdown.
    Countdown,
    Value,
    Stat,
    CodeShow,
    CodeEnter,
}

impl Type {
    pub fn font(self) -> FontId {
        let inter = |size| FontId::new(size, FontFamily::Proportional);
        let mono = |size| FontId::new(size, FontFamily::Monospace);
        let mono_medium = |size| FontId::new(size, FontFamily::Name(MONO_MEDIUM.into()));
        match self {
            Type::Body => inter(14.0),
            Type::Status => inter(15.0),
            Type::StatusStrong => FontId::new(15.0, FontFamily::Name(INTER_SEMIBOLD.into())),
            Type::Caption => inter(13.0),
            Type::Label => mono(11.0),
            Type::Head => mono_medium(12.0),
            Type::Countdown => mono(12.0),
            Type::Value => mono(13.0),
            Type::Stat => mono_medium(26.0),
            Type::CodeShow => mono_medium(54.0),
            Type::CodeEnter => mono_medium(44.0),
        }
    }

    /// Extra space after each character.
    pub fn letter_spacing(self) -> f32 {
        match self {
            Type::Label => 1.0,
            Type::CodeShow | Type::CodeEnter => 4.0,
            _ => 0.0,
        }
    }

    /// `text` laid out in this style. Labels are upper-cased.
    pub fn job(self, text: &str, colour: Color32) -> LayoutJob {
        let font_id = self.font();
        let line_height = match self {
            Type::Body | Type::Status | Type::StatusStrong | Type::Caption => Some(font_id.size * 1.4),
            _ => None,
        };
        let text = if self == Type::Label { text.to_uppercase() } else { text.to_owned() };
        let format = TextFormat {
            font_id,
            color: colour,
            extra_letter_spacing: self.letter_spacing(),
            line_height,
            ..Default::default()
        };
        LayoutJob::single_section(text, format)
    }
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_visuals_of(egui::Theme::Dark, visuals());
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, Type::Caption.font()),
            (TextStyle::Body, Type::Body.font()),
            (TextStyle::Button, Type::Body.font()),
            (TextStyle::Heading, Type::Status.font()),
            (TextStyle::Monospace, Type::Value.font()),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(SPACE_2, SPACE_2);
        style.spacing.button_padding = Vec2::new(SPACE_3, SPACE_2);
        style.spacing.interact_size.y = ROW_MIN_HEIGHT;
        style.spacing.combo_height = 320.0;
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        style.spacing.menu_margin = Margin::same(SPACE_2 as i8);
        style.animation_time = 0.12;
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.override_text_color = None;
    v.panel_fill = BG;
    v.window_fill = RAISED;
    v.window_stroke = line(1.0, LINE_STRONG);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.extreme_bg_color = BG;
    v.faint_bg_color = SURFACE;
    v.code_bg_color = BG;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = tint(BG, ACCENT, 0.3);
    v.selection.stroke = line(2.0, ACCENT);
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let none = CornerRadius::ZERO;
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = line(1.0, LINE);
    w.noninteractive.fg_stroke = line(1.0, TEXT);
    w.inactive.bg_fill = RAISED;
    w.inactive.weak_bg_fill = RAISED;
    w.inactive.bg_stroke = line(1.0, LINE_STRONG);
    w.inactive.fg_stroke = line(1.0, TEXT);
    w.hovered.bg_fill = hovered(RAISED);
    w.hovered.weak_bg_fill = hovered(RAISED);
    w.hovered.bg_stroke = line(1.0, TEXT);
    w.hovered.fg_stroke = line(1.0, TEXT);
    w.active.bg_fill = pressed(RAISED);
    w.active.weak_bg_fill = pressed(RAISED);
    w.active.bg_stroke = line(1.0, TEXT);
    w.active.fg_stroke = line(1.0, TEXT);
    w.open = w.active;
    for state in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.corner_radius = none;
        state.expansion = 0.0;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_goes_from_one_colour_to_the_other() {
        assert_eq!(mix(BG, ACCENT, 0.0), BG);
        assert_eq!(mix(BG, ACCENT, 1.0), ACCENT);
        assert_eq!(mix(Color32::BLACK, Color32::WHITE, 0.5), Color32::from_rgb(128, 128, 128));
    }

    #[test]
    fn the_tokens_meet_the_contrast_the_spec_promises() {
        let near = |got: f32, want: f32| assert!((got - want).abs() < 0.05, "{got} vs {want}");
        near(contrast(TEXT, BG), 15.4);
        near(contrast(TEXT_DIM, SURFACE), 6.82);
        near(contrast(ACCENT, BG), 14.45);
        near(contrast(ON_ACCENT, ACCENT), 14.45);
        near(contrast(WARN, RAISED), 9.5);
        near(contrast(DANGER, RAISED), 7.07);
        assert!(contrast(LINE_STRONG, BG) >= 3.0);
    }

    #[test]
    fn each_devices_tiles_have_their_own_muted_tint() {
        assert_ne!(tile_fill(true), tile_fill(false));
        assert_ne!(tile_fill(true), RAISED);
        for first in [true, false] {
            assert!(contrast(TEXT, tile_fill(first)) >= 7.0);
            assert!(contrast(TEXT_DIM, tile_fill(first)) >= 4.5);
        }
    }

    #[test]
    fn both_fonts_are_bundled_with_their_weights() {
        let fonts = fonts();
        for name in ["Inter", INTER_SEMIBOLD, "JetBrains Mono", MONO_MEDIUM] {
            assert!(fonts.font_data.contains_key(name), "{name}");
        }
        assert_eq!(fonts.families[&FontFamily::Proportional][0], "Inter");
        assert_eq!(fonts.families[&FontFamily::Monospace][0], "JetBrains Mono");
        // egui's own fonts stay behind them for ▾, × and anything else they lack.
        assert!(fonts.families[&FontFamily::Proportional].len() > 1);
    }

    #[test]
    fn the_type_scale_is_the_specs() {
        assert_eq!(Type::Body.font().size, 14.0);
        assert_eq!(Type::Status.font().size, 15.0);
        assert_eq!(Type::Caption.font().size, 13.0);
        assert_eq!((Type::Label.font().size, Type::Label.letter_spacing()), (11.0, 1.0));
        assert_eq!(Type::Head.font().family, FontFamily::Name(MONO_MEDIUM.into()));
        assert_eq!(Type::Countdown.font(), FontId::new(12.0, FontFamily::Monospace));
        assert_eq!(Type::Stat.font().size, 26.0);
        assert_eq!((Type::CodeShow.font().size, Type::CodeShow.letter_spacing()), (54.0, 4.0));
        assert_eq!((Type::CodeEnter.font().size, Type::CodeEnter.letter_spacing()), (44.0, 4.0));
        assert_eq!(Type::Label.job("lost", TEXT_DIM).text, "LOST");
    }
}
