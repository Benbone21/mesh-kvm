//! The app window. It draws what the `Client` last polled and sends what the person changes. The
//! decisions live in `model`; the arrangement is in `canvas`, and the device controls in `panels`.

use super::canvas::Canvas;
use super::client::Client;
use super::launch::ServiceStarter;
use super::model::{self, ControlsEnabled, Device, Edits, LayoutDraft, Light, ModeChange, ServiceStart, Snapshot};
use super::panels::{self, Panels, Requests, Shared};
use super::theme::{self, AMBER, DEEP, INK, PANEL, RED};
use super::widgets::{paint_light, paint_name_and_state, segment, slide_switch, slider_row, small};
use crate::webui::UNKNOWN_ADDRESS;
use eframe::egui::{
    self, vec2, Align2, Color32, Frame, Key, Margin, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, Ui,
    ViewportCommand,
};
use std::ops::RangeInclusive;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WINDOW_SIZE: [f32; 2] = [1100.0, 760.0];
const MIN_WINDOW_SIZE: [f32; 2] = [760.0, 560.0];
const SWITCH_SIZE: [f32; 2] = [176.0, 44.0];
const HEADER_HEIGHT: f32 = 46.0;
const BAY_GAP: f32 = 16.0;
/// Below this width a device's place in the header says it has control in fewer words.
const ROOM_FOR_FULL_STATE: f32 = 190.0;
/// Below this width the arrangement's settings take two rows.
const THREE_SETTINGS_FROM: f32 = 620.0;

const EDGE_PUSH_RANGE: RangeInclusive<f64> = 10.0..=180.0;
/// How often to look at how starting the service is going.
const START_CHECK: Duration = Duration::from_millis(250);
const PEER_PROMPT_WIDTH: f32 = 380.0;
const MODE_PROMPT_WIDTH: f32 = 420.0;
const MODE_PROMPT_SECONDS: f32 = crate::displays::REVERT_AFTER.as_secs() as f32;

/// The app size a person can choose, on top of what the window's size does by itself.
const UI_SCALES: [f32; 7] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.4, 1.5];
const UI_SCALE_KEY: &str = "ui_scale";
const DEFAULT_UI_SCALE: f32 = 1.0;
/// How far the window's own zoom has to move before it changes, so a resize doesn't flicker.
const ZOOM_SLACK: f32 = 0.04;

/// Opens the window and returns when it closes. `starter` is getting the service running.
///
/// eframe remembers the window's size, and its place where the desktop allows that: Windows and
/// X11 do, and Wayland leaves it to the compositor. It also keeps egui's zoom, without which the
/// remembered size would come back divided by the zoom.
pub fn run_window(starter: ServiceStarter) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mesh-kvm")
            .with_app_id("mesh-kvm")
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_icon(super::icon::egui_icon()),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "mesh-kvm",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            super::single::serve_focus(cc.egui_ctx.clone());
            let ctx = cc.egui_ctx.clone();
            let client = Client::start_with_displays(super::client::WINDOW_POLL, Box::new(move || ctx.request_repaint()));
            let mut window = Window::new(client, starter);
            let saved = cc.storage.and_then(|s| eframe::get_value::<f32>(s, UI_SCALE_KEY));
            window.ui_scale = saved.map_or(DEFAULT_UI_SCALE, nearest_ui_scale);
            Ok(Box::new(window))
        }),
    )
    .map_err(|e| format!("The mesh-kvm window couldn't start: {e}"))
}

fn nearest_ui_scale(scale: f32) -> f32 {
    let distance = |s: &&f32| ((**s - scale).abs() * 1000.0) as u32;
    UI_SCALES.iter().min_by_key(distance).copied().unwrap_or(DEFAULT_UI_SCALE)
}

/// The next app size up or down from `scale`, stopping at the ends.
fn stepped_ui_scale(scale: f32, up: bool) -> f32 {
    let at = UI_SCALES.iter().position(|s| *s == nearest_ui_scale(scale)).unwrap_or(2);
    let to = if up { (at + 1).min(UI_SCALES.len() - 1) } else { at.saturating_sub(1) };
    UI_SCALES[to]
}

/// The box that asks for the other device's address.
#[derive(Default)]
struct PeerPrompt {
    /// Opened by hand. On a first run it is open whatever this says, and can't be closed.
    open: bool,
    address: String,
    /// True once the text box has been given the keyboard.
    focused: bool,
    saving: Option<Receiver<Result<(), String>>>,
    error: Option<String>,
}

struct Window {
    client: Arc<Client>,
    starter: ServiceStarter,
    peer_prompt: PeerPrompt,
    edits: Edits,
    layout: LayoutDraft,
    canvas: Canvas,
    panels: Panels,
    main_change: Option<Receiver<Result<(), String>>>,
    main_error: Option<String>,
    /// The device the failed change was for, and whether the link has been seen down since.
    main_choice: Option<Device>,
    main_link_was_down: bool,
    /// The person's choice of app size, and what the window's size adds to it.
    ui_scale: f32,
    window_zoom: f32,
    /// The smallest size last given to the window.
    smallest_sent: Option<(f32, f32)>,
    settings_height: f32,
}

impl eframe::App for Window {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snapshot = self.client.snapshot();
        self.show(ctx, &snapshot, Instant::now());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, UI_SCALE_KEY, &self.ui_scale);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        DEEP.to_normalized_gamma_f32()
    }
}

impl Window {
    fn new(client: Client, starter: ServiceStarter) -> Self {
        Self {
            client: Arc::new(client),
            starter,
            peer_prompt: PeerPrompt::default(),
            edits: Edits::default(),
            layout: LayoutDraft::default(),
            canvas: Canvas::default(),
            panels: Panels::default(),
            main_change: None,
            main_error: None,
            main_choice: None,
            main_link_was_down: false,
            ui_scale: DEFAULT_UI_SCALE,
            window_zoom: 1.0,
            smallest_sent: None,
            settings_height: 0.0,
        }
    }

    /// Sizes everything from the window and the person's choice. The smallest window grows with
    /// their choice, so nothing has to fit in less room than the layout was made for.
    fn apply_scale(&mut self, ctx: &egui::Context) {
        let keys = ctx.input_mut(|i| {
            let plus = i.consume_key(Modifiers::COMMAND, Key::Plus) || i.consume_key(Modifiers::COMMAND, Key::Equals);
            let minus = i.consume_key(Modifiers::COMMAND, Key::Minus);
            (plus, minus, i.consume_key(Modifiers::COMMAND, Key::Num0))
        });
        match keys {
            (true, ..) => self.ui_scale = stepped_ui_scale(self.ui_scale, true),
            (_, true, _) => self.ui_scale = stepped_ui_scale(self.ui_scale, false),
            (.., true) => self.ui_scale = DEFAULT_UI_SCALE,
            _ => {}
        }
        let window = ctx.screen_rect().size() * ctx.zoom_factor();
        let wanted = model::window_zoom(window.x, window.y);
        if (wanted - self.window_zoom).abs() > ZOOM_SLACK {
            self.window_zoom = wanted;
        }
        let zoom = self.ui_scale * self.window_zoom;
        ctx.set_zoom_factor(zoom);
        let monitor = ctx.input(|i| i.viewport().monitor_size).map(|size| (size.x, size.y));
        let smallest = model::smallest_window(MIN_WINDOW_SIZE.into(), self.ui_scale, zoom, monitor);
        if Some(smallest) != self.smallest_sent {
            self.smallest_sent = Some(smallest);
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(smallest.into()));
        }
    }

    fn show(&mut self, ctx: &egui::Context, snapshot: &Snapshot, now: Instant) {
        self.apply_scale(ctx);
        let on = model::enabled(snapshot);
        self.collect_main_change();
        self.collect_peer_change();
        self.clear_stale_main_error(snapshot, &on);
        let start = self.starter.state();
        self.layout.follow(snapshot.layout.clone(), self.canvas.is_dragging(), now);

        egui::TopBottomPanel::top("header")
            .show_separator_line(false)
            .frame(Frame::new().fill(PANEL).inner_margin(Margin::symmetric(theme::MARGIN, 10)))
            .show(ctx, |ui| self.header(ui, snapshot, &on));
        if let Some(message) = model::start_notice(&start, snapshot).or_else(|| model::status_message(snapshot)) {
            let fill = if model::notice_is_serious(snapshot, &start) { RED } else { AMBER };
            notice_strip(ctx, "notice", fill, |ui| {
                ui.label(RichText::new(message).color(DEEP));
            });
        }
        for (index, note) in Device::BOTH.into_iter().filter_map(|d| snapshot.device(d)?.display_change.note.as_ref()).enumerate() {
            notice_strip(ctx, if index == 0 { "first screen note" } else { "second screen note" }, AMBER, |ui| {
                ui.label(RichText::new(note).color(DEEP));
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

        let body = model::body_layout(ctx.screen_rect().width());
        egui::SidePanel::right("controls")
            .exact_width(body.rail)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(PANEL))
            .show(ctx, |ui| self.controls(ui, snapshot, &on, body.columns, now));
        let settings = egui::TopBottomPanel::bottom("arrangement settings")
            .show_separator_line(false)
            .frame(Frame::new().fill(DEEP).inner_margin(Margin { left: theme::MARGIN, right: theme::MARGIN, top: 0, bottom: 12 }))
            .show(ctx, |ui| self.arrangement_settings(ui, snapshot, &on, now));
        // A panel takes the height it had last frame, so a reflow needs one more frame to settle.
        let settings_height = settings.response.rect.height();
        if settings_height != self.settings_height {
            self.settings_height = settings_height;
            ctx.request_repaint();
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(DEEP).inner_margin(Margin { left: theme::MARGIN, right: theme::MARGIN, top: theme::MARGIN, bottom: 10 }))
            .show(ctx, |ui| self.canvas.show(ui, &mut self.layout, snapshot, now));

        let first_run = model::needs_peer(snapshot);
        if first_run || self.peer_prompt.open {
            self.peer_window(ctx, snapshot, first_run);
        }
        if let Some((device, change)) = snapshot.mode_change() {
            self.mode_prompt(ctx, snapshot, device, change);
        }

        self.canvas.end_lost_drag(ctx, &mut self.layout, now);
        self.send_changes(now);
        let wake = [self.edits.next_wake(now), self.layout.next_wake(now)].into_iter().flatten().min();
        if let Some(wake) = wake {
            ctx.request_repaint_after(wake);
        }
        if start == ServiceStart::Starting {
            ctx.request_repaint_after(START_CHECK);
        }
    }

    /// Asks whether to keep a new mode. Only the device with the screen can keep it, as only
    /// someone there can see it worked. The other device's window can still send it back. The
    /// service undoes the change by itself if nobody answers, so closing this window can't leave
    /// a blank screen.
    fn mode_prompt(&mut self, ctx: &egui::Context, snapshot: &Snapshot, device: Device, change: &ModeChange) {
        let (mut keep, mut undo) = (false, false);
        let waiting = change.is_waiting();
        let seconds = (change.ms_left as f32 / 1000.0).ceil() as u32;
        let target = change.to.as_ref().map_or_else(String::new, |m| m.label());
        let going_back = change.from.as_ref().map_or_else(String::new, |m| m.label());
        let screen = format!("{} on {}", change.name, snapshot.name(device));
        let here = snapshot.is_here(device);
        let elsewhere = format!("It can only be kept from {}.", snapshot.name(device));
        let frame = Frame::popup(&ctx.style()).inner_margin(Margin::same(18));
        egui::Window::new("Keep these screen settings?")
            .id(egui::Id::new("mode prompt"))
            .collapsible(false)
            .resizable(false)
            .frame(frame)
            .min_width(MODE_PROMPT_WIDTH)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match change.stage.as_str() {
                "applying" => {
                    ui.label(format!("Changing {screen} to {target}."));
                }
                "going_back" => {
                    ui.label(format!("Putting {screen} back to {going_back}."));
                }
                _ => {
                    ui.label(format!("{screen} is now {target}."));
                    ui.label(small(format!("It goes back to {going_back} in {seconds} seconds unless you keep it.")));
                    let share = change.ms_left as f32 / 1000.0 / MODE_PROMPT_SECONDS;
                    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                    ui.painter().rect_filled(bar, 2.0, DEEP);
                    let left = Rect::from_min_size(bar.min, vec2(bar.width() * share.clamp(0.0, 1.0), bar.height()));
                    ui.painter().rect_filled(left, 2.0, AMBER);
                    ui.add_space(4.0);
                    if !here {
                        ui.label(small(elsewhere));
                    }
                    ui.horizontal(|ui| {
                        if here {
                            let text = RichText::new("Keep this").color(DEEP);
                            keep = ui.add(egui::Button::new(text).fill(AMBER).stroke(Stroke::NONE)).clicked();
                        }
                        undo = ui.button("Go back now").clicked();
                    });
                }
            });
        if waiting && (keep || undo) {
            self.client.local_action(device, model::mode_answer(change, keep));
        }
    }

    fn collect_peer_change(&mut self) {
        let Some(change) = &self.peer_prompt.saving else { return };
        match change.try_recv() {
            Ok(Ok(())) => self.peer_prompt = PeerPrompt::default(),
            Ok(Err(reason)) => {
                self.peer_prompt.error = Some(reason);
                self.peer_prompt.saving = None;
                // Enter took the keyboard from the box, and the person has an address to fix.
                self.peer_prompt.focused = false;
            }
            Err(TryRecvError::Disconnected) => self.peer_prompt.saving = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Hands the address to the service on a background thread, as the UI must not wait for it.
    fn save_peer(&mut self, ctx: &egui::Context) {
        let (done, change) = mpsc::channel();
        let (client, ctx, address) = (self.client.clone(), ctx.clone(), self.peer_prompt.address.clone());
        std::thread::spawn(move || {
            let _ = done.send(client.set_peer(&address));
            ctx.request_repaint();
        });
        self.peer_prompt.error = None;
        self.peer_prompt.saving = Some(change);
    }

    fn peer_window(&mut self, ctx: &egui::Context, snapshot: &Snapshot, first_run: bool) {
        let this_address = snapshot.local.as_ref().map_or("", |l| l.ip.as_str());
        let saving = self.peer_prompt.saving.is_some();
        let (mut connect, mut cancel) = (false, false);
        egui::Window::new("Connect to your other device")
            .collapsible(false)
            .resizable(false)
            .default_width(PEER_PROMPT_WIDTH)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Enter the address of the other device. mesh-kvm shows it on that device, next to its name.");
                if !this_address.is_empty() && this_address != UNKNOWN_ADDRESS {
                    ui.label(small(format!("This device's address is {this_address}.")));
                }
                let edit = egui::TextEdit::singleline(&mut self.peer_prompt.address)
                    .hint_text("192.168.1.20")
                    .desired_width(ui.available_width());
                let field = ui.add_enabled(!saving, edit);
                if !self.peer_prompt.focused {
                    self.peer_prompt.focused = true;
                    field.request_focus();
                }
                let pressed_enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if let Some(error) = &self.peer_prompt.error {
                    ui.label(RichText::new(error).color(RED));
                }
                ui.horizontal(|ui| {
                    let ready = !saving && !self.peer_prompt.address.trim().is_empty();
                    let clicked = ui.add_enabled(ready, egui::Button::new("Connect")).clicked();
                    connect = clicked || (ready && pressed_enter);
                    if !first_run {
                        cancel = ui.add_enabled(!saving, egui::Button::new("Cancel")).clicked();
                    }
                    if saving {
                        ui.label(small("Saving the address."));
                    }
                });
            });
        if connect {
            self.save_peer(ctx);
        }
        if cancel {
            self.peer_prompt = PeerPrompt::default();
        }
    }

    fn send_changes(&mut self, now: Instant) {
        for ((device, control), value) in self.edits.take_due(now) {
            self.client.local_action(device, control.action(value));
        }
        if let Some(edit) = self.layout.take_due(now) {
            self.client.save_layout(edit);
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

    fn header(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled) {
        let [left, right] = snapshot.in_order();
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_HEIGHT), Sense::hover());
        // The two buttons sit at the end, and the devices and their switch share the rest.
        let buttons = ui.painter().layout_no_wrap("Release all input".into(), egui::FontId::proportional(theme::BODY_SIZE), INK).size().x
            + ui.painter().layout_no_wrap("Settings".into(), egui::FontId::proportional(theme::BODY_SIZE), INK).size().x
            + 4.0 * ui.spacing().button_padding.x
            + ui.spacing().item_spacing.x;
        let room = Rect::from_min_max(bar.min, Pos2::new(bar.right() - buttons - BAY_GAP, bar.bottom()));
        let switch_rect = Rect::from_center_size(room.center(), SWITCH_SIZE.into());
        let left_bay = Rect::from_min_max(room.min, Pos2::new(switch_rect.left() - BAY_GAP, room.bottom()));
        let right_bay = Rect::from_min_max(Pos2::new(switch_rect.right() + BAY_GAP, room.top()), room.max);
        for (device, bay, side) in [(left, left_bay, Align2::LEFT_CENTER), (right, right_bay, Align2::RIGHT_CENTER)] {
            device_bay(ui, bay, device, snapshot, side);
        }
        let active = snapshot.active();
        let side = active.map_or(0.5, |d| if d == left { 0.0 } else { 1.0 });
        let knob = active.map(|d| snapshot.title(d));
        let hint = match (on.switch, active) {
            (true, Some(device)) => format!("Switch keyboard and mouse to {}", snapshot.name(device.other())),
            _ => "The switch works while the devices are linked and agree on a main device.".to_string(),
        };
        if slide_switch(ui, switch_rect, side, knob.as_deref(), on.switch, hint).clicked() {
            self.client.master_action("toggle_focus");
        }
        let buttons = Rect::from_min_max(Pos2::new(bar.right() - buttons, bar.top()), bar.max);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(buttons), |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("Settings", |ui| self.settings_menu(ui, snapshot));
                ui.add_enabled_ui(on.release, |ui| {
                    let text = RichText::new("Release all input").color(DEEP);
                    let button = egui::Button::new(text).fill(RED).stroke(Stroke::NONE);
                    if ui.add(button).on_hover_text("Gives each device back its own keyboard and mouse.").clicked() {
                        self.client.release_all_input();
                    }
                });
            });
        });
    }

    fn settings_menu(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        ui.set_min_width(260.0);
        ui.label("App size");
        ui.horizontal_wrapped(|ui| {
            for scale in UI_SCALES {
                let text = format!("{:.0}%", scale * 100.0);
                if ui.selectable_label(self.ui_scale == scale, text).clicked() {
                    self.ui_scale = scale;
                }
            }
        });
        ui.label(small("Ctrl and plus or minus do the same."));
        if let Some(local) = snapshot.local.as_ref().filter(|l| l.paired == Some(true)) {
            ui.add_space(8.0);
            ui.label(format!("{}'s address", snapshot.title(snapshot.in_order()[1])));
            ui.label(small(&local.peer_ip));
            ui.add_enabled_ui(!local.peer_pinned, |ui| {
                if ui.button("Change address").clicked() {
                    self.peer_prompt.open = true;
                    ui.close_menu();
                }
            });
            if local.peer_pinned {
                ui.label(small("How mesh-kvm starts on this device sets the address, so it can't be changed here."));
            }
        }
    }

    /// Where the devices sit and which leads, under the map they belong to.
    fn arrangement_settings(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        let gap = theme::MARGIN as f32;
        let width = ui.available_width();
        let three = width >= THREE_SETTINGS_FROM;
        let column = if three { (width - 2.0 * gap) / 3.0 } else { (width - gap) / 2.0 };
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(column);
                    self.position_control(ui, snapshot, on, now);
                });
                ui.vertical(|ui| {
                    ui.set_width(column);
                    self.edge_push_control(ui, on, now);
                });
                if three {
                    ui.vertical(|ui| {
                        ui.set_width(column);
                        self.main_device_controls(ui, snapshot, on);
                    });
                }
            });
        });
        if !three {
            self.main_device_controls(ui, snapshot, on);
        }
    }

    fn position_control(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        let cfg = self.layout.state.as_ref().map(|s| s.config.clone()).unwrap_or_default();
        let has_layout = self.layout.state.is_some();
        let placements = model::placements(&cfg, snapshot.displays(Device::Debian));
        let selected = if has_layout { model::position_label(&cfg, &placements) } else { "Not available".to_string() };
        let mut chosen = None;
        ui.add_enabled_ui(on.layout, |ui| {
            ui.add(egui::Label::new(format!("Where {} sits", snapshot.name(Device::Windows))).truncate());
            egui::ComboBox::from_id_salt("position")
                .selected_text(selected.clone())
                .width(ui.available_width())
                .truncate()
                .show_ui(ui, |ui| {
                    for place in &placements {
                        if ui.selectable_label(place.label == selected, &place.label).clicked() {
                            chosen = Some(place);
                        }
                    }
                });
            if cfg.has_custom_size() && ui.button("Use the real screen sizes").clicked() {
                self.client.master_action("reset_native_resolutions");
                self.layout.let_poll_through();
            }
        });
        let Some(place) = chosen else { return };
        if let Some(state) = &mut self.layout.state {
            model::apply_placement(&mut state.config, place);
        }
        self.layout.edited(now);
    }

    fn edge_push_control(&mut self, ui: &mut Ui, on: &ControlsEnabled, now: Instant) {
        let current = self.layout.state.as_ref().map(|s| s.config.edge_resistance);
        let mut push = current.unwrap_or_default() as f64;
        let text = current.map_or_else(String::new, |v| v.to_string());
        let moved = ui.add_enabled_ui(on.layout, |ui| slider_row(ui, "Push needed to cross", text, &mut push, EDGE_PUSH_RANGE, 5.0)).inner;
        if !moved {
            return;
        }
        if let Some(state) = &mut self.layout.state {
            state.config.edge_resistance = push as i32;
        }
        self.layout.edited(now);
    }

    fn main_device_controls(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled) {
        let changing = self.main_change.is_some();
        let current = snapshot.main_device();
        let mut chosen = None;
        ui.label("Main device").on_hover_text("It takes the keyboard and mouse when the devices link up.");
        ui.add_enabled_ui(on.main_device && !changing, |ui| {
            let gap = 2.0;
            let width = (ui.available_width() - gap) / 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for device in snapshot.in_order() {
                    if segment(ui, width, &snapshot.title(device), current == Some(device)).clicked() && current != Some(device) {
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
        }
    }

    /// Each device's sections: side by side with room for both, else one above the other.
    fn controls(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled, columns: usize, now: Instant) {
        let mut requests = Requests::default();
        let mut shared =
            Shared { edits: &mut self.edits, layout: &self.layout, canvas: &mut self.canvas, snapshot, on, now };
        let panels = &mut self.panels;
        let [first, second] = snapshot.in_order();
        ScrollArea::vertical().auto_shrink(false).drag_to_scroll(false).show(ui, |ui| {
            Frame::new().inner_margin(Margin::same(theme::MARGIN)).show(ui, |ui| {
                if columns >= 2 {
                    let gap = 2.0 * theme::MARGIN as f32;
                    let width = (ui.available_width() - gap) / 2.0;
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for device in [first, second] {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                ui.set_width(width);
                                panels.device(ui, device, &mut shared, &mut requests);
                            });
                        }
                    });
                } else {
                    panels.device(ui, first, &mut shared, &mut requests);
                    panels::device_divider(ui);
                    panels.device(ui, second, &mut shared, &mut requests);
                }
            });
        });
        self.send_requests(requests, now);
    }

    fn send_requests(&mut self, requests: Requests, now: Instant) {
        for (device, action) in requests.actions {
            self.client.local_action(device, action);
        }
        let (auto, manual) = (requests.toggle_auto_game_lock, requests.toggle_manual_game_lock);
        if !auto && !manual {
            return;
        }
        if let Some(state) = &mut self.layout.state {
            state.config.auto_game_lock ^= auto;
            state.config.manual_game_lock ^= manual;
        }
        if auto {
            self.client.master_action("toggle_auto_game_lock");
        }
        if manual {
            self.client.master_action("toggle_game_lock");
        }
        self.layout.hold(now);
    }
}

fn notice_strip(ctx: &egui::Context, id: &'static str, fill: Color32, contents: impl FnOnce(&mut Ui)) {
    egui::TopBottomPanel::top(id)
        .show_separator_line(false)
        .frame(Frame::new().fill(fill).inner_margin(Margin::symmetric(theme::MARGIN, 8)))
        .show(ctx, contents);
}

/// A device's name and state in the header, against one side of `rect`.
fn device_bay(ui: &Ui, rect: Rect, device: Device, snapshot: &Snapshot, side: Align2) {
    let (light, state) = model::device_badge(snapshot, device);
    let state = if rect.width() < ROOM_FOR_FULL_STATE && light == Light::Active { "Has control" } else { state };
    let painter = ui.painter().with_clip_rect(rect);
    let on_left = side == Align2::LEFT_CENTER;
    let light_x = if on_left { rect.left() + 8.0 } else { rect.right() - 8.0 };
    paint_light(&painter, Pos2::new(light_x, rect.center().y), light);
    let text = if on_left { rect.with_min_x(rect.left() + 26.0) } else { rect.with_max_x(rect.right() - 26.0) };
    paint_name_and_state(&painter, text, &snapshot.title(device), state, side);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::canvas::monitor_id;
    use crate::app::model::{Control, DeviceStatus, LayoutState};
    use crate::app::panels::device_details;
    use crate::platform::{DisplayInfo, DisplayMode};
    use eframe::egui::{Event, PointerButton, RawInput};

    const STATE: &str = r#"{"config":{"devices":[{"node":"Debian","x":0,"y":0,"monitors":[{"name":"DP-3 (240Hz Native)","x":0,"y":360,"w":1920,"h":1080},{"name":"DP-2 (360Hz Native)","x":1920,"y":0,"w":2560,"h":1440}]},{"node":"Windows","x":1920,"y":1440,"monitors":[{"name":"Display 1","x":0,"y":0,"w":2560,"h":1600}]}],"attach_side":"Bottom","edge_resistance":55,"auto_game_lock":true,"manual_game_lock":false},"telemetry":{"active_node":"Debian","peer_online":true}}"#;

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
        d.audio.mute_while_sending = Some(true);
        d
    }

    fn linked() -> Snapshot {
        Snapshot {
            local: Some(device("debian", Some(0))),
            peer: Some(device("windows", None)),
            layout: LayoutState::parse(STATE),
            ..Default::default()
        }
    }

    fn states() -> Vec<Snapshot> {
        let mut no_peer = linked();
        no_peer.peer = None;
        let laptop_alone = Snapshot { local: Some(device("windows", None)), layout: LayoutState::parse(STATE), ..Default::default() };
        let no_layout = Snapshot { local: Some(device("windows", None)), ..Default::default() };
        let mut split = linked();
        split.peer.as_mut().unwrap().main_device = 1;
        split.local.as_mut().unwrap().main_conflict = true;
        let mut older = linked();
        older.peer.as_mut().unwrap().sharing = None;
        older.peer.as_mut().unwrap().audio.mute_while_sending = None;
        let mut with_modes = linked();
        with_modes.local_displays = vec![display("DP-3"), display("DP-2")];
        with_modes.peer_displays = vec![display("Display 1")];
        let mut changing = with_modes.clone();
        changing.local.as_mut().unwrap().display_change.change = Some(waiting_change());
        vec![Snapshot::default(), linked(), no_peer, laptop_alone, no_layout, split, older, with_modes, changing]
    }

    fn mode(width: i32, height: i32, millihertz: u32) -> DisplayMode {
        DisplayMode { width, height, millihertz, id: String::new() }
    }

    fn display(id: &str) -> DisplayInfo {
        let modes = vec![mode(2560, 1440, 144_000), mode(1920, 1080, 60_000)];
        DisplayInfo { id: id.into(), name: format!("{id} model"), current: Some(modes[0].clone()), modes, can_change: true, ..Default::default() }
    }

    fn waiting_change() -> ModeChange {
        ModeChange {
            id: 7,
            output: "DP-2".into(),
            name: "Big".into(),
            from: Some(mode(2560, 1440, 144_000)),
            to: Some(mode(1920, 1080, 60_000)),
            stage: "waiting".into(),
            ms_left: 12_000,
        }
    }

    fn window() -> Window {
        // Nothing listens here, so whatever the tests send goes nowhere.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        Window::new(Client::start_on(port, Box::new(|| {})), ServiceStarter::default())
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

    /// Settles the layout, then returns a monitor's rectangle on screen.
    fn monitor_on_screen(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, device: Device, index: usize, now: Instant) -> Rect {
        for _ in 0..3 {
            frame(ctx, window, snapshot, WINDOW_SIZE, vec![], now);
        }
        ctx.read_response(monitor_id(device, index)).unwrap().rect
    }

    #[test]
    fn draws_every_state_from_the_smallest_window_to_full_screen() {
        for size in [MIN_WINDOW_SIZE, WINDOW_SIZE, [1920.0, 1080.0], [3840.0, 2160.0]] {
            for snapshot in states() {
                let (ctx, mut window) = (context(), window());
                for _ in 0..3 {
                    frame(&ctx, &mut window, &snapshot, size, vec![], Instant::now());
                }
            }
        }
    }

    #[test]
    fn the_arrangement_fills_the_height_and_stays_clear_of_the_controls() {
        for size in [MIN_WINDOW_SIZE, WINDOW_SIZE] {
            let (ctx, mut window, snapshot) = (context(), window(), linked());
            for _ in 0..4 {
                frame(&ctx, &mut window, &snapshot, size, vec![], Instant::now());
            }
            let screen = ctx.screen_rect();
            for device in Device::BOTH {
                let rect = ctx.read_response(monitor_id(device, 0)).unwrap().rect;
                let rail = model::body_layout(screen.width()).rail;
                assert!(rect.right() <= screen.right() - rail, "{size:?} {rect:?}");
                assert!(rect.left() >= 0.0 && rect.bottom() <= screen.bottom(), "{size:?} {rect:?}");
            }
        }
    }

    #[test]
    fn a_big_window_is_enlarged_on_top_of_the_persons_choice() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        assert_eq!(window.window_zoom, 1.0);
        window.ui_scale = 1.25;
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        assert_eq!(ctx.zoom_factor(), 1.25);
    }

    #[test]
    fn app_size_steps_stop_at_the_ends_and_a_saved_oddity_is_rounded() {
        assert_eq!(stepped_ui_scale(1.0, true), 1.1);
        assert_eq!(stepped_ui_scale(1.0, false), 0.9);
        assert_eq!(stepped_ui_scale(1.5, true), 1.5);
        assert_eq!(stepped_ui_scale(0.8, false), 0.8);
        assert_eq!(nearest_ui_scale(1.27), 1.25);
        assert_eq!(nearest_ui_scale(99.0), 1.5);
    }

    #[test]
    fn a_waiting_mode_change_shows_the_prompt_on_either_devices_window() {
        for this_device_changes in [true, false] {
            let (ctx, mut window) = (context(), window());
            let mut snapshot = linked();
            let target = if this_device_changes { snapshot.local.as_mut() } else { snapshot.peer.as_mut() };
            target.unwrap().display_change.change = Some(waiting_change());
            frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
            frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
            assert!(ctx.memory(|m| m.area_rect(egui::Id::new("mode prompt"))).is_some());
        }
    }

    /// The text of every shape drawn this frame.
    fn drawn_text(output: &egui::FullOutput) -> String {
        let mut text = String::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(shape) = &clipped.shape {
                text.push_str(shape.galley.text());
                text.push('\n');
            }
        }
        text
    }

    #[test]
    fn keep_is_offered_only_on_the_device_with_the_screen() {
        for (here, keeps) in [(true, true), (false, false)] {
            let (ctx, mut window) = (context(), window());
            let mut snapshot = linked();
            let target = if here { snapshot.local.as_mut() } else { snapshot.peer.as_mut() };
            target.unwrap().display_change.change = Some(waiting_change());
            frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
            let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
            assert_eq!(text.contains("Keep this"), keeps, "{text}");
            assert!(text.contains("Go back now"), "{text}");
            assert_eq!(text.contains("can only be kept from"), !keeps, "{text}");
        }
    }

    #[test]
    fn a_screen_with_no_working_mode_list_shows_its_mode_and_says_so() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        // The desktop answered nothing, so only the layout's own numbers are left.
        snapshot.local_displays.clear();
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("2560 \u{d7} 1440 at 360 Hz"), "{text}");
        assert!(text.contains("can't be changed from here right now"), "{text}");
        // Listed, but only for reading.
        let mut listed = display("DP-3");
        listed.can_change = false;
        snapshot.local_displays = vec![listed];
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("can't be changed from here right now"), "{text}");
    }

    fn unpaired() -> Snapshot {
        let mut local = device("windows", None);
        local.paired = Some(false);
        local.ip = "192.168.8.241".into();
        Snapshot { local: Some(local), layout: LayoutState::parse(STATE), ..Default::default() }
    }

    #[test]
    fn a_first_run_asks_for_the_address_and_a_failure_keeps_the_box_open() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (unpaired(), Instant::now());
        for size in [MIN_WINDOW_SIZE, WINDOW_SIZE] {
            frame(&ctx, &mut window, &snapshot, size, vec![], now);
        }
        assert!(window.peer_prompt.focused, "the box should be up, ready to type in");
        window.peer_prompt.address = "192.168.8.210".into();
        window.save_peer(&ctx);
        assert!(window.peer_prompt.saving.is_some(), "the save runs off the UI thread");
        for _ in 0..200 {
            frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
            if window.peer_prompt.saving.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Nothing listens in this test, so the service can't take it.
        assert!(window.peer_prompt.error.as_deref().unwrap().contains("didn't answer"));
        assert_eq!(window.peer_prompt.address, "192.168.8.210", "what was typed stays");
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert!(window.peer_prompt.focused, "the box has the keyboard again, to fix the address");
    }

    #[test]
    fn a_paired_device_shows_no_box_until_asked() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        snapshot.local.as_mut().unwrap().paired = Some(true);
        snapshot.local.as_mut().unwrap().peer_ip = "192.168.8.210".into();
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        assert!(!window.peer_prompt.focused);
        window.peer_prompt.open = true;
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        assert!(window.peer_prompt.focused);
    }

    #[test]
    fn a_failed_start_is_shown_and_starting_keeps_the_window_looking() {
        let (ctx, _) = (context(), ());
        let client = Client::start_on(
            std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(),
            Box::new(|| {}),
        );
        let starter = ServiceStarter::begin_for_test(ServiceStart::Starting);
        let mut window = Window::new(client, starter.clone());
        let output = frame(&ctx, &mut window, &Snapshot::default(), WINDOW_SIZE, vec![], Instant::now());
        let repaint = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert!(repaint <= START_CHECK, "{repaint:?}");
        starter.finish_for_test("mesh-kvm didn't start on this device.");
        assert!(matches!(window.starter.state(), ServiceStart::Failed(_)));
        frame(&ctx, &mut window, &Snapshot::default(), WINDOW_SIZE, vec![], Instant::now());
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
        window.canvas.start_test_drag(&snapshot.layout.as_ref().unwrap().config);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert!(!window.canvas.is_dragging());
    }

    /// Drags a monitor by `by` on screen, and returns the window still holding it.
    fn hold_and_drag(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, rect: Rect, by: egui::Vec2, now: Instant) -> Pos2 {
        let start = rect.center();
        let end = start + by;
        let mut step = |events| {
            frame(ctx, window, snapshot, WINDOW_SIZE, events, now);
        };
        step(vec![Event::PointerMoved(start)]);
        step(vec![pointer(start, true)]);
        step(vec![Event::PointerMoved(start + by / 2.0)]);
        step(vec![Event::PointerMoved(end)]);
        end
    }

    fn box_of(window: &Window, device: Device) -> (i32, i32, i32, i32) {
        window.layout.state.as_ref().unwrap().config.device_box(device)
    }

    #[test]
    fn dragging_a_monitor_snaps_its_device_and_ignores_the_poll_until_release() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (linked(), Instant::now());
        let screen = monitor_on_screen(&ctx, &mut window, &snapshot, Device::Windows, 0, now);
        let end = hold_and_drag(&ctx, &mut window, &snapshot, screen, vec2(60.0, 0.0), now);
        assert!(window.canvas.is_dragging(), "the device should be held");
        let held = box_of(&window, Device::Windows).0;
        assert!(held > 1920, "it should have moved, not {held}");

        // A poll arrives saying it is where it was, and the view stays frozen.
        let view = window.canvas.test_view();
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert_eq!(box_of(&window, Device::Windows).0, held);
        assert_eq!(window.canvas.test_view(), view);

        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![pointer(end, false)], now);
        assert!(!window.canvas.is_dragging());
        // Held for a moment after release, so the old poll doesn't undo the move.
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert_eq!(box_of(&window, Device::Windows).0, held);
    }

    #[test]
    fn every_monitor_of_both_devices_can_be_dragged() {
        for (device, index) in [(Device::Debian, 0), (Device::Debian, 1), (Device::Windows, 0)] {
            let (ctx, mut window) = (context(), window());
            let (snapshot, now) = (linked(), Instant::now());
            let gap = |w: &Window| {
                let (a, b) = (box_of(w, Device::Debian), box_of(w, Device::Windows));
                (b.0 - a.0, b.1 - a.1)
            };
            let screen = monitor_on_screen(&ctx, &mut window, &snapshot, device, index, now);
            let before = gap(&window);
            hold_and_drag(&ctx, &mut window, &snapshot, screen, vec2(50.0, 0.0), now);
            assert_ne!(gap(&window), before, "{device:?} {index}");
        }
    }

    #[test]
    fn a_click_on_a_monitor_selects_it_and_does_not_move_it() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (linked(), Instant::now());
        let screen = monitor_on_screen(&ctx, &mut window, &snapshot, Device::Debian, 1, now);
        let before = box_of(&window, Device::Windows);
        let at = screen.center();
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![Event::PointerMoved(at)], now);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![pointer(at, true)], now);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![pointer(at, false)], now);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert_eq!(window.canvas.selected.as_ref().map(|k| k.0), Some(Device::Debian));
        assert_eq!(box_of(&window, Device::Windows), before);
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
            w.main_error = Some("Couldn't reach it.".into());
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
    fn device_details_name_this_device() {
        let d = device("debian", None);
        assert_eq!(device_details(Some(&d), true), "This device, debian-host, 192.168.8.2");
        assert_eq!(device_details(Some(&d), false), "debian-host, 192.168.8.2");
        assert_eq!(device_details(None, false), "Not connected");
    }
}
