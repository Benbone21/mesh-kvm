//! The Connection details panel under the live figures: a bar that opens and closes it, with Copy
//! details at its end, and both devices' figures side by side with the groups down the left, as
//! in the settings table, then each device's history. It draws what `details::view` worked out
//! and hands back what the person asked for. Opening and closing is immediate, so there is no
//! motion to reduce.

use super::details::{DetailCell, DetailGroup, DetailRow, DetailsView, HistoryLine};
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

/// How wide the label column is for `width` in all, with one block of groups.
pub fn label_width(width: f32) -> f32 {
    (width * LABEL_SHARE).clamp(LABEL_WIDTH[0], LABEL_WIDTH[1]).min(width / 3.0)
}

/// How the groups are set out: in one block, or in two side by side on a wide window, each with
/// its label column and a column for each device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setout {
    pub blocks: usize,
    pub block: f32,
    pub label: f32,
    pub column: f32,
}

/// From this wide the groups go in two blocks side by side, which halves the height they take.
pub const TWO_BLOCKS_FROM: f32 = 1100.0;
const BLOCK_GAP: f32 = 32.0;
const TWO_BLOCK_LABEL: [f32; 2] = [130.0, 170.0];
/// The groups that go in the first of two blocks.
const FIRST_BLOCK_GROUPS: usize = 3;

pub fn shape(width: f32) -> Setout {
    if width >= TWO_BLOCKS_FROM {
        let block = (width - BLOCK_GAP) / 2.0;
        let label = (block * 0.3).clamp(TWO_BLOCK_LABEL[0], TWO_BLOCK_LABEL[1]);
        return Setout { blocks: 2, block, label, column: (block - label) / 2.0 };
    }
    let label = label_width(width);
    let column = ((width - label) / 2.0).clamp(1.0, MOST_COLUMN);
    Setout { blocks: 1, block: label + 2.0 * column, label, column }
}

impl Setout {
    fn block_left(&self, left: f32, block: usize) -> f32 {
        left + (self.block + BLOCK_GAP) * block as f32
    }
}

/// Both device names over each block's columns, with why a device has nothing, above a rule. It
/// stays put while the figures under it scroll.
pub fn header(ui: &mut Ui, view: &DetailsView, names: &[String; 2], shape: Setout) {
    ui.spacing_mut().item_spacing.y = 0.0;
    names_row(ui, Some(view), names, shape, "details header");
}

fn names_row(ui: &mut Ui, view: Option<&DetailsView>, names: &[String; 2], shape: Setout, salt: &str) {
    let head = ui.fonts(|f| f.row_height(&Type::Head.font()));
    let caption = ui.fonts(|f| f.row_height(&Type::Caption.font()));
    let has_status = view.is_some_and(|v| v.status.iter().any(Option::is_some));
    let height = head + if has_status { caption + SPACE_1 } else { 0.0 } + 6.0;
    let width = shape.block_left(0.0, shape.blocks - 1) + shape.block;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    for block in 0..shape.blocks {
        let block_left = shape.block_left(rect.left(), block);
        for (i, name) in names.iter().enumerate() {
            let left = block_left + shape.label + shape.column * i as f32;
            let room = shape.column - CELL_GAP;
            let (short, full) = fit_name(name);
            let galley = fitted(ui.painter(), &short.to_uppercase(), Type::Head.font(), TEXT, room, 1);
            let id = Id::new((salt, block, i));
            let response = ui.interact(Rect::from_min_size(Pos2::new(left, rect.top()), vec2(room, head)), id, Sense::hover());
            let whole = if galley.elided { Some(name.clone()) } else { full };
            ui.painter().galley(Pos2::new(left, rect.top()), galley, TEXT);
            if let Some(whole) = whole {
                response.on_hover_text(whole);
            }
            if let Some(status) = view.and_then(|v| v.status[i].as_ref()) {
                let at = Pos2::new(left, rect.top() + head + SPACE_1);
                let galley = fitted(ui.painter(), &status.text, Type::Caption.font(), tone(status.tone), room, 1);
                let said = ui.interact(Rect::from_min_size(at, vec2(room, caption)), id.with("status"), Sense::hover());
                if block == 0 {
                    said.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, format!("{name}, {}", status.text)));
                }
                ui.painter().galley(at, galley, tone(status.tone));
            }
        }
    }
    let (rule, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    for block in 0..shape.blocks {
        let left = shape.block_left(rule.left(), block);
        ui.painter().rect_filled(Rect::from_min_size(Pos2::new(left, rule.top()), vec2(shape.block, 1.0)), 0.0, LINE_STRONG);
    }
}

/// The groups, in one block or two, then each device's history. Where the names above don't stay
/// put, `names_at_history` says them again over the history.
pub fn content(ui: &mut Ui, view: &DetailsView, names: &[String; 2], shape: Setout, names_at_history: bool) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let groups: Vec<&DetailGroup> = view.groups.iter().collect();
    let split = if shape.blocks == 2 { FIRST_BLOCK_GROUPS.min(groups.len()) } else { groups.len() };
    let (first, second) = groups.split_at(split);
    if second.is_empty() {
        block(ui, first, names, shape);
    } else {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.allocate_ui_with_layout(vec2(shape.block, 0.0), Layout::top_down(Align::Min), |ui| block(ui, first, names, shape));
            ui.add_space(BLOCK_GAP);
            ui.allocate_ui_with_layout(vec2(shape.block, 0.0), Layout::top_down(Align::Min), |ui| block(ui, second, names, shape));
        });
    }
    ui.add_space(GROUP_GAP);
    ui.label(Type::Label.job("History", TEXT_DIM));
    ui.add_space(SPACE_1);
    if names_at_history && shape.blocks == 1 {
        names_row(ui, None, names, shape, "details history names");
    }
    history(ui, view, names, shape);
    ui.add_space(SPACE_3);
}

fn block(ui: &mut Ui, groups: &[&DetailGroup], names: &[String; 2], shape: Setout) {
    ui.spacing_mut().item_spacing.y = 0.0;
    for group in groups {
        ui.add_space(GROUP_GAP);
        ui.label(Type::Label.job(group.heading, TEXT_DIM));
        ui.add_space(SPACE_1);
        for row in &group.rows {
            value_row(ui, row, names, shape);
        }
    }
}

/// Values wrap onto a second line before they are cut.
const VALUE_ROWS: usize = 2;

fn value_row(ui: &mut Ui, row: &DetailRow, names: &[String; 2], shape: Setout) {
    let (label, column) = (shape.label, shape.column);
    let width = label + 2.0 * column;
    let mut job = Type::Body.job(row.label, TEXT);
    job.wrap.max_width = (label - CELL_GAP).max(1.0);
    let galley = ui.painter().layout_job(job);
    let values: Vec<_> = row
        .cells
        .iter()
        .map(|cell| wrapped_value(ui, &cell.text, tone(cell.tone), column - CELL_GAP))
        .collect();
    let tallest = values.iter().map(|g| g.size().y).fold(galley.size().y, f32::max);
    let height = (tallest + 2.0 * SPACE_1).max(ROW_MIN_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let label_rect = Rect::from_min_size(rect.min, vec2(label, height));
    ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, TEXT);
    if let Some(hint) = row.hint {
        ui.interact(label_rect, Id::new(("details label", row.label)), Sense::hover()).on_hover_text(hint);
    }
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, line(1.0, LINE_FAINT));
    for (i, (cell, galley)) in row.cells.iter().zip(values).enumerate() {
        let left = rect.left() + label + column * i as f32;
        let cell_rect = Rect::from_min_size(Pos2::new(left, rect.top()), vec2(column, height - 1.0));
        let cut = galley.elided;
        ui.painter().galley(Pos2::new(left, cell_rect.center().y - galley.size().y / 2.0), galley, tone(cell.tone));
        value(ui, cell, cut, cell_rect, cell_id(row.label, i), &format!("{}, {}", row.label, names[i]), row.hint);
    }
}

fn wrapped_value(ui: &Ui, text: &str, colour: eframe::egui::Color32, width: f32) -> std::sync::Arc<eframe::egui::Galley> {
    let mut job = Type::Value.job(text, colour);
    job.wrap = eframe::egui::text::TextWrapping {
        max_width: width.max(1.0),
        max_rows: VALUE_ROWS,
        break_anywhere: false,
        overflow_character: Some('\u{2026}'),
    };
    ui.painter().layout_job(job)
}

/// What a value says beyond itself, for its tooltip and for a screen reader.
fn more_about(cell: &DetailCell, cut: bool, hint: Option<&str>) -> Option<String> {
    let lines: Vec<&str> = [cut.then_some(cell.text.as_str()), cell.tooltip.as_deref(), hint].into_iter().flatten().collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// One value's place: named for a screen reader as its row and device, with what its tooltip says
/// as its description, and reachable from the keyboard as the settings table's cells are. The
/// tooltip shows while it has the keyboard, too.
fn value(ui: &mut Ui, cell: &DetailCell, cut: bool, rect: Rect, id: Id, spoken: &str, hint: Option<&str>) {
    let response = ui.interact(rect, id, Sense::focusable_noninteractive());
    response.widget_info(|| WidgetInfo {
        label: Some(spoken.to_string()),
        current_text_value: Some(cell.text.clone()),
        // As the settings table names its cells: a label for the row and device, the value as value.
        ..WidgetInfo::new(WidgetType::Other)
    });
    focus_ring(ui, &response);
    if let Some(more) = more_about(cell, cut, hint) {
        describe(ui, &response, &more);
    }
}

/// `text` as the tooltip on hover or keyboard focus, and as the accessible description.
fn describe(ui: &Ui, response: &Response, text: &str) {
    ui.ctx().accesskit_node_builder(response.id, |node| node.set_description(text.to_string()));
    if response.has_focus() {
        response.show_tooltip_text(text);
    } else {
        response.clone().on_hover_text(text);
    }
}

/// Each device's events, newest first: under the value columns in one block, or each across a block.
fn history(ui: &mut Ui, view: &DetailsView, names: &[String; 2], shape: Setout) {
    let top = ui.cursor().top();
    let left = ui.cursor().left();
    let mut bottom = top;
    let head = ui.fonts(|f| f.row_height(&Type::Head.font()));
    for (i, lines) in view.history.iter().enumerate() {
        let (x, width) = if shape.blocks == 2 {
            (shape.block_left(left, i), shape.block)
        } else {
            (left + shape.label + shape.column * i as f32, shape.column)
        };
        let mut y = top;
        if shape.blocks == 2 {
            // Each list goes across a block, so it says whose it is.
            let galley = fitted(ui.painter(), &names[i].to_uppercase(), Type::Head.font(), TEXT, width, 1);
            ui.painter().galley(Pos2::new(x, y), galley, TEXT);
            y += head + SPACE_1;
        }
        if lines.is_empty() {
            let galley = fitted(ui.painter(), "Nothing yet", Type::Body.font(), TEXT_DIM, width - CELL_GAP, 1);
            ui.painter().galley(Pos2::new(x, y + SPACE_1), galley.clone(), TEXT_DIM);
            y += galley.size().y + 2.0 * SPACE_1;
        }
        for (n, entry) in lines.iter().enumerate() {
            y = history_line(ui, entry, Pos2::new(x, y), width, Id::new(("details history", i, n)), &names[i]);
        }
        bottom = bottom.max(y);
    }
    let width = shape.block_left(0.0, shape.blocks - 1) + shape.block;
    ui.allocate_exact_size(vec2(width, bottom - top), Sense::hover());
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
    let response = ui.interact(rect, id, Sense::focusable_noninteractive());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, format!("{device}, {}, {}", entry.time, entry.text)));
    focus_ring(ui, &response);
    describe(ui, &response, &entry.tooltip);
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

    #[test]
    fn a_wide_window_sets_the_groups_out_in_two_blocks() {
        let wide = shape(1240.0);
        assert_eq!((wide.blocks, wide.block), (2, 604.0));
        assert_eq!(wide.label + 2.0 * wide.column, wide.block);
        assert!(wide.column >= 210.0, "{wide:?}");
        let narrow = shape(720.0);
        assert_eq!((narrow.blocks, narrow.label), (1, 201.6));
        assert_eq!(shape(1060.0).blocks, 1);
        assert!(shape(1060.0).column <= MOST_COLUMN);
    }
}
