//! The settings table: a header naming both devices, then each group's rows with one cell for
//! each device. It draws what `rows` worked out and hands back what the person asked for.

use super::model::{Control, Device};
use super::rows::{ActionKind, Cell, CellKind, ChoiceValue, GroupRows, Note, Pick, Row, Setting, DROPDOWN};
use super::theme::{
    line, tone, Type, ACCENT, BG, DANGER, DEVICE_COLUMN, LINE_FAINT, LINE_STRONG, RAISED, ROW_MIN_HEIGHT, SPACE_1, SPACE_2,
    TEXT, TEXT_DIM,
};
use super::view::fit_name;
use super::widgets::focus_ring;
use crate::settings::MAX_DEVICE_NAME;
use eframe::egui::{
    self, vec2, Align, Color32, CursorIcon, Id, Layout, Margin, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, UiBuilder,
};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

/// How long a new name stays on show before the polled one takes over.
const NAME_HOLD: Duration = Duration::from_secs(3);
/// The space between the header and its rule.
const HEADER_GAP: f32 = 6.0;
/// The space above a group's heading.
const GROUP_GAP: f32 = 10.0;
const LEVEL_HEIGHT: f32 = 4.0;
/// Kept clear at the end of a cell, so neighbouring values never touch.
const CELL_GAP: f32 = SPACE_2;
/// A value the service hasn't confirmed yet is drawn this faint.
const SAVING_OPACITY: f32 = 0.6;

/// What the person asked for in the table, for the window to send.
#[derive(Debug, Clone, PartialEq)]
pub enum TableRequest {
    /// A setting on one device. Every `Setting::Device` cell, toggle or number, sends this.
    Set(Device, Control, f64),
    /// A number that isn't a device's own setting: only `EdgePush`.
    Number(Setting, f64),
    /// A switch that isn't a device's own setting: `AutoGameLock` or `ManualGameLock`.
    Toggle(Setting),
    Pick(Device, Pick, ChoiceValue),
    Rename(Device, String),
    Action(Device, ActionKind),
    /// The monitor whose cell is under the pointer, to light up on the canvas.
    Hover(Option<(Device, String)>),
}

/// A device's name while it is being changed.
#[derive(Default)]
struct NameEdit {
    /// What's in the box. `None` shows the polled name.
    text: Option<String>,
    sent_at: Option<Instant>,
    /// The box is open for typing.
    editing: bool,
}

/// What the table keeps between frames.
#[derive(Default)]
pub struct Table {
    names: HashMap<Device, NameEdit>,
}

/// The id of a cell's control, from its row's label and its column (0 is this device).
pub fn cell_id(label: &str, column: usize) -> Id {
    Id::new(("settings cell", label, column))
}

/// What one frame of the table is drawn with.
struct Pass<'a> {
    label_width: f32,
    selected: Option<&'a (Device, String)>,
    scroll_to_selected: bool,
    now: Instant,
    requests: Vec<TableRequest>,
    hovered: Option<(Device, String)>,
    scrolled: bool,
}

impl Table {
    /// The header and every group. `columns` is each column's device and name, this device first.
    /// A cell about the canvas's `selected` monitor is lit, and brought into view on
    /// `scroll_to_selected`.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        groups: &[GroupRows],
        columns: [(Device, String); 2],
        label_width: f32,
        selected: Option<&(Device, String)>,
        scroll_to_selected: bool,
        now: Instant,
    ) -> Vec<TableRequest> {
        // The label takes what the two device columns leave, however the window is divided.
        let label_width = label_width.min(ui.available_width() - 2.0 * DEVICE_COLUMN).max(SPACE_2 * 4.0);
        let mut frame =
            Pass { label_width, selected, scroll_to_selected, now, requests: Vec::new(), hovered: None, scrolled: false };
        ui.spacing_mut().item_spacing.y = 0.0;
        header(ui, &columns, label_width);
        let devices = columns.map(|(device, _)| device);
        for group in groups.iter().filter(|g| !g.rows.is_empty() || !g.notes.is_empty()) {
            ui.add_space(GROUP_GAP);
            ui.label(Type::Label.job(group.group.heading(), TEXT_DIM));
            ui.add_space(SPACE_1);
            for row in &group.rows {
                self.row(ui, row, devices, &mut frame);
            }
            for note in &group.notes {
                note_line(ui, note);
            }
        }
        frame.requests.push(TableRequest::Hover(frame.hovered));
        frame.requests
    }

    fn row(&mut self, ui: &mut Ui, row: &Row, devices: [Device; 2], frame: &mut Pass) {
        let width = frame.label_width + 2.0 * DEVICE_COLUMN;
        let label = ui.painter().layout_job(wrapped(Type::Body.job(&row.label, TEXT), frame.label_width - CELL_GAP));
        let height = (label.size().y + 2.0 * SPACE_1).max(ROW_MIN_HEIGHT);
        let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let background = ui.painter().add(Shape::Noop);
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - label.size().y / 2.0), label, TEXT);
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, line(1.0, LINE_FAINT));
        let mut lit = false;
        for (column, cell) in row.cells.iter().enumerate() {
            let left = rect.left() + frame.label_width + DEVICE_COLUMN * column as f32;
            let cell_rect = Rect::from_min_size(Pos2::new(left, rect.top()), vec2(DEVICE_COLUMN, height - 1.0));
            let id = cell_id(&row.label, column);
            lit |= self.cell(ui, cell, cell_rect, id, devices[column], frame);
        }
        if lit {
            ui.painter().set(background, Shape::rect_filled(rect, 0.0, RAISED));
        }
        if let Some(error) = &row.error {
            ui.add_space(SPACE_1);
            ui.label(wrapped(Type::Caption.job(error, DANGER), width));
            ui.add_space(SPACE_1);
        }
    }

    /// Draws one cell. True when its row should be lit, as a clickable cell is under the pointer.
    fn cell(&mut self, ui: &mut Ui, cell: &Cell, rect: Rect, id: Id, device: Device, frame: &mut Pass) -> bool {
        if cell.kind == CellKind::Blank {
            return false;
        }
        let is_selected = cell.monitor.is_some() && cell.monitor.as_ref() == frame.selected;
        if is_selected {
            ui.painter().rect_filled(rect, 0.0, RAISED);
            if frame.scroll_to_selected && !frame.scrolled {
                frame.scrolled = true;
                ui.scroll_to_rect(rect, Some(Align::Center));
            }
        }
        if cell.monitor.is_some() && ui.rect_contains_pointer(rect) {
            frame.hovered = cell.monitor.clone();
        }
        let mut colour = tone(cell.tone);
        if cell.saving {
            colour = colour.gamma_multiply(SAVING_OPACITY);
        }
        let room = DEVICE_COLUMN - CELL_GAP;
        // A read-only cell and one waiting on the service only say what they are.
        let changeable = cell.read_only.is_none();
        let clickable = changeable && !cell.saving;
        match &cell.kind {
            CellKind::Blank => false,
            CellKind::Toggle(setting) if clickable => {
                let response = clickable_text(ui, cell, rect, id, colour);
                if response.clicked() {
                    let on = cell.is_on();
                    frame.requests.push(match setting {
                        Setting::Device(control) => TableRequest::Set(device, *control, if on { 0.0 } else { 1.0 }),
                        other => TableRequest::Toggle(*other),
                    });
                }
                response.hovered()
            }
            CellKind::Action(kind) if clickable => {
                let response = clickable_text(ui, cell, rect, id, colour);
                if response.clicked() {
                    frame.requests.push(TableRequest::Action(device, *kind));
                }
                response.hovered()
            }
            CellKind::Number { setting, value, range, step } if changeable => {
                // A number stays draggable while it saves, as dragging sets it again each frame.
                if let Some(value) = number(ui, cell, rect, id, colour, *value, range.clone(), *step) {
                    frame.requests.push(match setting {
                        Setting::Device(control) => TableRequest::Set(device, *control, value),
                        other => TableRequest::Number(*other, value),
                    });
                }
                false
            }
            CellKind::Dropdown { pick, options } if clickable => {
                let (shown, cut) = fitted_value(ui, &cell.value, room);
                let mut chosen = None;
                let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
                flat_widgets(&mut child);
                child.spacing_mut().icon_width = 0.0;
                child.spacing_mut().icon_spacing = 0.0;
                child.spacing_mut().button_padding = vec2(0.0, 0.0);
                let combo = egui::ComboBox::from_id_salt(id)
                    .selected_text(egui::RichText::new(shown).font(Type::Value.font()).color(colour))
                    .width(room)
                    .icon(|_, _, _, _, _| {})
                    .show_ui(&mut child, |ui| {
                        for choice in options {
                            if ui.selectable_label(choice.selected, &choice.label).clicked() && !choice.selected {
                                chosen = Some(choice.value.clone());
                            }
                        }
                    });
                with_hover_text(combo.response, cell, cut);
                if let Some(value) = chosen {
                    frame.requests.push(TableRequest::Pick(device, pick.clone(), value));
                }
                false
            }
            CellKind::Name if clickable => self.name(ui, cell, rect, id, device, frame.now, &mut frame.requests),
            CellKind::Level(percent) => {
                let response = ui.interact(rect, id, Sense::hover());
                let bar = Rect::from_min_size(
                    Pos2::new(rect.left(), rect.center().y - LEVEL_HEIGHT / 2.0),
                    vec2(room, LEVEL_HEIGHT),
                );
                ui.painter().rect_filled(bar, 0.0, BG);
                let filled = Rect::from_min_size(bar.min, vec2(room * *percent as f32 / 100.0, LEVEL_HEIGHT));
                ui.painter().rect_filled(filled, 0.0, ACCENT);
                with_hover_text(response, cell, Some(cell.value.clone()));
                false
            }
            // Plain text, and anything that can't be changed right now.
            _ => {
                let response = ui.interact(rect, id, Sense::hover());
                let cut = paint_value(ui, cell, rect, colour);
                with_hover_text(response, cell, cut);
                false
            }
        }
    }

    /// The device's name, which opens for typing when clicked. True while the pointer is on it.
    #[allow(clippy::too_many_arguments)]
    fn name(&mut self, ui: &mut Ui, cell: &Cell, rect: Rect, id: Id, device: Device, now: Instant, requests: &mut Vec<TableRequest>) -> bool {
        let polled = &cell.value;
        let edit = self.names.entry(device).or_default();
        let settled = edit.sent_at.is_none_or(|at| now.saturating_duration_since(at) >= NAME_HOLD);
        if !edit.editing && settled && edit.sent_at.is_some() {
            *edit = NameEdit::default();
        }
        let shown = edit.text.clone().unwrap_or_else(|| polled.clone());
        let opened = !edit.editing;
        if opened {
            let shown_cell = Cell { value: shown.clone(), ..cell.clone() };
            let response = clickable_text(ui, &shown_cell, rect, id, tone(cell.tone)).on_hover_cursor(CursorIcon::Text);
            if !response.clicked() {
                return response.hovered();
            }
            edit.editing = true;
            edit.text = Some(shown.clone());
        }
        let mut text = shown;
        let field = egui::TextEdit::singleline(&mut text)
            .id(id.with("box"))
            .font(Type::Value.font())
            .text_color(TEXT)
            .frame(false)
            .desired_width(DEVICE_COLUMN - CELL_GAP)
            .char_limit(MAX_DEVICE_NAME)
            .margin(Margin::ZERO);
        let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
        let response = child.add(field);
        if opened {
            response.request_focus();
        }
        let mark = if response.has_focus() { ACCENT } else { LINE_STRONG };
        ui.painter().hline(response.rect.x_range(), response.rect.bottom() + 1.0, line(1.0, mark));
        if response.changed() {
            edit.text = Some(text.clone());
        }
        // Done when the box lets go of the keyboard, or never got it.
        if response.lost_focus() || (!opened && !response.has_focus()) {
            edit.editing = false;
            let typed = text.trim().to_string();
            if typed != *polled {
                requests.push(TableRequest::Rename(device, typed.clone()));
                edit.text = Some(typed);
                edit.sent_at = Some(now);
            } else {
                edit.text = None;
            }
        }
        false
    }
}

/// SETTING and both device names, upper-cased and cut to fit, above a strong rule.
fn header(ui: &mut Ui, columns: &[(Device, String); 2], label_width: f32) {
    let height = ui.fonts(|f| f.row_height(&Type::Head.font()));
    let width = label_width + 2.0 * DEVICE_COLUMN;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height + HEADER_GAP), Sense::hover());
    let setting = ui.painter().layout_job(Type::Head.job("SETTING", TEXT_DIM));
    ui.painter().galley(rect.min, setting, TEXT_DIM);
    for (column, (_, name)) in columns.iter().enumerate() {
        let (short, mut full) = fit_name(name);
        let short = short.to_uppercase();
        let room = DEVICE_COLUMN - CELL_GAP;
        let shown = match cut_to(ui, &short, Type::Head, room) {
            Some(cut) => {
                full = Some(name.clone());
                cut
            }
            None => short,
        };
        let at = Pos2::new(rect.left() + label_width + DEVICE_COLUMN * column as f32, rect.top());
        let galley = ui.painter().layout_job(Type::Head.job(&shown, TEXT));
        let response = ui.interact(Rect::from_min_size(at, vec2(room, height)), Id::new(("settings header", column)), Sense::hover());
        ui.painter().galley(at, galley, TEXT);
        if let Some(full) = full {
            response.on_hover_text(full);
        }
    }
    let (rule, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rule, 0.0, LINE_STRONG);
}

fn note_line(ui: &mut Ui, note: &Note) {
    ui.add_space(SPACE_1);
    let width = ui.available_width();
    let response = ui.label(wrapped(Type::Caption.job(&note.text, tone(note.tone)), width));
    if let Some(tooltip) = &note.tooltip {
        response.on_hover_text(tooltip);
    }
}

fn wrapped(mut job: egui::text::LayoutJob, width: f32) -> egui::text::LayoutJob {
    job.wrap.max_width = width.max(1.0);
    job
}

/// A cell that does something when clicked: its text, the pointing hand, and its row lit on hover.
fn clickable_text(ui: &mut Ui, cell: &Cell, rect: Rect, id: Id, colour: Color32) -> Response {
    let response = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let cut = paint_value(ui, cell, rect, colour);
    focus_ring(ui, &response);
    with_hover_text(response, cell, cut)
}

/// Paints the cell's value cut to the column. The whole value comes back when it had to be cut.
fn paint_value(ui: &Ui, cell: &Cell, rect: Rect, colour: Color32) -> Option<String> {
    let (shown, cut) = fitted_value(ui, &cell.value, DEVICE_COLUMN - CELL_GAP);
    let galley = ui.painter().layout_job(Type::Value.job(&shown, colour));
    ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, colour);
    cut
}

/// The value as it fits in `room`, and the whole of it when it didn't. A dropdown keeps its `▾`.
fn fitted_value(ui: &Ui, value: &str, room: f32) -> (String, Option<String>) {
    let (stem, arrow) = match value.strip_suffix(DROPDOWN) {
        Some(stem) => (stem, DROPDOWN),
        None => (value, ""),
    };
    let arrow_width = if arrow.is_empty() { 0.0 } else { text_width(ui, arrow, Type::Value) };
    match cut_to(ui, stem, Type::Value, room - arrow_width) {
        Some(cut) => (format!("{cut}{arrow}"), Some(stem.to_string())),
        None => (value.to_string(), None),
    }
}

fn text_width(ui: &Ui, text: &str, kind: Type) -> f32 {
    ui.fonts(|f| f.layout_job(kind.job(text, TEXT))).size().x
}

/// `text` cut with `…` to fit `width`, or `None` when it fits as it is.
fn cut_to(ui: &Ui, text: &str, kind: Type, width: f32) -> Option<String> {
    if text_width(ui, text, kind) <= width {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len())
        .rev()
        .map(|keep| format!("{}\u{2026}", chars[..keep].iter().collect::<String>().trim_end()))
        .find(|cut| text_width(ui, cut, kind) <= width)
        .or_else(|| Some("\u{2026}".into()))
}

/// Gives the response what there is to say on hover: why it can't change, what it is, and the
/// whole value when it was cut.
fn with_hover_text(response: Response, cell: &Cell, whole: Option<String>) -> Response {
    let lines: Vec<&str> = [cell.read_only.as_deref(), cell.tooltip.as_deref(), whole.as_deref()].into_iter().flatten().collect();
    let mut unique: Vec<&str> = Vec::new();
    for text in lines {
        if !unique.contains(&text) {
            unique.push(text);
        }
    }
    if unique.is_empty() {
        response
    } else {
        response.on_hover_text(unique.join("\n"))
    }
}

/// Widgets in a cell are drawn without a box until the pointer is on them.
fn flat_widgets(ui: &mut Ui) {
    let widgets = &mut ui.visuals_mut().widgets;
    for state in [&mut widgets.inactive, &mut widgets.noninteractive] {
        state.weak_bg_fill = Color32::TRANSPARENT;
        state.bg_fill = Color32::TRANSPARENT;
        state.bg_stroke = Stroke::NONE;
    }
    for state in [&mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
        state.weak_bg_fill = RAISED;
        state.bg_fill = RAISED;
        state.bg_stroke = line(1.0, LINE_STRONG);
    }
}

/// A number to drag or type, shown as the cell's text is. The new value, when it moved.
#[allow(clippy::too_many_arguments)]
fn number(ui: &mut Ui, cell: &Cell, rect: Rect, id: Id, colour: Color32, value: f64, range: RangeInclusive<f64>, step: f64) -> Option<f64> {
    let (decimals, suffix) = number_style(&cell.value);
    let mut edited = value;
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)).id_salt(id));
    flat_widgets(&mut child);
    child.spacing_mut().button_padding = vec2(SPACE_1, 0.0);
    child.spacing_mut().interact_size.x = 0.0;
    child.style_mut().override_font_id = Some(Type::Value.font());
    child.visuals_mut().override_text_color = Some(colour);
    let unit = suffix.clone();
    let drag = egui::DragValue::new(&mut edited)
        .range(range)
        // A quarter of a step for each point moved, so the whole range takes a good swipe.
        .speed(step / 4.0)
        .clamp_existing_to_range(false)
        .min_decimals(decimals)
        .max_decimals(decimals)
        .custom_formatter(move |v, _| format!("{v:.decimals$}{suffix}"))
        .custom_parser(move |text| text.trim().trim_end_matches(unit.as_str()).trim().parse().ok());
    let response = child.add(drag);
    let changed = response.changed() && edited != value;
    with_hover_text(response, cell, None);
    changed.then_some(edited)
}

/// How many decimals a number cell shows, and what follows its figure: `0.35×` is `(2, "×")`.
fn number_style(text: &str) -> (usize, String) {
    let figure: String = text.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
    let decimals = figure.split_once('.').map_or(0, |(_, after)| after.len());
    (decimals, text[figure.len()..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::rows::Group;
    use crate::app::theme::Tone;
    use eframe::egui::{Event, PointerButton, RawInput};

    fn cell(value: &str, kind: CellKind) -> Cell {
        Cell { value: value.into(), tone: Tone::Text, kind, read_only: None, tooltip: None, saving: false, monitor: None }
    }

    /// The table with one row of these two cells, clicked in the middle of `column`'s cell.
    fn click(cells: [Cell; 2], column: usize) -> Vec<TableRequest> {
        let row = Row { label: "Row".into(), cells, error: None };
        let groups = [GroupRows { group: Group::Input, rows: vec![row], notes: Vec::new() }];
        let columns = [(Device::Debian, "Debian".to_string()), (Device::Windows, "Laptop".to_string())];
        let (ctx, mut table) = (egui::Context::default(), Table::default());
        crate::app::theme::apply(&ctx);
        let mut requests = Vec::new();
        let mut run = |events: Vec<Event>| {
            let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(700.0, 400.0))), events, ..Default::default() };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    requests.extend(table.show(ui, &groups, columns.clone(), 400.0, None, false, Instant::now()));
                });
            });
        };
        run(vec![]);
        let at = ctx.read_response(cell_id("Row", column)).unwrap().rect.center();
        let press = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Default::default() };
        run(vec![Event::PointerMoved(at)]);
        run(vec![press(true)]);
        run(vec![press(false)]);
        requests.retain(|r| !matches!(r, TableRequest::Hover(_)));
        requests
    }

    #[test]
    fn a_click_on_a_toggle_or_action_asks_for_it_on_that_columns_device() {
        let on = cell("ON", CellKind::Toggle(Setting::Device(Control::MouseSharing)));
        assert_eq!(click([on.clone(), on.clone()], 1), [TableRequest::Set(Device::Windows, Control::MouseSharing, 0.0)]);
        let lock = cell("OFF", CellKind::Toggle(Setting::ManualGameLock));
        assert_eq!(click([lock, Cell::blank()], 0), [TableRequest::Toggle(Setting::ManualGameLock)]);
        let unpair = cell("Unpair", CellKind::Action(ActionKind::Unpair));
        assert_eq!(click([Cell::blank(), unpair], 1), [TableRequest::Action(Device::Windows, ActionKind::Unpair)]);
    }

    #[test]
    fn a_saving_or_read_only_cell_ignores_clicks() {
        let saving = Cell { saving: true, ..cell("Unpairing\u{2026}", CellKind::Action(ActionKind::Unpair)) };
        assert_eq!(click([Cell::blank(), saving], 1), []);
        let main = Cell { saving: true, ..cell("Changing\u{2026}", CellKind::Action(ActionKind::MakeMain)) };
        assert_eq!(click([main, Cell::blank()], 0), []);
        let toggle = Cell { saving: true, ..cell("OFF", CellKind::Toggle(Setting::Device(Control::Autostart))) };
        assert_eq!(click([toggle, Cell::blank()], 0), []);
        let locked = Cell { read_only: Some("Only Laptop can.".into()), ..cell("ON", CellKind::Toggle(Setting::Device(Control::Autostart))) };
        assert_eq!(click([locked, Cell::blank()], 0), []);
    }

    #[test]
    fn a_number_cell_keeps_its_decimals_and_unit() {
        assert_eq!(number_style("0.35\u{d7}"), (2, "\u{d7}".into()));
        assert_eq!(number_style("80%"), (0, "%".into()));
        assert_eq!(number_style("55"), (0, String::new()));
    }
}
