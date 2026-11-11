//! The Connection details panel under the live figures: a bar that opens and closes it, with Copy
//! details at its end, and both devices' figures side by side with the groups down the left, as
//! in the settings table, then each device's history. It draws what `details::view` worked out
//! and hands back what the person asked for. Opening and closing is immediate, so there is no
//! motion to reduce.

use super::details::{DetailCell, DetailRow, DetailsView, HistoryLine};
use super::theme::{
    line, tone, Type, LINE_FAINT, LINE_STRONG, ROW_MIN_HEIGHT, SPACE_1, SPACE_2, SPACE_3, STATUS_BUTTON_HEIGHT,
    TEXT, TEXT_DIM,
};
use super::view::fit_name;
use super::widgets::{fitted, flat_button, focus_ring, ButtonKind};
use eframe::egui::{
    self, vec2, Align, CursorIcon, Id, Layout, Pos2, Rect, Response, Sense, Shape, Ui, WidgetInfo, WidgetType,
};

pub const TITLE: &str = "Connection details";
pub const COPY: &str = "Copy details";
pub const COPIED: &str = "Copied";
/// The bar's height while closed, and its toggle's.
pub const TOGGLE_HEIGHT: f32 = 28.0;
const TRIANGLE: f32 = 8.0;
/// The label column takes this share of the width, within these bounds.
const LABEL_SHARE: f32 = 0.28;
const LABEL_WIDTH: [f32; 2] = [140.0, 240.0];
/// Past this a column only spreads its values further from their labels.
const MOST_COLUMN: f32 = 380.0;
const CELL_GAP: f32 = SPACE_2;
const GROUP_GAP: f32 = 10.0;
/// The time at the start of each history line.
const TIME_WIDTH: f32 = 72.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailsRequest {
    Toggle,
    Copy,
}

/// The id of the toggle, for focus and tests.
pub fn toggle_id() -> Id {
    Id::new("connection details toggle")
}

/// The id of a value's cell, from its row's label and its column (0 is this device).
pub fn cell_id(label: &str, column: usize) -> Id {
    Id::new(("details cell", label, column))
}

/// The bar: the toggle on the left, and Copy details on the right while open. `copied` says the
/// last copy has just been made.
pub fn bar(ui: &mut Ui, open: bool, copied: bool) -> Option<DetailsRequest> {
    let mut request = None;
    let height = if open { STATUS_BUTTON_HEIGHT } else { TOGGLE_HEIGHT };
    ui.horizontal(|ui| {
        ui.set_min_height(height);
        let copy_label = if copied { COPIED } else { COPY };
        let copy_width = if open { super::widgets::flat_button_width(ui, copy_label, ButtonKind::Secondary) + SPACE_3 } else { 0.0 };
        let room = (ui.available_width() - copy_width).max(TOGGLE_HEIGHT);
        if toggle(ui, open, room, height).clicked() {
            request = Some(DetailsRequest::Toggle);
        }
        if open {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let button = flat_button(ui, copy_label, ButtonKind::Secondary, vec2(0.0, STATUS_BUTTON_HEIGHT), true)
                    .on_hover_text("Puts both devices' details and history on the clipboard as text, to paste into a message.");
                if button.clicked() {
                    request = Some(DetailsRequest::Copy);
                }
            });
        }
    });
    request
}

/// A triangle and the title, which open and close the panel by click, Space or Enter.
fn toggle(ui: &mut Ui, open: bool, room: f32, height: f32) -> Response {
    let galley = ui.painter().layout_job(Type::Label.job(TITLE, TEXT_DIM));
    let width = (TRIANGLE + SPACE_2 + galley.size().x + SPACE_2).min(room);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let response = ui.interact(rect, toggle_id(), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let state = if open { "shown" } else { "hidden" };
    response.widget_info(|| WidgetInfo::labeled(WidgetType::CollapsingHeader, true, format!("{TITLE}, {state}")));
    let ink = if response.hovered() || response.has_focus() { TEXT } else { TEXT_DIM };
    let centre = Pos2::new(rect.left() + TRIANGLE / 2.0, rect.center().y);
    let half = TRIANGLE / 2.0;
    // Pointing right while closed, down while open.
    let points = if open {
        vec![centre + vec2(-half, -half / 2.0), centre + vec2(half, -half / 2.0), centre + vec2(0.0, half / 1.5)]
    } else {
        vec![centre + vec2(-half / 2.0, -half), centre + vec2(-half / 2.0, half), centre + vec2(half / 1.5, 0.0)]
    };
    ui.painter().add(Shape::convex_polygon(points, ink, egui::Stroke::NONE));
    let at = Pos2::new(rect.left() + TRIANGLE + SPACE_2, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(at, galley, ink);
    focus_ring(ui, &response);
    response
}

/// How wide the label column is for `width` in all.
pub fn label_width(width: f32) -> f32 {
    (width * LABEL_SHARE).clamp(LABEL_WIDTH[0], LABEL_WIDTH[1]).min(width / 3.0)
}

/// Both columns, group by group, then the history. `names` is each column's device, this one first.
pub fn body(ui: &mut Ui, view: &DetailsView, names: &[String; 2]) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let width = ui.available_width();
    let label = label_width(width);
    let column = ((width - label) / 2.0).clamp(1.0, MOST_COLUMN);
    header(ui, view, names, label, column);
    for group in &view.groups {
        ui.add_space(GROUP_GAP);
        ui.label(Type::Label.job(group.heading, TEXT_DIM));
        ui.add_space(SPACE_1);
        for row in &group.rows {
            value_row(ui, row, names, label, column);
        }
    }
    ui.add_space(GROUP_GAP);
    ui.label(Type::Label.job("History", TEXT_DIM));
    ui.add_space(SPACE_1);
    history(ui, view, names, label, column);
    ui.add_space(SPACE_3);
}

/// Both device names, upper-cased and cut to fit, with why a device has nothing under its name.
fn header(ui: &mut Ui, view: &DetailsView, names: &[String; 2], label: f32, column: f32) {
    let head = ui.fonts(|f| f.row_height(&Type::Head.font()));
    let caption = ui.fonts(|f| f.row_height(&Type::Caption.font()));
    let has_status = view.status.iter().any(Option::is_some);
    let height = head + if has_status { caption + SPACE_1 } else { 0.0 } + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(label + 2.0 * column, height), Sense::hover());
    for (i, name) in names.iter().enumerate() {
        let left = rect.left() + label + column * i as f32;
        let room = column - CELL_GAP;
        let (short, full) = fit_name(name);
        let galley = fitted(ui.painter(), &short.to_uppercase(), Type::Head.font(), TEXT, room, 1);
        let response = ui.interact(Rect::from_min_size(Pos2::new(left, rect.top()), vec2(room, head)), Id::new(("details header", i)), Sense::hover());
        let whole = if galley.elided { Some(name.clone()) } else { full };
        ui.painter().galley(Pos2::new(left, rect.top()), galley, TEXT);
        if let Some(whole) = whole {
            response.on_hover_text(whole);
        }
        if let Some(status) = &view.status[i] {
            let at = Pos2::new(left, rect.top() + head + SPACE_1);
            let galley = fitted(ui.painter(), &status.text, Type::Caption.font(), tone(status.tone), room, 1);
            let said = ui.interact(Rect::from_min_size(at, vec2(room, caption)), Id::new(("details status", i)), Sense::hover());
            said.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, format!("{name}, {}", status.text)));
            ui.painter().galley(at, galley, tone(status.tone));
        }
    }
    let (rule, _) = ui.allocate_exact_size(vec2(label + 2.0 * column, 1.0), Sense::hover());
    ui.painter().rect_filled(rule, 0.0, LINE_STRONG);
}

fn value_row(ui: &mut Ui, row: &DetailRow, names: &[String; 2], label: f32, column: f32) {
    let width = label + 2.0 * column;
    let mut job = Type::Body.job(row.label, TEXT);
    job.wrap.max_width = (label - CELL_GAP).max(1.0);
    let galley = ui.painter().layout_job(job);
    let height = (galley.size().y + 2.0 * SPACE_1).max(ROW_MIN_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let label_rect = Rect::from_min_size(rect.min, vec2(label, height));
    ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, TEXT);
    if let Some(hint) = row.hint {
        ui.interact(label_rect, Id::new(("details label", row.label)), Sense::hover()).on_hover_text(hint);
    }
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, line(1.0, LINE_FAINT));
    for (i, cell) in row.cells.iter().enumerate() {
        let left = rect.left() + label + column * i as f32;
        let cell_rect = Rect::from_min_size(Pos2::new(left, rect.top()), vec2(column, height - 1.0));
        value(ui, cell, cell_rect, cell_id(row.label, i), &format!("{}, {}", row.label, names[i]), row.hint);
    }
}

/// One value, cut to its column with the whole of it in a tooltip, and named for a screen reader
/// as its row and device.
fn value(ui: &mut Ui, cell: &DetailCell, rect: Rect, id: Id, spoken: &str, hint: Option<&str>) {
    let colour = tone(cell.tone);
    let galley = fitted(ui.painter(), &cell.text, Type::Value.font(), colour, rect.width() - CELL_GAP, 1);
    let cut = galley.elided;
    ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, colour);
    let response = ui.interact(rect, id, Sense::hover());
    response.widget_info(|| WidgetInfo {
        label: Some(spoken.to_string()),
        current_text_value: Some(cell.text.clone()),
        // As the settings table names its cells: a label for the row and device, the value as value.
        ..WidgetInfo::new(WidgetType::Other)
    });
    let lines: Vec<&str> = [cut.then_some(cell.text.as_str()), cell.tooltip.as_deref(), hint].into_iter().flatten().collect();
    if !lines.is_empty() {
        response.on_hover_text(lines.join("\n"));
    }
}

/// Each device's events side by side, newest first, the label column left empty.
fn history(ui: &mut Ui, view: &DetailsView, names: &[String; 2], label: f32, column: f32) {
    let top = ui.cursor().top();
    let left = ui.cursor().left();
    let mut bottom = top;
    for (i, lines) in view.history.iter().enumerate() {
        let x = left + label + column * i as f32;
        let mut y = top;
        if lines.is_empty() {
            let galley = fitted(ui.painter(), "Nothing yet", Type::Body.font(), TEXT_DIM, column - CELL_GAP, 1);
            y += galley.size().y + SPACE_1;
            ui.painter().galley(Pos2::new(x, top + SPACE_1), galley, TEXT_DIM);
        }
        for (n, entry) in lines.iter().enumerate() {
            y = history_line(ui, entry, Pos2::new(x, y), column, Id::new(("details history", i, n)), &names[i]);
        }
        bottom = bottom.max(y);
    }
    ui.allocate_exact_size(vec2(label + 2.0 * column, bottom - top), Sense::hover());
}

/// One line at `at`, its time then its words wrapped beside it. Where the next line goes.
fn history_line(ui: &mut Ui, entry: &HistoryLine, at: Pos2, column: f32, id: Id, device: &str) -> f32 {
    let room = (column - TIME_WIDTH - CELL_GAP).max(1.0);
    let time = ui.painter().layout_job(Type::Value.job(&entry.time, TEXT_DIM));
    let mut job = Type::Body.job(&entry.text, tone(entry.tone));
    job.wrap.max_width = room;
    let words = ui.painter().layout_job(job);
    let height = (words.size().y + 2.0 * SPACE_1).max(ROW_MIN_HEIGHT - 4.0);
    let rect = Rect::from_min_size(at, vec2(column - CELL_GAP, height));
    let first_line = ui.fonts(|f| f.row_height(&Type::Body.font()));
    let time_y = rect.top() + SPACE_1 + (first_line - time.size().y) / 2.0;
    ui.painter().galley(Pos2::new(rect.left(), time_y), time, TEXT_DIM);
    ui.painter().galley(Pos2::new(rect.left() + TIME_WIDTH, rect.top() + SPACE_1), words, tone(entry.tone));
    let response = ui.interact(rect, id, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, format!("{device}, {}, {}", entry.time, entry.text)));
    response.on_hover_text(&entry.tooltip);
    rect.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_column_takes_a_share_within_its_bounds() {
        assert_eq!(label_width(1240.0), 240.0);
        assert_eq!(label_width(720.0), 201.6);
        assert_eq!(label_width(300.0), 100.0, "never more than a third");
        assert_eq!(label_width(500.0), 140.0);
    }
}
