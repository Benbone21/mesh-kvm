//! The small controls the window is built from.

use super::theme::{self, line, legacy::{AMBER, AMBER_GROOVE, DEEP, DIM, EDGE, GREEN, INK, RAISED, UNLIT}};
use eframe::egui::{
    self, text::LayoutJob, text::TextWrapping, vec2, Align2, Color32, FontId, Galley, Painter, Pos2, Rect, Response,
    RichText, Sense, StrokeKind, TextFormat, TextStyle, Ui,
};
use std::ops::RangeInclusive;
use std::sync::Arc;

const PILL_SIZE: [f32; 2] = [40.0, 22.0];
const METER_HEIGHT: f32 = 6.0;
const SWITCH_TRAVEL_SECONDS: f32 = 0.16;
const DISABLED_SWITCH: f32 = 0.55;

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

fn focus_ring(ui: &Ui, response: &Response, rect: Rect) {
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), theme::legacy::RADIUS, line(2.0, AMBER), StrokeKind::Outside);
    }
}

/// What a device's status light shows.
pub use super::model::Light;

pub fn paint_light(painter: &Painter, center: Pos2, light: Light) {
    const RADIUS: f32 = 6.0;
    let colour = match light {
        Light::Off => {
            painter.circle(center, RADIUS, UNLIT, line(2.0, EDGE));
            return;
        }
        Light::Connected => GREEN,
        Light::Active => AMBER,
    };
    for (spread, alpha) in [(6.0, 0.05), (4.0, 0.08), (2.5, 0.14), (1.2, 0.2)] {
        painter.circle_filled(center, RADIUS + spread, colour.gamma_multiply(alpha));
    }
    painter.circle_filled(center, RADIUS, colour);
}

/// The one physical object on the panel: a slide switch that shows who owns the keyboard. `side`
/// is where the knob sits, from 0 at the left to 1 at the right, and 0.5 when nobody is known to
/// have control.
pub fn slide_switch(ui: &mut Ui, rect: Rect, side: f32, knob_text: Option<&str>, enabled: bool, hint: String) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.id().with("slide switch"), sense).on_hover_text(hint);
    let shade = if enabled { 1.0 } else { DISABLED_SWITCH };
    let painter = ui.painter();
    let radius = theme::legacy::RADIUS;
    let border = if enabled && response.hovered() { INK } else { EDGE };
    painter.rect_filled(rect, radius, DEEP);
    // A shadow along the top makes the track read as a recess.
    for (depth, alpha) in [(2.0, 0.4), (4.0, 0.25), (7.0, 0.12)] {
        let shadow = Rect::from_min_size(rect.min, vec2(rect.width(), depth)).shrink2(vec2(2.0, 0.0));
        painter.rect_filled(shadow, 0.0, Color32::BLACK.gamma_multiply(alpha));
    }
    painter.rect_stroke(rect, radius, line(2.0, border.gamma_multiply(shade)), StrokeKind::Inside);

    let inset = 5.0;
    let knob_size = vec2((rect.width() - 2.0 * inset) * 0.58, rect.height() - 2.0 * inset);
    let travel = rect.width() - 2.0 * inset - knob_size.x;
    let position = ui.ctx().animate_value_with_time(response.id.with("knob"), side, SWITCH_TRAVEL_SECONDS);
    let knob = Rect::from_min_size(rect.min + vec2(inset + travel * position, inset), knob_size);
    painter.rect_filled(knob, 3.0, AMBER.gamma_multiply(shade));
    let grooves = painter.with_clip_rect(knob.shrink(1.0));
    for edge in [knob.left() + 4.0, knob.right() - 7.0] {
        let groove = Rect::from_min_size(Pos2::new(edge, knob.top()), vec2(2.0, knob.height()));
        grooves.rect_filled(groove, 0.0, AMBER_GROOVE.gamma_multiply(shade));
    }
    if let Some(text) = knob_text {
        let galley = fitted(painter, text, FontId::proportional(theme::legacy::SMALL_SIZE + 1.0), DEEP, knob.width() - 18.0, 1);
        let at = knob.center() - galley.size() / 2.0;
        // Painted twice, half a pixel apart, as the bundled font has no bold.
        painter.galley(at, galley.clone(), DEEP);
        painter.galley(at + vec2(0.5, 0.0), galley, DEEP);
    }
    focus_ring(ui, &response, rect);
    response
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
    focus_ring(ui, &response, track);
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
    focus_ring(ui, &response, rect);
    response
}

/// Text at one end of `rect`, cut to fit, with a line under it if there is a second.
pub fn paint_name_and_state(painter: &Painter, rect: Rect, title: &str, state: &str, side: Align2) {
    let width = rect.width();
    let title = fitted(painter, title, FontId::proportional(theme::legacy::HEADING_SIZE), INK, width, 1);
    let state = fitted(painter, state, FontId::proportional(theme::legacy::SMALL_SIZE), DIM, width, 1);
    let total = title.size().y + state.size().y;
    let top = rect.center().y - total / 2.0;
    let x = |size: f32| if side == Align2::LEFT_CENTER { rect.left() } else { rect.right() - size };
    let state_at = Pos2::new(x(state.size().x), top + title.size().y);
    painter.galley(Pos2::new(x(title.size().x), top), title, INK);
    painter.galley(state_at, state, DIM);
}
