//! The arrangement: every screen of both devices as a tile on the map, to scale, with the bright
//! edge where the mouse crosses. Decisions live in `model`; this file is painting and the pointer.

use super::model::{self, Device, Display, LayoutConfig, LayoutDraft, MapView, Snapshot};
use super::theme::{self, line, Type, ACCENT, BG, CROSSING_EDGE, LINE_STRONG, SPACE_6, TEXT, TEXT_DIM};
use super::view::fit_name;
use super::widgets::{fitted, focus_ring};
use eframe::egui::{
    self, vec2, CursorIcon, EventFilter, FontFamily, FontId, Key, Modifiers, Painter, Pos2, Rect, ScrollArea, Sense, Shape,
    StrokeKind, Ui, UiBuilder,
};
use std::time::Instant;

/// How near, on screen, a dragged device has to be to line up with a monitor.
const MAGNET_POINTS: f32 = 14.0;
/// The label and hint sit this far in from the canvas's own margin.
const TEXT_INSET: f32 = 4.0;
const TILE_PADDING: f32 = 10.0;
/// How far an arrow key moves a device, in desktop pixels, and with Shift held.
const NUDGE: i32 = 10;
const NUDGE_FINE: i32 = 1;
const HINT: &str = "Drag a screen to move it. The bright edge is where the mouse crosses.";

/// One monitor on the map: its device, and its name in the layout.
pub type MonitorKey = (Device, String);

/// A device being dragged. The view stays as it was when the drag began, so the map doesn't
/// rescale under the pointer.
struct Drag {
    view: MapView,
    content: [f32; 2],
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
    /// The tile that had the keyboard last frame.
    focused: Option<MonitorKey>,
}

pub fn monitor_id(device: Device, index: usize) -> egui::Id {
    egui::Id::new(("monitor on the map", device, index))
}

impl Canvas {
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn show(&mut self, ui: &mut Ui, draft: &mut LayoutDraft, snapshot: &Snapshot, now: Instant) {
        let (area, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        ui.painter().rect_filled(area, 0.0, BG);
        self.just_selected = false;

        let label = ui.painter().layout_job(Type::Label.job("Layout", TEXT_DIM));
        let label_at = area.min + vec2(TEXT_INSET, 0.0);
        let mut below = label_at.y + label.size().y;
        ui.painter().galley(label_at, label, TEXT_DIM);

        let Some(state) = &draft.state else {
            let text = "Your screens show up here once mesh-kvm is running on this device.";
            let galley = fitted(ui.painter(), text, Type::Body.font(), TEXT_DIM, area.width() - 2.0 * SPACE_6, 3);
            ui.painter().galley(area.center() - galley.size() / 2.0, galley, TEXT_DIM);
            return;
        };
        let cfg = state.config.clone();
        let hint = fitted(ui.painter(), HINT, Type::Caption.font(), TEXT_DIM, area.width() - 2.0 * TEXT_INSET, 2);
        let hint_at = Pos2::new(label_at.x, below + 6.0);
        below = hint_at.y + hint.size().y;
        ui.painter().galley(hint_at, hint, TEXT_DIM);

        let map = Rect::from_min_max(Pos2::new(area.left() + TEXT_INSET, below + SPACE_6 - TEXT_INSET), area.max - vec2(TEXT_INSET, TEXT_INSET));
        if map.width() < 1.0 || map.height() < 1.0 {
            return;
        }
        let (view, content) = match &self.drag {
            Some(d) => (d.view, d.content),
            None => model::tile_view(&cfg, map.width(), map.height()),
        };
        ui.scope_builder(UiBuilder::new().max_rect(map), |ui| {
            if content[0] > map.width() + 0.5 || content[1] > map.height() + 0.5 {
                ScrollArea::both().id_salt("layout canvas").auto_shrink(false).drag_to_scroll(false).show(ui, |ui| {
                    let (inner, _) = ui.allocate_exact_size(vec2(content[0], content[1]), Sense::hover());
                    self.map(ui, inner, &cfg, view, content, snapshot, draft, now);
                });
            } else {
                self.map(ui, map, &cfg, view, content, snapshot, draft, now);
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn map(&mut self, ui: &mut Ui, inner: Rect, cfg: &LayoutConfig, view: MapView, content: [f32; 2], snapshot: &Snapshot, draft: &mut LayoutDraft, now: Instant) {
        let to_screen = |x: i32, y: i32| {
            let (sx, sy) = view.to_screen(x as f32, y as f32);
            inner.min + vec2(sx, sy)
        };
        let to_rect = |m: &Display| Rect::from_min_size(to_screen(m.x, m.y), vec2(m.w as f32, m.h as f32) * view.scale);
        let groups: Vec<(Device, Vec<Display>)> = Device::BOTH.into_iter().map(|d| (d, cfg.rects(d))).collect();

        // The tiles' hit areas come first, so the borders below know who is hovered or focused.
        self.pointer(ui, cfg, &groups, &to_rect, inner, view, content, draft, now);

        let painter = ui.painter().clone();
        let mut borders = Vec::new();
        for (device, rects) in &groups {
            let held = self.drag.as_ref().is_some_and(|d| d.device == *device);
            let group_hovered = (0..rects.len()).any(|i| ui.ctx().read_response(monitor_id(*device, i)).is_some_and(|r| r.hovered()));
            for (index, m) in rects.iter().enumerate() {
                let rect = to_rect(m);
                let key = (*device, m.name.clone());
                let lit = held
                    || group_hovered
                    || self.selected.as_ref() == Some(&key)
                    || self.hovered_in_controls.as_ref() == Some(&key);
                painter.rect_filled(rect, 0.0, theme::tile_fill(*device == Device::Debian));
                let display = model::display_for(snapshot.displays(*device), m);
                paint_tile_text(&painter, rect, &snapshot.title(*device), &model::tile_caption(m, display));
                borders.push((rect, lit, ui.ctx().read_response(monitor_id(*device, index))));
            }
        }
        // The bright borders go last, so a neighbour's plain one never covers them.
        for lit in [false, true] {
            for (rect, is_lit, _) in &borders {
                if *is_lit == lit {
                    painter.rect_stroke(*rect, 0.0, line(1.0, if lit { TEXT } else { LINE_STRONG }), StrokeKind::Inside);
                }
            }
        }
        for (start, end) in model::seams(cfg) {
            painter.line_segment([to_screen(start.0, start.1), to_screen(end.0, end.1)], line(CROSSING_EDGE, ACCENT));
        }
        if let Some((x, y)) = model::cursor_position(snapshot, cfg) {
            paint_pointer(&painter, to_screen(x, y));
        }
        for (_, _, response) in &borders {
            if let Some(response) = response {
                focus_ring(ui, response);
            }
        }
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
        content: [f32; 2],
        draft: &mut LayoutDraft,
        now: Instant,
    ) {
        let in_map = |p: Pos2| (p.x - inner.min.x, p.y - inner.min.y);
        for (device, rects) in groups {
            for (index, m) in rects.iter().enumerate() {
                let response = ui.interact(to_rect(m), monitor_id(*device, index), Sense::click_and_drag());
                let response = response.on_hover_cursor(if self.drag.is_some() { CursorIcon::Grabbing } else { CursorIcon::Grab });
                if response.clicked() {
                    response.request_focus();
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
                            content,
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
                if response.has_focus() {
                    // A tile that gets the keyboard is the selected one.
                    let key = (*device, m.name.clone());
                    if self.focused.as_ref() != Some(&key) {
                        self.selected = Some(key.clone());
                        self.focused = Some(key);
                    }
                } else if self.focused.as_ref() == Some(&(*device, m.name.clone())) {
                    self.focused = None;
                }
                if response.has_focus() && self.drag.is_none() {
                    // The arrows move the device, so they must not also move the focus.
                    let arrows = EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() };
                    ui.memory_mut(|m| m.set_focus_lock_filter(response.id, arrows));
                    self.arrow_keys(ui, *device, draft, now);
                }
            }
        }
    }

    /// Moves the focused tile's device a step for each arrow key pressed.
    fn arrow_keys(&mut self, ui: &Ui, device: Device, draft: &mut LayoutDraft, now: Instant) {
        let steps = [(Key::ArrowLeft, -1, 0), (Key::ArrowRight, 1, 0), (Key::ArrowUp, 0, -1), (Key::ArrowDown, 0, 1)];
        for (key, dx, dy) in steps {
            // Shift is checked first, as the plain key would match it too.
            for (modifiers, step) in [(Modifiers::SHIFT, NUDGE_FINE), (Modifiers::NONE, NUDGE)] {
                while ui.input_mut(|i| i.consume_key(modifiers, key)) {
                    if let Some(state) = &mut draft.state {
                        model::nudge(&mut state.config, device, dx * step, dy * step);
                        draft.edited(now);
                    }
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
        self.drag = Some(Drag { view, content: [500.0, 300.0], device: Device::Windows, grab: (0.0, 0.0), from: (1920, 1440) });
    }

    #[cfg(test)]
    pub fn test_view(&self) -> Option<MapView> {
        self.drag.as_ref().map(|d| d.view)
    }
}

/// A tile's device name and caption, cut to the tile.
fn paint_tile_text(painter: &Painter, rect: Rect, title: &str, caption: &str) {
    let painter = painter.with_clip_rect(rect.shrink(1.0).intersect(painter.clip_rect()));
    let width = rect.width() - 2.0 * TILE_PADDING;
    let strong = FontId::new(14.0, FontFamily::Name(theme::INTER_SEMIBOLD.into()));
    let (shown, _) = fit_name(title);
    let title = fitted(&painter, &shown, strong, TEXT, width, 1);
    let at = rect.min + vec2(TILE_PADDING, TILE_PADDING);
    let below = at.y + title.size().y + 2.0;
    painter.galley(at, title, TEXT);
    let caption = fitted(&painter, caption, FontId::new(12.0, FontFamily::Monospace), TEXT_DIM, width, 1);
    painter.galley(Pos2::new(at.x, below), caption, TEXT_DIM);
}

/// A mouse pointer, where the real one is: a `text` arrow outlined in `bg`.
fn paint_pointer(painter: &Painter, tip: Pos2) {
    let shape = [(0.0, 0.0), (0.0, 15.0), (4.0, 11.5), (10.5, 11.0)].map(|(x, y)| tip + vec2(x, y));
    painter.add(Shape::convex_polygon(shape.to_vec(), TEXT, line(1.5, BG)));
}
