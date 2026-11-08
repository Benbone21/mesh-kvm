//! The small controls the window is built from.

use super::theme::{self, line, legacy::{AMBER, DEEP, DIM, EDGE, GREEN, INK, RAISED}, Type, ACCENT, DANGER, LINE_STRONG, ON_ACCENT, SPACE_3, STATUS_SQUARE, TEXT};
use eframe::egui::{
    self, text::LayoutJob, text::TextWrapping, vec2, Color32, CursorIcon, FontFamily, FontId, Galley, Painter, Pos2, Rect,
    Response, RichText, Sense, StrokeKind, TextFormat, TextStyle, Ui, Vec2, WidgetInfo, WidgetType,
};
use std::ops::RangeInclusive;
use std::sync::Arc;

const PILL_SIZE: [f32; 2] = [40.0, 22.0];
const METER_HEIGHT: f32 = 6.0;

pub fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).size(theme::legacy::SMALL_SIZE).color(DIM)
}

fn body_font(ui: &Ui) -> FontId {
    TextStyle::Body.resolve(ui.style())
}

/// Text cut to fit `width`, on at most `rows` rows, ending in an ellipsis if it had to be cut.
pub fn fitted(painter: &Painter, text: &str, font: FontId, colour: Color32, width: f32, rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat::simple(font, colour));
    job.wrap = TextWrapping { max_width: width.max(1.0), max_rows: rows, break_anywhere: rows == 1, overflow_character: Some('\u{2026}') };
    painter.layout_job(job)
}

pub fn wide_button(ui: &mut Ui, button: egui::Button) -> Response {
    ui.add_sized([ui.available_width(), theme::legacy::BUTTON_HEIGHT], button)
}

/// The old controls' ring, until they go.
fn legacy_focus_ring(ui: &Ui, response: &Response, rect: Rect) {
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), theme::legacy::RADIUS, line(2.0, AMBER), StrokeKind::Outside);
    }
}

/// A 2px `accent` outline 2px out from a control that has the keyboard.
pub fn focus_ring(ui: &Ui, response: &Response) {
    if response.has_focus() {
        ui.painter().rect_stroke(response.rect.expand(2.0), 0.0, line(2.0, ACCENT), StrokeKind::Outside);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// Filled `accent`, for the one thing a view most wants done.
    Primary,
    /// Outlined in `line-strong`.
    Secondary,
    /// Outlined in `danger`.
    Danger,
}

impl ButtonKind {
    /// A primary button's label is semibold.
    fn font(self) -> FontId {
        match self {
            ButtonKind::Primary => FontId::new(Type::Body.font().size, FontFamily::Name(theme::INTER_SEMIBOLD.into())),
            _ => Type::Body.font(),
        }
    }
}

/// How wide `flat_button` makes a button with `label`, when its least width is less.
pub fn flat_button_width(ui: &Ui, label: &str, kind: ButtonKind) -> f32 {
    ui.fonts(|f| f.layout_no_wrap(label.to_owned(), kind.font(), TEXT)).size().x + 2.0 * SPACE_3
}

/// A square button with its label centred, at least `min_size`. A disabled one is drawn at 40%
/// and takes no clicks or keyboard.
pub fn flat_button(ui: &mut Ui, label: &str, kind: ButtonKind, min_size: Vec2, enabled: bool) -> Response {
    let ink = match kind {
        ButtonKind::Primary => ON_ACCENT,
        ButtonKind::Secondary => TEXT,
        ButtonKind::Danger => DANGER,
    };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), kind.font(), ink);
    let size = min_size.max(galley.size() + vec2(2.0 * SPACE_3, 0.0));
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, mut response) = ui.allocate_exact_size(size, sense);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if enabled {
        response = response.on_hover_cursor(CursorIcon::PointingHand);
    }
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let (hovered, pressed) = (enabled && response.hovered(), enabled && response.is_pointer_button_down_on());
    let (fill, border) = match kind {
        ButtonKind::Primary => {
            let fill = if pressed { theme::pressed(ACCENT) } else if hovered { theme::hovered(ACCENT) } else { ACCENT };
            (fill, None)
        }
        ButtonKind::Secondary => {
            let fill = if pressed { LINE_STRONG.gamma_multiply(0.15) } else { Color32::TRANSPARENT };
            (fill, Some(if hovered { TEXT } else { LINE_STRONG }))
        }
        ButtonKind::Danger => {
            let fill = if pressed { DANGER.gamma_multiply(0.15) } else if hovered { DANGER.gamma_multiply(0.1) } else { Color32::TRANSPARENT };
            (fill, Some(DANGER))
        }
    };
    let shade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.4) };
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, shade(fill));
    if let Some(border) = border {
        painter.rect_stroke(rect, 0.0, line(1.0, shade(border)), StrokeKind::Inside);
    }
    painter.galley(rect.center() - galley.size() / 2.0, galley, shade(ink));
    focus_ring(ui, &response);
    response
}

/// The square at the start of the status bar, and of the update and notice lines.
pub fn status_square(ui: &mut Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(STATUS_SQUARE), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, colour);
}

/// A label with an on and off switch. The whole row can be clicked.
pub fn toggle_row(ui: &mut Ui, label: &str, on: bool) -> Response {
    let width = ui.available_width();
    let pill = vec2(PILL_SIZE[0], PILL_SIZE[1]);
    let galley = ui.painter().layout(label.to_string(), body_font(ui), INK, (width - pill.x - 12.0).max(1.0));
    let height = (galley.size().y + 8.0).max(theme::legacy::ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let enabled = ui.is_enabled();
    let ink = if enabled { INK } else { DIM };
    let painter = ui.painter();
    painter.galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, ink);

    let track = Rect::from_center_size(Pos2::new(rect.right() - pill.x / 2.0, rect.center().y), pill);
    let t = ui.ctx().animate_bool(response.id, on);
    let corner = pill.y / 2.0;
    painter.rect_filled(track, corner, theme::mix(DEEP, AMBER, t));
    let border = if response.hovered() { INK } else { EDGE };
    painter.rect_stroke(track, corner, line(1.0, border), StrokeKind::Inside);
    let travel = pill.x - pill.y;
    let center = Pos2::new(track.left() + corner + travel * t, track.center().y);
    painter.circle_filled(center, corner - 4.0, theme::mix(DIM, DEEP, t));
    legacy_focus_ring(ui, &response, track);
    response
}

/// A label, its current value, and a slider under them. Returns true when the person moved it.
pub fn slider_row(
    ui: &mut Ui,
    label: &str,
    value_text: String,
    value: &mut f64,
    range: RangeInclusive<f64>,
    step: f64,
) -> bool {
    // The label wraps in the room the value leaves, so the two never touch.
    let value_galley = ui.painter().layout_no_wrap(value_text, body_font(ui), AMBER);
    let label_width = (ui.available_width() - value_galley.size().x - 12.0).max(40.0);
    let label_galley = ui.painter().layout(label.to_owned(), body_font(ui), INK, label_width);
    let height = label_galley.size().y.max(value_galley.size().y).max(20.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let top = rect.top() + (height - label_galley.size().y) / 2.0;
    ui.painter().galley(Pos2::new(rect.left(), top), label_galley, INK);
    let value_pos = Pos2::new(rect.right() - value_galley.size().x, rect.center().y - value_galley.size().y / 2.0);
    ui.painter().galley(value_pos, value_galley, AMBER);

    let before = *value;
    ui.spacing_mut().slider_width = ui.available_width();
    // A value already outside the range stays as it is until the person moves the slider.
    let slider = egui::Slider::new(value, range)
        .step_by(step)
        .show_value(false)
        .clamping(egui::SliderClamping::Never);
    ui.add(slider).changed() && *value != before
}

pub fn meter(ui: &mut Ui, percent: u32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), METER_HEIGHT), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, DEEP);
    let level = rect.width() * percent.min(100) as f32 / 100.0;
    let filled = Rect::from_min_size(rect.min, vec2(level, rect.height()));
    ui.painter().rect_filled(filled, 2.0, GREEN);
}

/// One half of a two-way choice.
pub fn segment(ui: &mut Ui, width: f32, label: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(width, theme::legacy::BUTTON_HEIGHT), Sense::click());
    let (fill, ink) = if selected { (AMBER, DEEP) } else { (RAISED, INK) };
    let painter = ui.painter();
    painter.rect_filled(rect, theme::legacy::SMALL_RADIUS, fill);
    let border = if response.hovered() { INK } else if selected { AMBER } else { EDGE };
    painter.rect_stroke(rect, theme::legacy::SMALL_RADIUS, line(1.0, border), StrokeKind::Inside);
    let galley = fitted(painter, label, FontId::proportional(theme::legacy::SMALL_SIZE + 1.0), ink, width - 12.0, 1);
    painter.galley(rect.center() - galley.size() / 2.0, galley, ink);
    legacy_focus_ring(ui, &response, rect);
    response
}
