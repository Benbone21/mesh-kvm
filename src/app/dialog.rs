//! The one look every dialog has: a raised box a quarter of the way down, over the dimmed app.

use super::theme::{
    self,
    line, Type, BG, DIALOG_BUTTON_HEIGHT, DIALOG_BUTTON_MIN_WIDTH, DIALOG_WIDTH, LINE_STRONG, RAISED, SPACE_1, SPACE_3, SPACE_4, TEXT_DIM,
};
use super::widgets::{flat_button, ButtonKind};
use eframe::egui::{
    self, pos2, vec2, Align, Area, Color32, Frame, Id, Layout, Margin, Modal, Order, Pos2, Rect, Response, Sense, Ui,
    UiBuilder, UiKind, Vec2,
};
use std::hash::Hash;

/// The padding inside a dialog: 18 at the top, 20 at the sides and bottom.
const PADDING: Margin = Margin { left: 20, right: 20, top: 18, bottom: 20 };
const BORDER: f32 = 1.0;
/// The space between a dialog's buttons, as the spec's component table has it.
const BUTTON_GAP: f32 = SPACE_3;
/// How much of the app shows through the backdrop.
const BACKDROP: f32 = 0.6;

/// Where a dialog `height` tall goes on a window of `screen`: centred across, a quarter of the
/// way down, and moved up when that would put its bottom within 16 of the edge.
pub fn origin(screen: Vec2, height: f32) -> Pos2 {
    let x = (screen.x - DIALOG_WIDTH) / 2.0;
    let y = (screen.y * 0.25).min(screen.y - SPACE_4 - height).max(SPACE_4);
    pos2(x.max(0.0), y)
}

pub struct Shown<R> {
    pub inner: R,
    /// Esc was pressed while this was the dialog on top.
    pub escaped: bool,
}

/// Draws a dialog titled `title`, with `body` inside it. Everything behind it is dimmed and
/// takes no input while it is up.
pub fn show<R>(ctx: &egui::Context, id: Id, title: &str, opacity: f32, body: impl FnOnce(&mut Ui) -> R) -> Shown<R> {
    let height = ctx.memory(|m| m.area_rect(id)).map_or(0.0, |r| r.height());
    let area = Area::new(id)
        .kind(UiKind::Modal)
        .sense(Sense::hover())
        .order(Order::Foreground)
        .interactable(true)
        .fade_in(false)
        .fixed_pos(origin(ctx.screen_rect().size(), height));
    let just_opened = !ctx.memory(|m| m.areas().visible_last_frame(&area.layer()));
    let frame = Frame::new().fill(RAISED).stroke(line(BORDER, LINE_STRONG)).inner_margin(PADDING);
    let shown = Modal::new(id)
        .area(area)
        .frame(frame)
        .backdrop_color(theme::faded(BG, BACKDROP * opacity))
        .show(ctx, |ui| {
            ui.multiply_opacity(opacity);
            ui.set_width(DIALOG_WIDTH - PADDING.sum().x - 2.0 * BORDER);
            ui.spacing_mut().item_spacing = Vec2::splat(SPACE_3);
            ui.data_mut(|d| d.insert_temp(just_opened_id(), just_opened));
            text(ui, title, Type::Label, TEXT_DIM);
            body(ui)
        });
    let escaped = shown.is_top_modal
        && !shown.any_popup_open
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    Shown { inner: shown.inner, escaped }
}

fn just_opened_id() -> Id {
    Id::new("dialog just opened")
}

/// Gives `response` the keyboard on the frame its dialog opens.
pub fn focus_on_open(ui: &Ui, response: &Response) {
    if ui.data(|d| d.get_temp(just_opened_id())).unwrap_or(false) {
        response.request_focus();
    }
}

/// `text` in style `kind`, wrapped to the dialog.
pub fn text(ui: &mut Ui, text: &str, kind: Type, colour: Color32) -> Response {
    let mut job = kind.job(text, colour);
    job.wrap.max_width = ui.available_width();
    ui.label(job)
}

/// A dialog button, at least 96 by 40.
pub fn button(ui: &mut Ui, label: &str, kind: ButtonKind, enabled: bool) -> Response {
    flat_button(ui, label, kind, vec2(DIALOG_BUTTON_MIN_WIDTH, DIALOG_BUTTON_HEIGHT), enabled)
}

/// A row of buttons against the right edge, 12 apart, in the order `add` adds them, which is also
/// the order the keyboard reaches them in. The row's width is only known once it is drawn, so it
/// is placed by last frame's width, kept under `id_salt`, and drawn again when that changes.
pub fn buttons(ui: &mut Ui, id_salt: impl Hash, add: impl FnOnce(&mut Ui)) {
    let id = ui.id().with(("dialog buttons", id_salt));
    let width = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or(0.0);
    ui.add_space(SPACE_1);
    let full = ui.available_rect_before_wrap();
    let left = (full.right() - width).max(full.left());
    let rect = Rect::from_min_max(pos2(left, full.top()), pos2(full.right(), full.top() + DIALOG_BUTTON_HEIGHT));
    let row = ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = BUTTON_GAP;
        add(ui);
    });
    let drawn = row.response.rect.width();
    if (drawn - width).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, drawn));
        ui.ctx().request_discard("dialog buttons moved to the right edge");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::theme;
    use eframe::egui::{pos2, vec2, Event, Key, RawInput};

    #[test]
    fn a_dialog_sits_a_quarter_down_and_centred() {
        assert_eq!(origin(vec2(1280.0, 760.0), 300.0), pos2(410.0, 190.0));
        assert_eq!(origin(vec2(760.0, 560.0), 500.0), pos2(150.0, 44.0));
    }

    fn run(ctx: &egui::Context, events: Vec<Event>) -> (bool, Option<Rect>, Vec<(String, Rect)>, Vec<Rect>) {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 760.0))), events, ..Default::default() };
        let (mut escaped, mut pressable) = (false, Vec::new());
        let output = ctx.run(raw, |ctx| {
            escaped = show(ctx, Id::new("test dialog"), "Test", 1.0, |ui| {
                text(ui, "A sentence.", Type::Body, theme::TEXT);
                buttons(ui, "test", |ui| {
                    pressable.push(button(ui, "Cancel", ButtonKind::Secondary, true).rect);
                    pressable.push(button(ui, "Go", ButtonKind::Primary, true).rect);
                });
            })
            .escaped;
        });
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(shape) = &clipped.shape {
                texts.push((shape.galley.text().to_string(), shape.visual_bounding_rect()));
            }
        }
        (escaped, ctx.memory(|m| m.area_rect(Id::new("test dialog"))), texts, pressable)
    }

    #[test]
    fn a_dialog_is_460_wide_with_its_title_and_its_buttons_on_the_right_in_order() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut last = (false, None, Vec::new(), Vec::new());
        for _ in 0..3 {
            last = run(&ctx, vec![]);
        }
        let (escaped, area, texts, pressable) = last;
        let area = area.unwrap();
        assert!(!escaped);
        assert_eq!(area.width(), DIALOG_WIDTH);
        assert_eq!(area.left(), (1280.0 - DIALOG_WIDTH) / 2.0);
        assert!(texts.iter().any(|(t, _)| t == "TEST"), "{texts:?}");
        let at = |label: &str| texts.iter().find(|(t, _)| t == label).unwrap().1;
        assert!(at("Cancel").right() < at("Go").left());
        let [cancel, go] = pressable[..] else { panic!("{pressable:?}") };
        assert_eq!(go.left() - cancel.right(), 12.0, "the gap between buttons");
        assert_eq!(go.right(), area.right() - 21.0, "against the padding on the right");
        assert_eq!((go.width(), go.height()), (DIALOG_BUTTON_MIN_WIDTH, DIALOG_BUTTON_HEIGHT));
        // The last button ends at the padding on the right, and is at least 96 wide.
        let edge = area.right() - 21.0;
        assert!(at("Go").right() < edge && at("Go").left() > edge - DIALOG_BUTTON_MIN_WIDTH, "{area:?} {:?}", at("Go"));
    }

    #[test]
    fn esc_is_reported() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        run(&ctx, vec![]);
        let esc = Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
        assert!(run(&ctx, vec![esc]).0);
        assert!(!run(&ctx, vec![]).0);
    }
}
