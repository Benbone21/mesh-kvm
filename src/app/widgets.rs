//! The small controls the window is built from.

use super::theme::{self, line, Type, ACCENT, DANGER, LINE_STRONG, ON_ACCENT, SPACE_3, STATUS_SQUARE, TEXT};
use eframe::egui::{
    text::LayoutJob, text::TextWrapping, vec2, Color32, CursorIcon, FontFamily, FontId, Galley, Painter, Response,
    Sense, StrokeKind, TextFormat, Ui, Vec2, WidgetInfo, WidgetType,
};
use std::sync::Arc;

/// Text cut to fit `width`, on at most `rows` rows, ending in an ellipsis if it had to be cut.
pub fn fitted(painter: &Painter, text: &str, font: FontId, colour: Color32, width: f32, rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat::simple(font, colour));
    job.wrap = TextWrapping { max_width: width.max(1.0), max_rows: rows, break_anywhere: rows == 1, overflow_character: Some('\u{2026}') };
    painter.layout_job(job)
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

