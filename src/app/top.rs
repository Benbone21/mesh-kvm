//! The top of the window: the status bar, the update line, notices and the stats row. Each draws
//! what the view model says and hands back what the person asked for.

use super::model::Snapshot;
use super::motion;
use super::sparkline::{Sparkline, Sparklines};
use super::theme::{
    line, tone, Type, ACCENT, DANGER, LINE, SPACE_1, SPACE_2, SPACE_3, SPACE_5, SPARKLINE_SIZE, SPARKLINE_STROKE,
    STATUS_BUTTON_HEIGHT, SURFACE, TEXT, TEXT_DIM,
};
use super::updates::{notes_url, Banner, UpdateButton};
use super::view::{fit_name, stat_value, Notice, Stat, StatusLine, STATS};
use super::widgets::{flat_button, flat_button_width, status_square, ButtonKind};
use eframe::egui::{
    text::TextWrapping, vec2, Align, Color32, Frame, Layout, Margin, Pos2, Rect, RichText, Sense, Shape,
    StrokeKind, Ui, Vec2, WidgetInfo, WidgetType,
};

pub const RELEASE_NOTES: &str = "Release notes";
const RELEASE_ALL: &str = "Release all";
const DISMISS: &str = "Dismiss";
/// The space between the status square and the words after it.
const SQUARE_GAP: f32 = 10.0;
const CELL_MARGIN: Margin = Margin { left: SPACE_5 as i8, right: SPACE_5 as i8, top: 10, bottom: SPACE_3 as i8 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopRequest {
    Switch,
    ReleaseAll,
    Update(UpdateButton),
    DismissError,
}

fn button_size() -> Vec2 {
    vec2(0.0, STATUS_BUTTON_HEIGHT)
}

/// The square, the sentence, and Switch and Release all at the end. `switch_name` is the name of
/// the device in `line.switch_to`.
pub fn status_bar(ui: &mut Ui, line: &StatusLine, square: Color32, switch_name: Option<&str>) -> Option<TopRequest> {
    let mut request = None;
    let switch = line.switch_to.map(|_| {
        let full = switch_name.unwrap_or("the other device");
        (format!("Switch to {}", fit_name(full).0), format!("Switch keyboard and mouse to {full}."))
    });
    let gap = SPACE_2;
    let mut buttons = 0.0;
    if let Some((label, _)) = &switch {
        buttons += flat_button_width(ui, label, ButtonKind::Primary) + gap;
    }
    if line.show_release {
        buttons += flat_button_width(ui, RELEASE_ALL, ButtonKind::Danger) + gap;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.set_min_height(STATUS_BUTTON_HEIGHT);
        status_square(ui, square);
        ui.add_space(SQUARE_GAP - gap);
        let room = (ui.available_width() - buttons).max(0.0);
        // A name is cut at 24 characters, with the whole of it in a tooltip.
        let (rest, full) = if line.rest_strong { fit_name(&line.rest) } else { (line.rest.clone(), None) };
        let mut job = Type::Status.job(&format!("{} ", line.lead), TEXT_DIM);
        let rest_job = (if line.rest_strong { Type::StatusStrong } else { Type::Status }).job(&rest, TEXT);
        job.append(&rest_job.text, 0.0, rest_job.sections[0].format.clone());
        job.wrap = TextWrapping { max_width: room.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let galley = ui.painter().layout_job(job);
        let (rect, response) = ui.allocate_exact_size(vec2(room, galley.size().y), Sense::hover());
        let elided = galley.elided;
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, TEXT);
        let whole = if elided { Some(format!("{} {}", line.lead, line.rest)) } else { full };
        if let Some(whole) = whole {
            response.on_hover_text(whole);
        }
        if let Some((label, hint)) = &switch {
            if flat_button(ui, label, ButtonKind::Primary, button_size(), true).on_hover_text(hint).clicked() {
                request = Some(TopRequest::Switch);
            }
        }
        if line.show_release {
            let release = flat_button(ui, RELEASE_ALL, ButtonKind::Danger, button_size(), true);
            if release.on_hover_text("Gives each device back its own keyboard and mouse.").clicked() {
                request = Some(TopRequest::ReleaseAll);
            }
        }
    });
    request
}

/// What an update is doing, with the release notes after the words and the button at the end.
pub fn update_line(ui: &mut Ui, banner: &Banner) -> Option<TopRequest> {
    let mut request = None;
    let gap = SPACE_3;
    let button = banner.button.map_or(0.0, |b| flat_button_width(ui, b.label(), ButtonKind::Primary) + gap);
    let notes = banner.notes.as_ref().map_or(0.0, |_| {
        ui.fonts(|f| f.layout_no_wrap(RELEASE_NOTES.into(), Type::Body.font(), ACCENT)).size().x + gap
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.set_min_height(STATUS_BUTTON_HEIGHT);
        if banner.failed {
            status_square(ui, DANGER);
        }
        let room = (ui.available_width() - button - notes).max(0.0);
        let natural = |text: &str, kind: Type| ui.fonts(|f| f.layout_job(kind.job(text, TEXT))).size().x;
        let wanted = banner.detail.as_deref().map_or(0.0, |d| natural(d, Type::Caption)).max(natural(&banner.text, Type::Body));
        let width = wanted.min(room).ceil();
        ui.allocate_ui(vec2(width, 0.0), |ui| {
            ui.set_width(width);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(Type::Body.job(&banner.text, TEXT));
                if let Some(detail) = &banner.detail {
                    ui.label(Type::Caption.job(detail, TEXT_DIM));
                }
            });
        });
        if let Some(version) = &banner.notes {
            ui.hyperlink_to(RichText::new(RELEASE_NOTES).font(Type::Body.font()), notes_url(version));
        }
        if let Some(b) = banner.button {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if flat_button(ui, b.label(), ButtonKind::Primary, button_size(), true).clicked() {
                    request = Some(TopRequest::Update(b));
                }
            });
        }
    });
    request
}

/// One thing the window has to say, with its square, and Dismiss if it can be dismissed.
pub fn notice_line(ui: &mut Ui, notice: &Notice) -> Option<TopRequest> {
    let mut request = None;
    let gap = SPACE_3;
    let dismiss = if notice.dismiss { flat_button_width(ui, DISMISS, ButtonKind::Secondary) + gap } else { 0.0 };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.set_min_height(if notice.dismiss { STATUS_BUTTON_HEIGHT } else { 0.0 });
        status_square(ui, tone(notice.tone));
        let room = (ui.available_width() - dismiss).max(0.0);
        ui.allocate_ui(vec2(room, 0.0), |ui| {
            ui.set_width(room);
            let said = ui.label(Type::Body.job(&notice.text, TEXT));
            motion::live(ui.ctx(), said.id);
        });
        if notice.dismiss && flat_button(ui, DISMISS, ButtonKind::Secondary, button_size(), true).clicked() {
            request = Some(TopRequest::DismissError);
        }
    });
    request
}

/// The five figures, `rows[0]` of them in equal columns and then `rows[1]`, each with its label
/// above and its last minute below.
pub fn stats_row(ui: &mut Ui, snapshot: &Snapshot, lines: &Sparklines, rows: [usize; 2]) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    let mut stats = STATS.iter().copied();
    for (index, count) in rows.into_iter().filter(|c| *c > 0).enumerate() {
        let row: Vec<Stat> = stats.by_ref().take(count).collect();
        let width = ui.available_width();
        if index > 0 {
            let (rule, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
            ui.painter().rect_filled(rule, 0.0, LINE);
        }
        let cell = width / row.len() as f32;
        let top = ui.cursor().top();
        let left = ui.cursor().left();
        ui.horizontal_top(|ui| {
            for stat in &row {
                ui.allocate_ui_with_layout(vec2(cell, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(cell);
                    stat_cell(ui, *stat, snapshot, lines.get(*stat));
                });
            }
        });
        let bottom = ui.min_rect().bottom();
        for i in 1..row.len() {
            ui.painter().vline(left + cell * i as f32, top..=bottom, line(1.0, LINE));
        }
    }
}

fn stat_cell(ui: &mut Ui, stat: Stat, snapshot: &Snapshot, sparkline: &Sparkline) {
    let value = stat_value(stat, snapshot);
    Frame::new().inner_margin(CELL_MARGIN).show(ui, |ui| {
        let inner = ui.available_width().max(1.0);
        ui.label(Type::Label.job(stat.label(), TEXT_DIM));
        // A long word in a narrow column is set smaller rather than cut.
        let colour = tone(value.tone);
        let full = Type::Stat.job(&value.text, colour);
        let height = ui.fonts(|f| f.row_height(&Type::Stat.font()));
        let natural = ui.fonts(|f| f.layout_job(full.clone())).size().x;
        let mut job = full;
        if natural > inner {
            job.sections[0].format.font_id.size *= inner / natural;
        }
        let galley = ui.painter().layout_job(job);
        let (rect, response) = ui.allocate_exact_size(vec2(inner, height), Sense::hover());
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &value.spoken));
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, colour);
        ui.add_space(SPACE_1);
        let size = vec2(SPARKLINE_SIZE[0].min(inner), SPARKLINE_SIZE[1]);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        paint_sparkline(ui, rect, sparkline, stat.floor());
    });
}

/// The line, hidden until it has a point, and with its last point hollow while it is frozen.
fn paint_sparkline(ui: &Ui, rect: Rect, sparkline: &Sparkline, floor: f32) {
    if sparkline.points().is_empty() {
        return;
    }
    let points: Vec<Pos2> = sparkline.path(rect.size().into(), floor).into_iter().map(|[x, y]| rect.min + vec2(x, y)).collect();
    let last = points[points.len() - 1];
    let painter = ui.painter();
    if points.len() > 1 {
        painter.add(Shape::line(points, line(SPARKLINE_STROKE, ACCENT)));
    } else {
        painter.rect_filled(Rect::from_center_size(last, Vec2::splat(SPARKLINE_STROKE)), 0.0, ACCENT);
    }
    if sparkline.frozen() {
        let hollow = Rect::from_center_size(last, Vec2::splat(3.0));
        painter.rect_filled(hollow, 0.0, SURFACE);
        painter.rect_stroke(hollow, 0.0, line(1.0, ACCENT), StrokeKind::Outside);
    }
}
