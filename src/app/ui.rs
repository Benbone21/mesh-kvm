//! The app window. It draws what the `Client` last polled and sends what the person changes. The
//! decisions live in `model`; this file is layout and painting.

use super::client::Client;
use super::model::{
    self, Control, ControlsEnabled, Device, DeviceStatus, Edits, LayoutDraft, Light, MapView,
    Snapshot,
};
use super::theme::{
    self, line, AMBER, AMBER_GROOVE, DEBIAN, DEEP, DIM, EDGE, GREEN, INK, PANEL, RAISED, RED, UNLIT,
};
use eframe::egui::{
    self, vec2, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Margin, Painter, Pos2, Rect,
    Response, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextStyle, Ui,
};
use serde_json::json;
use std::ops::RangeInclusive;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

const WINDOW_SIZE: [f32; 2] = [1100.0, 760.0];
const MIN_WINDOW_SIZE: [f32; 2] = [760.0, 560.0];
const SWITCH_SIZE: [f32; 2] = [196.0, 58.0];
const SIDE_WIDTH: f32 = 300.0;
const MIN_MAP_HEIGHT: f32 = 300.0;
/// Below this content width, the two device panels stack.
const TWO_DEVICE_COLUMNS: f32 = 1060.0;
const PILL_SIZE: [f32; 2] = [40.0, 22.0];
const METER_HEIGHT: f32 = 8.0;
const SWITCH_TRAVEL_SECONDS: f32 = 0.16;
const DISABLED_SWITCH: f32 = 0.55;

const MIN_POINTER_SPEED: f64 = 0.05;
const MAX_POINTER_SPEED: f64 = 2.0;
const VOLUME_RANGE: RangeInclusive<f64> = 0.0..=150.0;
const EDGE_PUSH_RANGE: RangeInclusive<f64> = 10.0..=180.0;

/// Opens the window and returns when it closes.
pub fn run_window() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mesh-kvm")
            .with_app_id("mesh-kvm")
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_icon(super::icon::egui_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "mesh-kvm",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            super::single::serve_focus(cc.egui_ctx.clone());
            let ctx = cc.egui_ctx.clone();
            let client = Client::start(super::client::WINDOW_POLL, Box::new(move || ctx.request_repaint()));
            Ok(Box::new(Window::new(client)))
        }),
    )
    .map_err(|e| format!("The mesh-kvm window couldn't start: {e}"))
}

/// A laptop being dragged on the map. The view stays as it was when the drag began, so the map
/// doesn't rescale under the pointer.
struct MapDrag {
    view: MapView,
    /// Where the laptop was grabbed, from its corner, in layout units.
    grab: (f32, f32),
    from: (i32, i32),
}

struct Window {
    client: Arc<Client>,
    edits: Edits,
    layout: LayoutDraft,
    drag: Option<MapDrag>,
    main_change: Option<Receiver<Result<(), String>>>,
    main_error: Option<String>,
    /// The device the failed change was for, and whether the link has been seen down since.
    main_choice: Option<Device>,
    main_link_was_down: bool,
    /// Last frame's heights, so that cards in a row can match the tallest.
    side_height: f32,
    device_height: f32,
}

impl eframe::App for Window {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snapshot = self.client.snapshot();
        self.show(ctx, &snapshot, Instant::now());
    }
}

impl Window {
    fn new(client: Client) -> Self {
        Self {
            client: Arc::new(client),
            edits: Edits::default(),
            layout: LayoutDraft::default(),
            drag: None,
            main_change: None,
            main_error: None,
            main_choice: None,
            main_link_was_down: false,
            side_height: 0.0,
            device_height: 0.0,
        }
    }

    fn show(&mut self, ctx: &egui::Context, snapshot: &Snapshot, now: Instant) {
        let on = model::enabled(snapshot);
        self.collect_main_change();
        self.clear_stale_main_error(snapshot, &on);
        self.layout.follow(snapshot.layout.clone(), self.drag.is_some(), now);

        egui::TopBottomPanel::top("top bar")
            .frame(Frame::new().fill(PANEL).inner_margin(Margin::symmetric(24, 14)))
            .show(ctx, |ui| self.top_bar(ui, snapshot, &on));
        if let Some(message) = model::status_message(snapshot) {
            let fill = if model::notice_is_serious(snapshot) { RED } else { AMBER };
            notice_strip(ctx, "notice", fill, |ui| {
                ui.label(RichText::new(message).color(DEEP));
            });
        }
        if let Some(message) = self.client.last_error() {
            let client = self.client.clone();
            notice_strip(ctx, "last error", RED, |ui| {
                let text_width = (ui.available_width() - 92.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.allocate_ui(vec2(text_width, 0.0), |ui| {
                        ui.set_width(text_width);
                        ui.label(RichText::new(message).color(DEEP));
                    });
                    if ui.add(egui::Button::new(RichText::new("Dismiss").color(INK)).fill(DEEP)).clicked() {
                        client.dismiss_error();
                    }
                });
            });
        }
        egui::CentralPanel::default().frame(Frame::new().fill(DEEP)).show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink(false).drag_to_scroll(false).show(ui, |ui| {
                Frame::new().inner_margin(Margin::same(theme::CARD_PADDING)).show(ui, |ui| {
                    self.content(ui, snapshot, &on, now);
                });
            });
        });

        self.end_lost_drag(ctx, now);
        self.send_changes(snapshot, now);
        let wake = [self.edits.next_wake(now), self.layout.next_wake(now)].into_iter().flatten().min();
        if let Some(wake) = wake {
            ctx.request_repaint_after(wake);
        }
    }

    fn send_changes(&mut self, snapshot: &Snapshot, now: Instant) {
        for ((device, control), value) in self.edits.take_due(now) {
            self.client.local_action(device, control.action(value));
        }
        if let Some(layout) = self.layout.take_due(now) {
            let body = self.edits.layout_save_body(layout, snapshot, now);
            self.client.save_layout(body);
        }
    }

    fn collect_main_change(&mut self) {
        let Some(change) = &self.main_change else { return };
        match change.try_recv() {
            Ok(result) => {
                self.main_error = result.err();
                self.main_change = None;
            }
            Err(TryRecvError::Disconnected) => self.main_change = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Drops the main device error once it no longer applies: the devices link up again after
    /// being apart, or the polled main device is the one that was chosen.
    fn clear_stale_main_error(&mut self, snapshot: &Snapshot, on: &ControlsEnabled) {
        if self.main_error.is_none() || self.main_change.is_some() {
            return;
        }
        if !on.main_device {
            self.main_link_was_down = true;
        }
        let recovered = on.main_device && self.main_link_was_down;
        let done = self.main_choice.is_some() && snapshot.main_device() == self.main_choice;
        if recovered || done {
            self.main_error = None;
        }
    }

    /// Asks both devices to change on a background thread, as it can take seconds.
    fn choose_main(&mut self, ctx: &egui::Context, device: Device) {
        let (done, change) = mpsc::channel();
        let (client, ctx) = (self.client.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = done.send(client.set_main_device(device));
            ctx.request_repaint();
        });
        self.main_error = None;
        self.main_choice = Some(device);
        self.main_link_was_down = false;
        self.main_change = Some(change);
    }

    fn top_bar(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled) {
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), SWITCH_SIZE[1]), Sense::hover());
        let switch_rect = Rect::from_center_size(bar.center(), SWITCH_SIZE.into());
        let gap = 24.0;
        let left = Rect::from_min_max(bar.min, Pos2::new(switch_rect.left() - gap, bar.bottom()));
        let right = Rect::from_min_max(Pos2::new(switch_rect.right() + gap, bar.top()), bar.max);
        device_bay(ui, left, Device::Debian, snapshot, Align2::LEFT_CENTER);
        device_bay(ui, right, Device::Windows, snapshot, Align2::RIGHT_CENTER);
        if slide_switch(ui, switch_rect, snapshot.active(), on.switch).clicked() {
            self.client.master_action("toggle_focus");
        }
    }

    fn content(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        let width = ui.available_width();
        let border = 2.0 * (theme::CARD_PADDING as f32 + 1.0);
        ui.horizontal_top(|ui| {
            let map_width = (width - SIDE_WIDTH - theme::CARD_GAP - border).max(0.0);
            card(ui, map_width, |ui| self.map(ui, snapshot, now));
            ui.add_space(theme::CARD_GAP - ui.spacing().item_spacing.x);
            card(ui, SIDE_WIDTH - border, |ui| {
                let height = measured_height(ui, |ui| self.side_panel(ui, snapshot, on, now));
                if height != self.side_height {
                    self.side_height = height;
                    ui.ctx().request_repaint();
                }
            });
        });
        ui.add_space(theme::CARD_GAP - ui.spacing().item_spacing.y);

        let mut tallest: f32 = 0.0;
        let two_columns = width >= TWO_DEVICE_COLUMNS;
        if two_columns {
            let card_width = (width - theme::CARD_GAP) / 2.0 - border;
            ui.horizontal_top(|ui| {
                for (i, device) in Device::BOTH.into_iter().enumerate() {
                    if i > 0 {
                        ui.add_space(theme::CARD_GAP - ui.spacing().item_spacing.x);
                    }
                    card(ui, card_width, |ui| {
                        let height = measured_height(ui, |ui| self.device_panel(ui, device, snapshot, on, now));
                        tallest = tallest.max(height);
                        ui.add_space((self.device_height - height).max(0.0));
                    });
                }
            });
        } else {
            for device in Device::BOTH {
                card(ui, width - border, |ui| self.device_panel(ui, device, snapshot, on, now));
                ui.add_space(theme::CARD_GAP - ui.spacing().item_spacing.y);
            }
        }
        if tallest != self.device_height {
            self.device_height = tallest;
            ui.ctx().request_repaint();
        }
    }

    fn map(&mut self, ui: &mut Ui, snapshot: &Snapshot, now: Instant) {
        let has_layout = self.layout.state.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.heading("Screens");
            if has_layout {
                ui.label(small("Drag the laptop to where it sits on your desk."));
            }
        });
        let height = (self.side_height - ui.min_rect().height() - ui.spacing().item_spacing.y).max(MIN_MAP_HEIGHT);
        let (well, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        paint_map_well(ui.painter(), well);

        let Some(state) = &self.layout.state else {
            let text = "The layout is stored on the Debian desktop and needs it to be connected.";
            let galley = ui.painter().layout(text.into(), body_font(ui), DIM, well.width() - 48.0);
            let at = well.center() - galley.size() / 2.0;
            ui.painter().galley(at, galley, DIM);
            return;
        };
        let cfg = state.config.clone();
        let view = self.drag.as_ref().map_or_else(|| model::fit(&cfg, well.width(), well.height()), |d| d.view);
        let to_rect = |x: i32, y: i32, w: i32, h: i32| {
            let (sx, sy) = view.to_screen(x as f32, y as f32);
            Rect::from_min_size(well.min + vec2(sx, sy), vec2(w as f32, h as f32) * view.scale)
        };
        let painter = ui.painter().with_clip_rect(well);
        let active = snapshot.active();

        for m in &cfg.debian_monitors {
            let colours = (theme::mix(DEEP, DEBIAN, 0.28), DEBIAN);
            paint_screen(&painter, to_rect(m.x, m.y, m.w, m.h), &m.name, (m.w, m.h), colours, active == Some(Device::Debian));
        }
        let laptop = to_rect(cfg.win_x, cfg.win_y, cfg.win_w, cfg.win_h);
        let colours = (theme::mix(DEEP, AMBER, 0.16), AMBER);
        paint_screen(&painter, laptop, Device::Windows.title(), (cfg.win_w, cfg.win_h), colours, active == Some(Device::Windows));
        if self.drag.is_some() {
            paint_attached_edge(&painter, laptop, &cfg.attach_side);
        }
        if let Some((x, y)) = model::cursor_position(snapshot, &cfg) {
            let (sx, sy) = view.to_screen(x as f32, y as f32);
            painter.circle_filled(well.min + vec2(sx, sy), 5.0, RED);
        }

        let grab = ui.interact(laptop, laptop_id(), Sense::drag());
        let grab = grab.on_hover_cursor(if self.drag.is_some() { CursorIcon::Grabbing } else { CursorIcon::Grab });
        let in_map = |p: Pos2| (p.x - well.min.x, p.y - well.min.y);
        if grab.drag_started() {
            let pressed = ui.input(|i| i.pointer.press_origin()).or(grab.interact_pointer_pos());
            if let Some(pressed) = pressed {
                let (px, py) = in_map(pressed);
                let (wx, wy) = view.to_world(px, py);
                let offset = (wx - cfg.win_x as f32, wy - cfg.win_y as f32);
                self.drag = Some(MapDrag { view, grab: offset, from: (cfg.win_x, cfg.win_y) });
            }
        }
        if let (true, Some(drag), Some(pointer)) = (grab.dragged(), &self.drag, grab.interact_pointer_pos()) {
            let (x, y) = model::drag_target(&drag.view, in_map(pointer), drag.grab);
            if let (Some(place), Some(state)) = (model::snap(&cfg, x, y), &mut self.layout.state) {
                model::apply_placement(&mut state.config, &place);
            }
        }
        if grab.drag_stopped() {
            self.finish_drag(now);
        }
    }

    fn finish_drag(&mut self, now: Instant) {
        let Some(drag) = self.drag.take() else { return };
        let moved = self.layout.state.as_ref().is_some_and(|s| (s.config.win_x, s.config.win_y) != drag.from);
        if moved {
            self.layout.edited(now);
        }
    }

    /// Ends a drag whose release never reached the laptop, as when the map stops being drawn.
    fn end_lost_drag(&mut self, ctx: &egui::Context, now: Instant) {
        if self.drag.is_some() && !ctx.input(|i| i.pointer.any_down()) {
            self.finish_drag(now);
        }
    }

    fn side_panel(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        ui.heading("Layout");
        ui.add_enabled_ui(on.layout, |ui| self.layout_controls(ui, snapshot, now));
        ui.add_space(10.0);
        ui.heading("Main device");
        self.main_device_controls(ui, snapshot, on);
        ui.add_space(10.0);
        ui.add_enabled_ui(on.release, |ui| {
            let text = RichText::new("Release all input").color(DEEP);
            if wide_button(ui, egui::Button::new(text).fill(RED).stroke(Stroke::NONE)).clicked() {
                self.client.release_all_input();
            }
        });
    }

    fn layout_controls(&mut self, ui: &mut Ui, snapshot: &Snapshot, now: Instant) {
        let cfg = self.layout.state.as_ref().map(|s| s.config.clone()).unwrap_or_default();
        let has_layout = self.layout.state.is_some();
        let placements = model::placements(&cfg);
        let unavailable = || "Not available".to_string();

        ui.label("Laptop position");
        let selected = if has_layout { model::position_label(&cfg, &placements) } else { unavailable() };
        let mut chosen_place = None;
        egui::ComboBox::from_id_salt("laptop position")
            .selected_text(selected.clone())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for place in &placements {
                    if ui.selectable_label(place.label == selected, &place.label).clicked() {
                        chosen_place = Some(place);
                    }
                }
            });

        ui.label("Laptop resolution");
        let selected = if has_layout { model::resolution_label(&cfg) } else { unavailable() };
        let mut chosen_size = None;
        egui::ComboBox::from_id_salt("laptop resolution")
            .selected_text(selected.clone())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                if ui.selectable_label(selected == "Native", "Native").clicked() {
                    chosen_size = Some(None);
                }
                for size in model::RESOLUTIONS {
                    let label = model::size_label(size);
                    if ui.selectable_label(label == selected, &label).clicked() {
                        chosen_size = Some(Some(size));
                    }
                }
            });

        let mut push = cfg.edge_resistance as f64;
        let push_text = if has_layout { format!("{push:.0}") } else { String::new() };
        let moved = slider_row(ui, "Push needed to cross a screen edge", push_text, &mut push, EDGE_PUSH_RANGE, 5.0);

        let auto_lock = toggle_row(ui, "Keep the mouse on the screen while a game runs", cfg.auto_game_lock).clicked();
        ui.label(small("This applies to the Debian desktop. The laptop always watches for games."));
        let manual_lock = toggle_row(ui, "Lock the mouse to this screen now", cfg.manual_game_lock).clicked();
        let telemetry = snapshot.layout.as_ref().map(|l| &l.telemetry);
        if let Some(hint) = telemetry.and_then(model::game_lock_hint) {
            ui.label(small(hint));
        }
        let reset = wide_button(ui, egui::Button::new("Reset to native resolutions")).clicked();

        let Some(state) = &mut self.layout.state else { return };
        if let Some(place) = chosen_place {
            model::apply_placement(&mut state.config, place);
        }
        if let Some(size) = chosen_size {
            model::set_resolution(&mut state.config, size);
        }
        if moved {
            state.config.edge_resistance = push as i32;
        }
        if auto_lock {
            state.config.auto_game_lock = !state.config.auto_game_lock;
            self.client.master_action("toggle_auto_game_lock");
        }
        if manual_lock {
            state.config.manual_game_lock = !state.config.manual_game_lock;
            self.client.master_action("toggle_game_lock");
        }
        if auto_lock || manual_lock {
            self.layout.hold(now);
        }
        if chosen_place.is_some() || chosen_size.is_some() || moved {
            self.layout.edited(now);
        }
        if reset {
            self.client.master_action("reset_native_resolutions");
            self.layout.let_poll_through();
        }
    }

    fn main_device_controls(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled) {
        let changing = self.main_change.is_some();
        let current = snapshot.main_device();
        let mut chosen = None;
        ui.add_enabled_ui(on.main_device && !changing, |ui| {
            let gap = 2.0;
            let width = (ui.available_width() - gap) / 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for device in Device::BOTH {
                    if segment(ui, width, device.title(), current == Some(device)).clicked() && current != Some(device) {
                        chosen = Some(device);
                    }
                }
            });
        });
        if let Some(device) = chosen {
            self.choose_main(ui.ctx(), device);
        }
        if let Some(error) = &self.main_error {
            ui.label(small(error).color(RED));
        } else if changing {
            ui.label(small("Changing the main device."));
        } else if on.main_device {
            ui.label(small("It takes the keyboard and mouse when the devices link up."));
        } else {
            ui.label(small("Both devices need to be linked to change this."));
        }
    }

    fn device_panel(&mut self, ui: &mut Ui, device: Device, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        let status = snapshot.device(device);
        ui.horizontal_wrapped(|ui| {
            ui.heading(device.title());
            let here = snapshot.local.as_ref().is_some_and(|l| l.node == device.node());
            ui.label(small(device_details(status, here)));
        });
        let Some(status) = status else {
            if let Some(message) = model::status_message(snapshot) {
                ui.label(RichText::new(message).color(DIM));
            }
            return;
        };
        let mut panel = DevicePanel { edits: &mut self.edits, device, status, now };
        ui.columns(2, |columns| {
            panel.input_and_clipboard(&mut columns[0]);
            let test_sound = panel.audio(&mut columns[1], on.test_sound);
            if test_sound {
                self.client.local_action(device, json!({ "action": "test_audio_chime", "target": "peer" }));
            }
        });
    }
}

/// The controls of one reachable device.
struct DevicePanel<'a> {
    edits: &'a mut Edits,
    device: Device,
    status: &'a DeviceStatus,
    now: Instant,
}

impl DevicePanel<'_> {
    fn value(&self, control: Control) -> f64 {
        self.edits.shown((self.device, control), control.read(self.status), self.now)
    }

    fn set(&mut self, control: Control, value: f64) {
        self.edits.set((self.device, control), value, self.now);
    }

    fn toggle(&mut self, ui: &mut Ui, control: Control, label: &str) {
        let on = self.value(control) != 0.0;
        if toggle_row(ui, label, on).clicked() {
            self.set(control, if on { 0.0 } else { 1.0 });
        }
    }

    fn slider(
        &mut self,
        ui: &mut Ui,
        control: Control,
        label: &str,
        format: impl Fn(f64) -> String,
        range: RangeInclusive<f64>,
        step: f64,
    ) {
        let mut value = self.value(control);
        if slider_row(ui, label, format(value), &mut value, range, step) {
            self.set(control, value);
        }
    }

    fn input_and_clipboard(&mut self, ui: &mut Ui) {
        let other = self.device.other().nickname();
        // An older service has no actions for these, so they stay off rather than failing.
        let newer = !self.status.is_older();
        ui.add_enabled_ui(newer, |ui| {
            section(ui, "Mouse and keyboard");
            self.toggle(ui, Control::MouseSharing, "Share mouse");
            self.toggle(ui, Control::KeyboardSharing, "Share keyboard");
            let speed_label = format!("Pointer speed on {other}");
            let range = MIN_POINTER_SPEED..=MAX_POINTER_SPEED;
            self.slider(ui, Control::PointerSpeed, &speed_label, |v| format!("{v:.2}\u{d7}"), range, 0.05);

            ui.add_space(6.0);
            section(ui, "Clipboard");
            self.toggle(ui, Control::ClipboardSend, &format!("Send clipboard to {other}"));
            self.toggle(ui, Control::ClipboardReceive, &format!("Receive clipboard from {other}"));
        });

        ui.add_space(6.0);
        section(ui, "Startup");
        self.toggle(ui, Control::Autostart, "Start mesh-kvm when this device boots");
    }

    /// Returns true when the test sound was asked for.
    fn audio(&mut self, ui: &mut Ui, can_test: bool) -> bool {
        let other = self.device.other().nickname();
        let percent = |v: f64| format!("{v:.0}%");
        section(ui, "Audio sent");
        self.toggle(ui, Control::AudioSend, &format!("Send audio to {other}"));
        meter(ui, self.status.audio.tx_peak);
        if self.status.audio.tx_source.is_some() {
            self.source_picker(ui);
        }
        self.slider(ui, Control::SendVolume, "Send volume", percent, VOLUME_RANGE, 5.0);

        ui.add_space(6.0);
        section(ui, "Audio received");
        self.toggle(ui, Control::AudioPlay, &format!("Play audio from {other}"));
        meter(ui, self.status.audio.rx_peak);
        self.slider(ui, Control::PlayVolume, "Playback volume", percent, VOLUME_RANGE, 5.0);

        let label = format!("Play a test sound on {other}");
        ui.add_enabled_ui(can_test, |ui| wide_button(ui, egui::Button::new(label)).clicked()).inner
    }

    fn source_picker(&mut self, ui: &mut Ui) {
        const SOURCES: [&str; 2] = ["System sound", "Microphone"];
        let current = self.value(Control::AudioSource) as usize;
        ui.label("Audio to send");
        let mut chosen = None;
        egui::ComboBox::from_id_salt((self.device, "audio source"))
            .selected_text(SOURCES[current.min(1)])
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for (mode, name) in SOURCES.iter().enumerate() {
                    if ui.selectable_label(mode == current, *name).clicked() {
                        chosen = Some(mode);
                    }
                }
            });
        if let Some(mode) = chosen {
            self.set(Control::AudioSource, mode as f64);
        }
    }
}

fn laptop_id() -> egui::Id {
    egui::Id::new("laptop on the map")
}

fn device_details(status: Option<&DeviceStatus>, here: bool) -> String {
    match status {
        None => "Not connected".into(),
        Some(d) if here => format!("This device, {}, {}", d.hostname, d.ip),
        Some(d) => format!("{}, {}", d.hostname, d.ip),
    }
}

/// A raised panel with `contents` in a column of this width.
fn card(ui: &mut Ui, width: f32, contents: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(PANEL)
        .stroke(line(1.0, EDGE))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(theme::CARD_PADDING))
        .show(ui, |ui| {
            // A frame takes on its parent's layout, and cards sit in rows.
            ui.vertical(|ui| {
                ui.set_width(width);
                contents(ui);
            });
        });
}

/// How tall `contents` came out, before anything stretches the card around it.
fn measured_height(ui: &mut Ui, contents: impl FnOnce(&mut Ui)) -> f32 {
    let top = ui.next_widget_position().y;
    contents(ui);
    ui.min_rect().bottom() - top
}

fn wide_button(ui: &mut Ui, button: egui::Button) -> Response {
    ui.add_sized([ui.available_width(), theme::BUTTON_HEIGHT], button)
}

fn notice_strip(ctx: &egui::Context, id: &'static str, fill: Color32, contents: impl FnOnce(&mut Ui)) {
    egui::TopBottomPanel::top(id)
        .show_separator_line(false)
        .frame(Frame::new().fill(fill).inner_margin(Margin::symmetric(24, 10)))
        .show(ctx, contents);
}

fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).size(theme::SMALL_SIZE).color(DIM)
}

fn section(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(DIM));
}

fn body_font(ui: &Ui) -> FontId {
    TextStyle::Body.resolve(ui.style())
}

fn focus_ring(ui: &Ui, response: &Response, rect: Rect) {
    if response.has_focus() {
        let ring = line(2.0, AMBER);
        ui.painter().rect_stroke(rect.expand(2.0), theme::RADIUS, ring, StrokeKind::Outside);
    }
}

fn paint_light(painter: &Painter, center: Pos2, light: Light) {
    const RADIUS: f32 = 7.0;
    let colour = match light {
        Light::Off => {
            painter.circle(center, RADIUS, UNLIT, line(2.0, EDGE));
            return;
        }
        Light::Connected => GREEN,
        Light::Active => AMBER,
    };
    for (spread, alpha) in [(7.0, 0.05), (5.0, 0.08), (3.0, 0.14), (1.5, 0.2)] {
        painter.circle_filled(center, RADIUS + spread, colour.gamma_multiply(alpha));
    }
    painter.circle_filled(center, RADIUS, colour);
}

/// A device's name and state in the top bar, against one side of `rect`.
fn device_bay(ui: &Ui, rect: Rect, device: Device, snapshot: &Snapshot, side: Align2) {
    let (light, state) = model::device_badge(snapshot, device);
    let painter = ui.painter().with_clip_rect(rect);
    let on_left = side == Align2::LEFT_CENTER;
    let light_x = if on_left { rect.left() + 9.0 } else { rect.right() - 9.0 };
    paint_light(&painter, Pos2::new(light_x, rect.center().y), light);
    let text_x = if on_left { rect.left() + 28.0 } else { rect.right() - 28.0 };
    let anchor = if on_left { Align2::LEFT_BOTTOM } else { Align2::RIGHT_BOTTOM };
    let title_at = Pos2::new(text_x, rect.center().y + 3.0);
    painter.text(title_at, anchor, device.title(), FontId::proportional(theme::TITLE_SIZE), INK);
    let anchor = if on_left { Align2::LEFT_TOP } else { Align2::RIGHT_TOP };
    let state_at = Pos2::new(text_x, rect.center().y + 5.0);
    painter.text(state_at, anchor, state, FontId::proportional(theme::SMALL_SIZE), DIM);
}

/// Where the knob sits, from 0 at the Debian end to 1 at the Windows end. It waits in the middle
/// when nothing says who has control.
fn knob_position(active: Option<Device>) -> f32 {
    match active {
        Some(Device::Debian) => 0.0,
        Some(Device::Windows) => 1.0,
        None => 0.5,
    }
}

/// The one physical object on the panel: a slide switch that shows who owns the keyboard.
fn slide_switch(ui: &mut Ui, rect: Rect, active: Option<Device>, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.id().with("slide switch"), sense);
    let hint = match (enabled, active) {
        (true, Some(device)) => format!("Switch keyboard and mouse to {}", device.other().name()),
        _ => "The switch works while the devices are linked and agree on a main device.".to_string(),
    };
    let response = response.on_hover_text(hint);

    let shade = if enabled { 1.0 } else { DISABLED_SWITCH };
    let painter = ui.painter();
    let radius = theme::RADIUS;
    let border = if enabled && response.hovered() { INK } else { EDGE };
    painter.rect_filled(rect, radius, DEEP);
    // A shadow along the top makes the track read as a recess.
    for (depth, alpha) in [(2.0, 0.4), (4.0, 0.25), (7.0, 0.12)] {
        let shadow = Rect::from_min_size(rect.min, vec2(rect.width(), depth)).shrink2(vec2(2.0, 0.0));
        painter.rect_filled(shadow, 0.0, Color32::BLACK.gamma_multiply(alpha));
    }
    painter.rect_stroke(rect, radius, line(2.0, border.gamma_multiply(shade)), StrokeKind::Inside);

    let inset = 6.0;
    let knob_size = vec2((rect.width() - 2.0 * inset) / 2.0 - 2.0, rect.height() - 2.0 * inset);
    let travel = rect.width() - 2.0 * inset - knob_size.x;
    let position = ui.ctx().animate_value_with_time(
        response.id.with("knob"),
        knob_position(active),
        SWITCH_TRAVEL_SECONDS,
    );
    let knob = Rect::from_min_size(rect.min + vec2(inset + travel * position, inset), knob_size);
    painter.rect_filled(knob, 3.0, AMBER.gamma_multiply(shade));
    let grooves = painter.with_clip_rect(knob.shrink(1.0));
    let mut x = knob.left() + 5.0;
    while x < knob.right() {
        let groove = Rect::from_min_size(Pos2::new(x, knob.top()), vec2(2.0, knob.height()));
        grooves.rect_filled(groove, 0.0, AMBER_GROOVE.gamma_multiply(shade));
        x += 7.0;
    }
    if let Some(device) = active {
        let font = FontId::proportional(16.0);
        // Painted twice, half a pixel apart, as the bundled font has no bold.
        for nudge in [0.0, 0.5] {
            painter.text(knob.center() + vec2(nudge, 0.0), Align2::CENTER_CENTER, device.short_name(), font.clone(), DEEP);
        }
    }
    focus_ring(ui, &response, rect);
    response
}

/// A label with an on and off switch. The whole row can be clicked.
fn toggle_row(ui: &mut Ui, label: &str, on: bool) -> Response {
    let width = ui.available_width();
    let pill = vec2(PILL_SIZE[0], PILL_SIZE[1]);
    let galley = ui.painter().layout(label.to_string(), body_font(ui), INK, width - pill.x - 12.0);
    let height = (galley.size().y + 8.0).max(theme::ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter();
    painter.galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, INK);

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
fn slider_row(
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

fn meter(ui: &mut Ui, percent: u32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), METER_HEIGHT), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, DEEP);
    let level = rect.width() * percent.min(100) as f32 / 100.0;
    let filled = Rect::from_min_size(rect.min, vec2(level, rect.height()));
    ui.painter().rect_filled(filled, 2.0, GREEN);
}

/// One half of a two-way choice.
fn segment(ui: &mut Ui, width: f32, label: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(width, theme::BUTTON_HEIGHT), Sense::click());
    let (fill, ink) = if selected { (AMBER, DEEP) } else { (RAISED, INK) };
    let painter = ui.painter();
    painter.rect_filled(rect, theme::SMALL_RADIUS, fill);
    let border = if response.hovered() { INK } else if selected { AMBER } else { EDGE };
    painter.rect_stroke(rect, theme::SMALL_RADIUS, line(1.0, border), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(14.0), ink);
    focus_ring(ui, &response, rect);
    response
}

fn paint_map_well(painter: &Painter, well: Rect) {
    const DOT_GAP: f32 = 24.0;
    painter.rect_filled(well, theme::SMALL_RADIUS, DEEP);
    let dot = EDGE.gamma_multiply(0.45);
    let mut y = well.top() + DOT_GAP / 2.0;
    while y < well.bottom() {
        let mut x = well.left() + DOT_GAP / 2.0;
        while x < well.right() {
            painter.circle_filled(Pos2::new(x, y), 1.0, dot);
            x += DOT_GAP;
        }
        y += DOT_GAP;
    }
}

fn paint_screen(
    painter: &Painter,
    rect: Rect,
    name: &str,
    size: (i32, i32),
    (fill, border): (Color32, Color32),
    has_control: bool,
) {
    painter.rect_filled(rect, theme::SMALL_RADIUS, fill);
    painter.rect_stroke(rect, theme::SMALL_RADIUS, line(2.0, border), StrokeKind::Inside);
    if has_control {
        painter.rect_stroke(rect, theme::SMALL_RADIUS, line(2.0, INK), StrokeKind::Outside);
    }
    let text = painter.with_clip_rect(rect.shrink(4.0).intersect(painter.clip_rect()));
    let wrap = (rect.width() - 8.0).max(1.0);
    let title = text.layout(name.to_string(), FontId::proportional(14.0), INK, wrap);
    let detail = text.layout(model::size_label(size), FontId::proportional(12.0), DIM, wrap);
    let height = title.size().y + detail.size().y;
    let top = rect.center().y - height / 2.0;
    let title_at = Pos2::new(rect.center().x - title.size().x / 2.0, top);
    let detail_at = Pos2::new(rect.center().x - detail.size().x / 2.0, top + title.size().y);
    text.galley(title_at, title, INK);
    text.galley(detail_at, detail, DIM);
}

/// Marks the edge of the laptop that touches the desktop, to show where a drag will land.
fn paint_attached_edge(painter: &Painter, laptop: Rect, side: &str) {
    let (a, b) = match side {
        // The side is the desktop edge the laptop sits against, so the laptop's opposite edge.
        "Right" => (laptop.left_top(), laptop.left_bottom()),
        "Left" => (laptop.right_top(), laptop.right_bottom()),
        "Bottom" => (laptop.left_top(), laptop.right_top()),
        "Top" => (laptop.left_bottom(), laptop.right_bottom()),
        _ => return,
    };
    painter.line_segment([a, b], line(4.0, AMBER));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model::LayoutState;
    use eframe::egui::{Event, PointerButton, RawInput};
    use std::time::Duration;

    const STATE: &str = r#"{"config":{"debian_monitors":[{"name":"DP-3 (240Hz Native)","x":0,"y":360,"w":1920,"h":1080},{"name":"DP-2 (360Hz Native)","x":1920,"y":0,"w":2560,"h":1440}],"deb_total_w":4480,"deb_total_h":1440,"win_x":1920,"win_y":1440,"win_w":2560,"win_h":1600,"win_native_w":2560,"win_native_h":1600,"resolution_mode":"native","attach_side":"Bottom","mouse_speed":0.35,"edge_resistance":55,"auto_game_lock":true,"manual_game_lock":false},"telemetry":{"active_node":"Debian","deb_cursor_x":3200,"deb_cursor_y":720,"peer_online":true}}"#;

    fn device(node: &str, tx_source: Option<u32>) -> DeviceStatus {
        let mut d: DeviceStatus = serde_json::from_str(&format!(
            r#"{{"node":"{node}","hostname":"{node}-host","ip":"192.168.8.2","peer_online":true,"active_node":"Debian","cursor":[10,20]}}"#
        ))
        .unwrap();
        d.sharing = Some(Default::default());
        d.audio.tx_source = tx_source;
        d.audio.tx_volume = 100;
        d.audio.rx_volume = 100;
        d.audio.tx_peak = 60;
        d
    }

    fn linked() -> Snapshot {
        Snapshot {
            local: Some(device("debian", Some(0))),
            peer: Some(device("windows", None)),
            layout: LayoutState::parse(STATE),
        }
    }

    fn states() -> Vec<Snapshot> {
        let mut no_peer = linked();
        no_peer.peer = None;
        let laptop_alone = Snapshot { local: Some(device("windows", None)), peer: None, layout: None };
        let mut split = linked();
        split.peer.as_mut().unwrap().main_device = 1;
        split.local.as_mut().unwrap().main_conflict = true;
        let mut older = linked();
        older.peer.as_mut().unwrap().sharing = None;
        vec![Snapshot::default(), linked(), no_peer, laptop_alone, split, older]
    }

    fn window() -> Window {
        // Nothing listens here, so whatever the tests send goes nowhere.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        Window::new(Client::start_on(port, Box::new(|| {})))
    }

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        ctx
    }

    fn frame(
        ctx: &egui::Context,
        window: &mut Window,
        snapshot: &Snapshot,
        size: [f32; 2],
        events: Vec<Event>,
        now: Instant,
    ) -> egui::FullOutput {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size.into())),
            events,
            ..Default::default()
        };
        ctx.run(input, |ctx| window.show(ctx, snapshot, now))
    }

    fn pointer(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    /// Settles the layout, then returns the laptop's rectangle on screen.
    fn laptop_on_screen(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, now: Instant) -> Rect {
        for _ in 0..3 {
            frame(ctx, window, snapshot, WINDOW_SIZE, vec![], now);
        }
        ctx.read_response(laptop_id()).unwrap().rect
    }

    #[test]
    fn draws_every_state_at_both_window_sizes() {
        for size in [MIN_WINDOW_SIZE, WINDOW_SIZE] {
            for snapshot in states() {
                let (ctx, mut window) = (context(), window());
                for _ in 0..3 {
                    frame(&ctx, &mut window, &snapshot, size, vec![], Instant::now());
                }
            }
        }
    }

    #[test]
    fn the_window_is_idle_when_nothing_changes() {
        for snapshot in states() {
            let (ctx, mut window) = (context(), window());
            let now = Instant::now();
            // Long enough for the first frames' measuring and the switch's travel to finish.
            let mut last = None;
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(10));
                last = Some(frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now));
            }
            let repaint = last.unwrap().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            assert_eq!(repaint, Duration::MAX, "still repainting");
        }
    }

    #[test]
    fn a_drag_that_loses_its_release_still_ends() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (linked(), Instant::now());
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        let view = model::fit(&snapshot.layout.as_ref().unwrap().config, 500.0, 300.0);
        window.drag = Some(MapDrag { view, grab: (0.0, 0.0), from: (1920, 1440) });
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert!(window.drag.is_none());
    }

    #[test]
    fn dragging_the_laptop_snaps_it_and_ignores_the_poll_until_release() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (linked(), Instant::now());
        let laptop = laptop_on_screen(&ctx, &mut window, &snapshot, now);
        let start = laptop.center();
        let end = start + vec2(120.0, 0.0);
        let step = |w: &mut Window, events| {
            frame(&ctx, w, &snapshot, WINDOW_SIZE, events, now);
        };
        step(&mut window, vec![Event::PointerMoved(start)]);
        step(&mut window, vec![pointer(start, true)]);
        step(&mut window, vec![Event::PointerMoved(start + vec2(60.0, 0.0))]);
        assert!(window.drag.is_some(), "the laptop should be held");
        step(&mut window, vec![Event::PointerMoved(end)]);
        let held = window.layout.state.as_ref().unwrap().config.win_x;
        assert!(held > 1920, "the laptop should have moved, not {held}");

        // A poll arrives saying the laptop is where it was, and the view stays frozen.
        let view = window.drag.as_ref().unwrap().view;
        step(&mut window, vec![]);
        assert_eq!(window.layout.state.as_ref().unwrap().config.win_x, held);
        assert_eq!(window.drag.as_ref().unwrap().view, view);

        step(&mut window, vec![pointer(end, false)]);
        assert!(window.drag.is_none());
        // Held for a moment after release, so the old poll doesn't undo the move.
        step(&mut window, vec![]);
        assert_eq!(window.layout.state.as_ref().unwrap().config.win_x, held);
    }

    #[test]
    fn main_device_runs_off_the_ui_thread_and_shows_its_error() {
        let (ctx, mut window) = (context(), window());
        let snapshot = Snapshot::default();
        window.choose_main(&ctx, Device::Debian);
        assert!(window.main_change.is_some());
        for _ in 0..200 {
            frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
            if window.main_change.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let error = window.main_error.as_deref().unwrap();
        assert!(error.contains("Start mesh-kvm"), "{error}");
    }

    #[test]
    fn main_error_goes_when_the_link_recovers_or_the_choice_takes_hold() {
        let mut window = window();
        let (down, up) = (Snapshot::default(), linked());
        let set = |w: &mut Window| {
            w.main_error = Some("Couldn't reach the laptop.".into());
            w.main_choice = Some(Device::Windows);
            w.main_link_was_down = false;
        };
        set(&mut window);
        window.clear_stale_main_error(&down, &model::enabled(&down));
        assert!(window.main_error.is_some());
        window.clear_stale_main_error(&up, &model::enabled(&up));
        assert!(window.main_error.is_none());

        // A refusal while the link is up stays until something changes.
        set(&mut window);
        window.clear_stale_main_error(&up, &model::enabled(&up));
        assert!(window.main_error.is_some());
        // The poll showing the chosen device clears it.
        let mut chosen = up.clone();
        for d in [chosen.local.as_mut(), chosen.peer.as_mut()].into_iter().flatten() {
            d.main_device = Device::Windows.id();
        }
        window.clear_stale_main_error(&chosen, &model::enabled(&chosen));
        assert!(window.main_error.is_none());
    }

    #[test]
    fn edits_are_sent_on_the_frame_they_become_due() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (Snapshot::default(), Instant::now());
        window.edits.set((Device::Debian, Control::Autostart), 1.0, now);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        // With no service the send fails at once, which shows that it was tried.
        assert!(window.client.last_error().is_some());
        assert_eq!(window.edits.take_due(now), vec![]);
        // The error strip draws, and the window keeps working.
        frame(&ctx, &mut window, &snapshot, MIN_WINDOW_SIZE, vec![], now);
    }

    #[test]
    fn two_device_columns_need_room() {
        // The content width at the minimum and default window sizes.
        let content = |window: f32| window - 2.0 * theme::CARD_PADDING as f32;
        assert!(content(MIN_WINDOW_SIZE[0]) < TWO_DEVICE_COLUMNS);
        assert!(content(WINDOW_SIZE[0]) >= TWO_DEVICE_COLUMNS);
    }

    #[test]
    fn device_details_name_this_device() {
        let d = device("debian", None);
        assert_eq!(device_details(Some(&d), true), "This device, debian-host, 192.168.8.2");
        assert_eq!(device_details(Some(&d), false), "debian-host, 192.168.8.2");
        assert_eq!(device_details(None, false), "Not connected");
    }

    #[test]
    fn knob_waits_in_the_middle_when_nobody_is_known_to_have_control() {
        assert_eq!(knob_position(Some(Device::Debian)), 0.0);
        assert_eq!(knob_position(Some(Device::Windows)), 1.0);
        assert_eq!(knob_position(None), 0.5);
    }
}
