//! The arrangement: a desk mat with every monitor of both devices standing on it, to scale. The
//! screens of the device that has the keyboard and mouse are lit, so who has control reads from
//! across the room. Decisions live in `model`; this file is painting and the pointer.

use super::model::{self, Device, Display, LayoutConfig, LayoutDraft, MapView, Snapshot};
use super::theme::{self, line, legacy::{AMBER, BEZEL, DIM, GLASS, INK, MAT, ON_SCREEN, RED}};
use super::widgets::fitted;
use eframe::egui::{
    self, epaint::Vertex, vec2, Color32, CornerRadius, CursorIcon, FontId, Mesh, Painter, Pos2, Rect, Sense, Shape,
    StrokeKind, Ui,
};
use std::time::Instant;

/// Room above and below the monitors for each device's name.
const TAG_ROOM: f32 = 30.0;
const SIDE_ROOM: f32 = 10.0;
const HINT_GAP: f32 = 8.0;
/// How near, on screen, a dragged device has to be to line up with a monitor.
const MAGNET_POINTS: f32 = 14.0;
const SMALLEST_NAME: f32 = 11.0;
const LARGEST_NAME: f32 = 22.0;
/// The unlit screens are the lit colour with this much of the mat in it.
const UNLIT_MIX: f32 = 0.74;

/// One monitor on the map: its device, and its name in the layout.
pub type MonitorKey = (Device, String);

/// A device being dragged. The view stays as it was when the drag began, so the map doesn't
/// rescale under the pointer.
struct Drag {
    view: MapView,
    device: Device,
    /// Where it was grabbed, from the corner of its box, in layout units.
    grab: (f32, f32),
    from: (i32, i32),
}

#[derive(Default)]
pub struct Canvas {
    drag: Option<Drag>,
    /// The monitor clicked on the map or hovered in the controls, for each to light up the other.
    pub selected: Option<MonitorKey>,
    pub hovered_in_controls: Option<MonitorKey>,
    /// True for one frame after a click, so the controls can bring that monitor into view.
    pub just_selected: bool,
}

pub fn monitor_id(device: Device, index: usize) -> egui::Id {
    egui::Id::new(("monitor on the map", device, index))
}

/// What state a device's screens are in.
#[derive(Clone, Copy, PartialEq)]
enum Screens {
    Lit,
    Unlit,
    Off,
}

impl Canvas {
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn show(&mut self, ui: &mut Ui, draft: &mut LayoutDraft, snapshot: &Snapshot, now: Instant) {
        let (mat, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        ui.painter().rect_filled(mat, theme::legacy::RADIUS, MAT);
        ui.painter().rect_stroke(mat, theme::legacy::RADIUS, line(1.0, theme::mix(MAT, Color32::BLACK, 0.35)), StrokeKind::Inside);
        self.just_selected = false;

        let Some(state) = &draft.state else {
            let text = "Your screens show up here once mesh-kvm is running on this device.";
            let galley = ui.painter().layout(text.into(), FontId::proportional(theme::legacy::BODY_SIZE), DIM, mat.width() - 48.0);
            ui.painter().galley(mat.center() - galley.size() / 2.0, galley, DIM);
            return;
        };
        let cfg = state.config.clone();
        let hint = "Drag any screen to move its whole device. The mouse crosses at the amber line.";
        let hint_width = mat.width() - 2.0 * (SIDE_ROOM + 4.0);
        let hint = fitted(ui.painter(), hint, FontId::proportional(theme::legacy::SMALL_SIZE), DIM, hint_width, 2);
        let inner = Rect::from_min_max(
            mat.min + vec2(SIDE_ROOM, TAG_ROOM),
            mat.max - vec2(SIDE_ROOM, TAG_ROOM + hint.size().y + HINT_GAP),
        );
        let view = self.drag.as_ref().map_or_else(|| model::fit(&cfg, inner.width(), inner.height()), |d| d.view);
        let to_screen = |x: i32, y: i32| {
            let (sx, sy) = view.to_screen(x as f32, y as f32);
            inner.min + vec2(sx, sy)
        };
        let to_rect = |m: &Display| Rect::from_min_size(to_screen(m.x, m.y), vec2(m.w as f32, m.h as f32) * view.scale);
        let painter = ui.painter().with_clip_rect(mat);
        let active = snapshot.active();

        let groups: Vec<(Device, Vec<Display>)> = Device::BOTH.into_iter().map(|d| (d, cfg.rects(d))).collect();
        let mut group_boxes = Vec::new();
        for (device, rects) in &groups {
            let screens = screens_state(snapshot, *device, active);
            let boxed = rects.iter().map(&to_rect).reduce(|a, b| a.union(b)).unwrap_or(Rect::NOTHING);
            group_boxes.push((*device, boxed));
            let held = self.drag.as_ref().is_some_and(|d| d.device == *device);
            let group_hovered = rects.iter().enumerate().any(|(i, _)| ui.ctx().read_response(monitor_id(*device, i)).is_some_and(|r| r.hovered()));
            if held || group_hovered {
                painter.rect_stroke(boxed.expand(4.0), theme::legacy::RADIUS, line(1.5, if held { AMBER } else { INK }), StrokeKind::Outside);
            }
            for m in rects {
                let key = (*device, m.name.clone());
                let picked = self.selected.as_ref() == Some(&key) || self.hovered_in_controls.as_ref() == Some(&key);
                let display = model::display_for(snapshot.displays(*device), m);
                let label = model::monitor_label(m, display);
                paint_monitor(&painter, to_rect(m), &label, *device, screens, picked);
            }
        }
        paint_tags(&painter, &group_boxes, snapshot);
        for (start, end) in model::seams(&cfg) {
            let (a, b) = (to_screen(start.0, start.1), to_screen(end.0, end.1));
            painter.line_segment([a, b], line(4.0, AMBER));
        }
        if let Some((x, y)) = model::cursor_position(snapshot, &cfg) {
            paint_pointer(&painter, to_screen(x, y));
        }
        painter.galley(Pos2::new(mat.left() + SIDE_ROOM + 4.0, mat.bottom() - hint.size().y - HINT_GAP), hint, DIM);

        self.pointer(ui, &cfg, &groups, &to_rect, inner, view, draft, now);
    }

    #[allow(clippy::too_many_arguments)]
    fn pointer(
        &mut self,
        ui: &mut Ui,
        cfg: &LayoutConfig,
        groups: &[(Device, Vec<Display>)],
        to_rect: &impl Fn(&Display) -> Rect,
        inner: Rect,
        view: MapView,
        draft: &mut LayoutDraft,
        now: Instant,
    ) {
        let in_map = |p: Pos2| (p.x - inner.min.x, p.y - inner.min.y);
        for (device, rects) in groups {
            for (index, m) in rects.iter().enumerate() {
                let response = ui.interact(to_rect(m), monitor_id(*device, index), Sense::click_and_drag());
                let response = response.on_hover_cursor(if self.drag.is_some() { CursorIcon::Grabbing } else { CursorIcon::Grab });
                if response.clicked() {
                    self.selected = Some((*device, m.name.clone()));
                    self.just_selected = true;
                }
                if response.drag_started() {
                    let pressed = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos());
                    if let Some(pressed) = pressed {
                        let (px, py) = in_map(pressed);
                        let (wx, wy) = view.to_world(px, py);
                        let (x, y, ..) = cfg.device_box(*device);
                        self.drag = Some(Drag {
                            view,
                            device: *device,
                            grab: (wx - x as f32, wy - y as f32),
                            from: (x, y),
                        });
                    }
                }
                if let (true, Some(drag), Some(pointer)) = (response.dragged(), &self.drag, response.interact_pointer_pos()) {
                    let (x, y) = model::drag_target(&drag.view, in_map(pointer), drag.grab);
                    let magnet = (MAGNET_POINTS / drag.view.scale) as i32;
                    if let Some(state) = &mut draft.state {
                        model::drag_device(&mut state.config, drag.device, x, y, magnet);
                    }
                }
                if response.drag_stopped() {
                    self.finish_drag(draft, now);
                }
            }
        }
    }

    fn finish_drag(&mut self, draft: &mut LayoutDraft, now: Instant) {
        let Some(drag) = self.drag.take() else { return };
        // Dragging the first device moves it the other way if it snaps back, so any change counts.
        let moved = draft.state.as_ref().is_some_and(|s| {
            let (x, y, ..) = s.config.device_box(drag.device);
            (x, y) != drag.from
        });
        if moved {
            draft.edited(now);
        }
    }

    /// Ends a drag whose release never reached the monitor, as when the map stops being drawn.
    pub fn end_lost_drag(&mut self, ctx: &egui::Context, draft: &mut LayoutDraft, now: Instant) {
        if self.drag.is_some() && !ctx.input(|i| i.pointer.any_down()) {
            self.finish_drag(draft, now);
        }
    }

    #[cfg(test)]
    pub fn start_test_drag(&mut self, cfg: &LayoutConfig) {
        let view = model::fit(cfg, 500.0, 300.0);
        self.drag = Some(Drag { view, device: Device::Windows, grab: (0.0, 0.0), from: (1920, 1440) });
    }

    #[cfg(test)]
    pub fn test_view(&self) -> Option<MapView> {
        self.drag.as_ref().map(|d| d.view)
    }
}

fn screens_state(snapshot: &Snapshot, device: Device, active: Option<Device>) -> Screens {
    if snapshot.device(device).is_none() {
        Screens::Off
    } else if active == Some(device) && (snapshot.linked() || snapshot.is_here(device)) {
        Screens::Lit
    } else {
        Screens::Unlit
    }
}

/// A rectangle lit a little brighter at the top, as a screen is.
fn paint_glow(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    for (pos, colour) in [
        (rect.left_top(), top),
        (rect.right_top(), top),
        (rect.right_bottom(), bottom),
        (rect.left_bottom(), bottom),
    ] {
        mesh.vertices.push(Vertex { pos, uv: egui::epaint::WHITE_UV, color: colour });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}

fn paint_monitor(
    painter: &Painter,
    rect: Rect,
    (name, size, rate): &(String, String, Option<String>),
    device: Device,
    screens: Screens,
    picked: bool,
) {
    let bezel = (rect.width().min(rect.height()) * 0.03).clamp(2.0, 7.0);
    painter.rect_filled(rect.shrink(0.5), CornerRadius::same(theme::legacy::SMALL_RADIUS), BEZEL);
    let screen = rect.shrink(bezel);
    let lit = theme::screen_colour(device == Device::Debian);
    let (top, bottom, ink, soft) = match screens {
        Screens::Lit => (lit, theme::mix(lit, MAT, 0.14), ON_SCREEN, theme::mix(ON_SCREEN, lit, 0.25)),
        Screens::Unlit => {
            let dim = theme::mix(lit, MAT, UNLIT_MIX);
            (dim, theme::mix(dim, BEZEL, 0.3), INK, theme::mix(INK, dim, 0.2))
        }
        Screens::Off => (GLASS, GLASS, DIM, DIM),
    };
    paint_glow(painter, screen, top, bottom);
    if picked {
        painter.rect_stroke(rect.expand(1.0), theme::legacy::SMALL_RADIUS, line(2.5, AMBER), StrokeKind::Outside);
    }

    let width = (screen.width() - 12.0).max(1.0);
    let room = screen.height() - 8.0;
    let name_size = (screen.height() * 0.15).clamp(SMALLEST_NAME, LARGEST_NAME);
    let detail_size = (name_size * 0.8).max(SMALLEST_NAME);
    let name_rows = if room > name_size * 5.0 { 2 } else { 1 };
    let name = fitted(painter, name, FontId::proportional(name_size), ink, width, name_rows);
    let detail_font = FontId::proportional(detail_size);
    let both = rate.as_ref().map_or_else(|| size.clone(), |rate| format!("{size} at {rate}"));
    let one_line = painter.layout_no_wrap(both.clone(), detail_font.clone(), soft);
    // At this width the size and rate go on a line each, or, with no height either, just the size.
    let lines: Vec<String> = if one_line.size().x <= width {
        vec![both]
    } else if room > name.size().y + detail_size * 2.6 {
        [Some(size.clone()), rate.clone()].into_iter().flatten().collect()
    } else {
        vec![size.clone()]
    };
    let details: Vec<_> = lines.iter().map(|l| fitted(painter, l, detail_font.clone(), soft, width, 1)).collect();
    let height = name.size().y + details.iter().map(|d| d.size().y).sum::<f32>();
    let text = painter.with_clip_rect(screen.intersect(painter.clip_rect()));
    let mut y = screen.center().y - height / 2.0;
    let name_height = name.size().y;
    text.galley(Pos2::new(screen.center().x - name.size().x / 2.0, y), name, ink);
    y += name_height;
    for detail in details {
        let detail_height = detail.size().y;
        text.galley(Pos2::new(screen.center().x - detail.size().x / 2.0, y), detail, soft);
        y += detail_height;
    }
}

/// Each device's name, outside its group: above it, or below if the other device is above.
fn paint_tags(painter: &Painter, groups: &[(Device, Rect)], snapshot: &Snapshot) {
    let stacked = |a: Rect, b: Rect| a.x_range().intersects(b.x_range()) && a.center().y > b.center().y;
    for (index, (device, rect)) in groups.iter().enumerate() {
        let (_, other) = groups[1 - index];
        let below = stacked(*rect, other);
        let colour = theme::screen_colour(*device == Device::Debian);
        let suffix = if snapshot.is_here(*device) { " (this device)" } else { "" };
        let text = format!("{}{suffix}", snapshot.title(*device));
        let galley = fitted(painter, &text, FontId::proportional(theme::legacy::BODY_SIZE), colour, rect.width().max(160.0), 1);
        let y = if below { rect.bottom() + 6.0 } else { rect.top() - galley.size().y - 6.0 };
        painter.galley(Pos2::new(rect.left(), y), galley, colour);
    }
}

/// A mouse pointer, where the real one is.
fn paint_pointer(painter: &Painter, tip: Pos2) {
    let shape = [(0.0, 0.0), (0.0, 15.0), (4.0, 11.5), (10.5, 11.0)].map(|(x, y)| tip + vec2(x, y));
    painter.add(Shape::convex_polygon(shape.to_vec(), RED, line(1.5, BEZEL)));
}
