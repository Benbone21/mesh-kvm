//! The app window. It draws what the `Client` last polled and sends what the person changes. The
//! decisions live in `model`; the arrangement is in `canvas`, and the device controls in `panels`.

use super::canvas::Canvas;
use super::client::{Client, PairingReply};
use super::launch::ServiceStarter;
use super::model::{self, ControlsEnabled, Device, Edits, LayoutDraft, Light, ModeChange, ServiceStart, Snapshot};
use super::pairing::{self, PairingInput, PairingRequest, PairingView};
use super::panels::{self, Panels, Requests, Shared};
use super::theme::{self, legacy::{AMBER, DEEP, INK, PANEL, RAISED, RED}};
use super::updates::{self, Banner, VersionWatch};
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

const LICENCES_WIDTH: f32 = 600.0;
const RELEASE_NOTES: &str = "Release notes";

/// How long the pairing window says it worked before it goes.
const PAIRED_SHOWN: Duration = Duration::from_secs(2);

/// The pairing window, and the things waiting on the service for it.
#[derive(Default)]
struct PairingWindow {
    open: bool,
    input: PairingInput,
    /// A show or a code with the service. Pairing can take seconds, so it runs off the UI thread.
    pending: Option<Receiver<PairingReply>>,
    /// When the status first said it was paired.
    paired_at: Option<Instant>,
    unpairing: Option<Receiver<PairingReply>>,
    unpair_error: Option<String>,
}

struct Window {
    client: Arc<Client>,
    starter: ServiceStarter,
    peer_prompt: PeerPrompt,
    pairing: PairingWindow,
    /// The Settings menu was drawn this frame, so it is open and wants the fingerprint.
    pairing_in_menu: bool,
    show_licences: bool,
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
    /// The last line about an update under way, for while its service can't be seen.
    update_under_way: Option<Banner>,
    /// The version the device that's behind was last told to look for.
    nudged_for: Option<String>,
    version_watch: VersionWatch,
    /// A Windows copy run from somewhere other than where mesh-kvm is installed.
    not_installed: bool,
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
            pairing: PairingWindow::default(),
            pairing_in_menu: false,
            show_licences: false,
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
            update_under_way: None,
            nudged_for: None,
            version_watch: VersionWatch::default(),
            not_installed: cfg!(target_os = "windows") && !super::install::runs_from_installed(),
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
        self.collect_pairing();
        self.pairing_in_menu = false;
        self.clear_stale_main_error(snapshot, &on);
        let start = self.starter.state();
        self.layout.follow(snapshot.layout.clone(), self.canvas.is_dragging(), now);

        egui::TopBottomPanel::top("header")
            .show_separator_line(false)
            .frame(Frame::new().fill(PANEL).inner_margin(Margin::symmetric(theme::legacy::MARGIN, 10)))
            .show(ctx, |ui| self.header(ui, snapshot, &on));
        let updating = self.follow_update(ctx, snapshot);
        // A device dropping out is what an update looks like, and the line above says so.
        let notice = if updating { None } else { model::start_notice(&start, snapshot).or_else(|| model::status_message(snapshot)) };
        if let Some(message) = notice {
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
            .frame(Frame::new().fill(DEEP).inner_margin(Margin { left: theme::legacy::MARGIN, right: theme::legacy::MARGIN, top: 0, bottom: 12 }))
            .show(ctx, |ui| self.arrangement_settings(ui, snapshot, &on, now));
        // A panel takes the height it had last frame, so a reflow needs one more frame to settle.
        let settings_height = settings.response.rect.height();
        if settings_height != self.settings_height {
            self.settings_height = settings_height;
            ctx.request_repaint();
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(DEEP).inner_margin(Margin { left: theme::legacy::MARGIN, right: theme::legacy::MARGIN, top: theme::legacy::MARGIN, bottom: 10 }))
            .show(ctx, |ui| self.canvas.show(ui, &mut self.layout, snapshot, now));

        let first_run = model::needs_peer(snapshot);
        self.licences_window(ctx);
        if first_run || self.peer_prompt.open {
            self.peer_window(ctx, snapshot, first_run);
        }
        if let Some((device, change)) = snapshot.mode_change() {
            self.mode_prompt(ctx, snapshot, device, change);
        }
        self.pairing_window(ctx, snapshot, now);
        // The menu is drawn inside the header, so only now is it known whether it was open.
        self.client.want_pairing(self.pairing.open || self.pairing_in_menu);

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

    /// The update line, and what goes with it. A service that has just updated means this window
    /// is the old version, so it starts again from the new exe. True while an update is under way.
    fn follow_update(&mut self, ctx: &egui::Context, snapshot: &Snapshot) -> bool {
        let own = crate::update::trust::running_version().to_string();
        if self.version_watch.moved_on(&own, snapshot.local.as_ref().map(|l| l.version.as_str())) {
            match super::launch::relaunch("--app") {
                Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Err(e) => crate::settings::log_note(&format!("Couldn't reopen the window after updating: {e}")),
            }
        }
        if let Some((device, version)) = updates::device_to_nudge(snapshot) {
            if self.nudged_for.as_ref() != Some(&version) {
                self.client.local_action(device, serde_json::json!({ "action": "check_update" }));
                self.nudged_for = Some(version);
            }
        }
        let carried = self.update_under_way.clone().filter(|_| snapshot.local.is_none());
        let banner = updates::with_install_hint(updates::update_banner(snapshot).or(carried), self.not_installed);
        self.update_under_way = banner.clone().filter(|b| b.busy);
        if let Some(banner) = &banner {
            self.update_strip(ctx, snapshot, banner);
        }
        self.update_under_way.is_some()
    }

    /// One line at the top: what's happening, with the button and the release notes at its end.
    fn update_strip(&self, ctx: &egui::Context, snapshot: &Snapshot, banner: &Banner) {
        let quiet = banner.button.is_none() && !banner.busy && !banner.failed;
        let (fill, ink) = match (banner.failed, quiet) {
            (true, _) => (RED, DEEP),
            (_, true) => (RAISED, INK),
            _ => (AMBER, DEEP),
        };
        let mut pressed = None;
        // A gap below keeps it apart from a notice of the same colour.
        let frame = Frame::new().fill(fill).inner_margin(Margin::symmetric(theme::legacy::MARGIN, 8)).outer_margin(Margin { bottom: 2, ..Default::default() });
        egui::TopBottomPanel::top("update").show_separator_line(false).frame(frame).show(ctx, |ui| {
            let text_size = |ui: &Ui, text: &str| ui.painter().layout_no_wrap(text.into(), egui::FontId::proportional(theme::legacy::BODY_SIZE), INK).size().x;
            let gap = ui.spacing().item_spacing.x;
            let button = banner.button.map_or(0.0, |b| text_size(ui, b.label()) + 2.0 * ui.spacing().button_padding.x + gap);
            let notes = banner.notes.as_ref().map_or(0.0, |_| text_size(ui, RELEASE_NOTES) + gap);
            let text_width = (ui.available_width() - button - notes).max(0.0);
            ui.horizontal(|ui| {
                ui.allocate_ui(vec2(text_width, 0.0), |ui| {
                    ui.set_width(text_width);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(&banner.text).color(ink));
                        if let Some(detail) = &banner.detail {
                            ui.label(small(detail).color(ink));
                        }
                    });
                });
                if let Some(button) = banner.button {
                    let label = RichText::new(button.label()).color(INK);
                    if ui.add(egui::Button::new(label).fill(DEEP)).clicked() {
                        pressed = Some(button);
                    }
                }
                if let Some(version) = &banner.notes {
                    ui.visuals_mut().hyperlink_color = ink;
                    ui.hyperlink_to(RELEASE_NOTES, updates::notes_url(version));
                }
            });
        });
        if let (Some(button), Some(here)) = (pressed, snapshot.this_device()) {
            self.client.local_action(here, button.action());
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

    /// Starts a pairing action on a background thread, as the service can take seconds to answer.
    fn send_pairing(&self, ctx: &egui::Context, action: serde_json::Value) -> Receiver<PairingReply> {
        let (done, reply) = mpsc::channel();
        let (client, ctx) = (self.client.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = done.send(client.pairing_action(action));
            ctx.request_repaint();
        });
        reply
    }

    /// Cancels in the background, for when nobody waits to hear.
    fn cancel_pairing(&self) {
        let client = self.client.clone();
        std::thread::spawn(move || {
            client.pairing_action(serde_json::json!({ "action": "pair_cancel" }));
        });
    }

    fn open_pairing(&mut self, snapshot: &Snapshot) {
        self.pairing = PairingWindow { open: true, ..PairingWindow::default() };
        // An attempt that failed before is over, and shouldn't be the first thing to see.
        if snapshot.pairing.as_ref().is_some_and(|p| p.state == "failed") {
            self.cancel_pairing();
        }
    }

    fn collect_pairing(&mut self) {
        if let Some(reply) = self.pairing.pending.as_ref().map(Receiver::try_recv) {
            match reply {
                Ok(PairingReply::Refused(why)) => {
                    self.pairing.input.refusal = Some(why);
                    self.pairing.pending = None;
                }
                // The status says how it went, and a service that is restarting says nothing.
                Ok(_) | Err(TryRecvError::Disconnected) => self.pairing.pending = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(reply) = self.pairing.unpairing.as_ref().map(Receiver::try_recv) {
            match reply {
                Ok(PairingReply::Refused(why)) => {
                    self.pairing.unpair_error = Some(why);
                    self.pairing.unpairing = None;
                }
                Ok(_) | Err(TryRecvError::Disconnected) => self.pairing.unpairing = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    fn apply_pairing(&mut self, ctx: &egui::Context, snapshot: &Snapshot, request: Option<PairingRequest>) {
        match request {
            Some(PairingRequest::Show) => {
                self.pairing.input.refusal = None;
                self.pairing.pending = Some(self.send_pairing(ctx, serde_json::json!({ "action": "pair_show" })));
            }
            Some(PairingRequest::Enter(code)) => {
                self.pairing.input.refusal = None;
                let action = serde_json::json!({ "action": "pair_enter", "code": code });
                self.pairing.pending = Some(self.send_pairing(ctx, action));
            }
            Some(PairingRequest::Cancel) => {
                let busy = snapshot.pairing.as_ref().is_some_and(|p| matches!(p.state.as_str(), "showing" | "entering" | "failed"));
                if busy || self.pairing.pending.is_some() {
                    self.cancel_pairing();
                }
                self.pairing = PairingWindow::default();
            }
            None => {}
        }
    }

    fn pairing_window(&mut self, ctx: &egui::Context, snapshot: &Snapshot, now: Instant) {
        if !self.pairing.open {
            return;
        }
        // The service restarts to pair, so the first thing seen after may be a peer already there.
        let paired = snapshot.pairing.as_ref().is_some_and(|p| p.state == "paired" || p.peer_name.is_some());
        if paired && self.pairing.paired_at.is_none() {
            self.pairing.paired_at = Some(now);
        }
        if let Some(at) = self.pairing.paired_at {
            match PAIRED_SHOWN.checked_sub(now.saturating_duration_since(at)).filter(|left| !left.is_zero()) {
                Some(left) => ctx.request_repaint_after(left),
                None => {
                    self.pairing = PairingWindow::default();
                    return;
                }
            }
        }
        let other = snapshot.name(snapshot.in_order()[1]);
        let here = snapshot.name(snapshot.in_order()[0]);
        let address = snapshot.local.as_ref().map(|l| l.peer_ip.as_str()).filter(|a| !a.is_empty() && *a != UNKNOWN_ADDRESS);
        let state = snapshot.pairing.as_ref().map(|p| p.state.as_str());
        let working = self.pairing.pending.is_some() || state == Some("entering");
        let view = PairingView { status: snapshot.pairing.as_ref(), other: &other, this_device: &here, other_address: address, working };
        let request = pairing::window(ctx, &view, &mut self.pairing.input);
        self.apply_pairing(ctx, snapshot, request);
        // The countdown on a code moves each second.
        if state == Some("showing") {
            ctx.request_repaint_after(Duration::from_millis(500));
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
        let buttons = ui.painter().layout_no_wrap("Release all input".into(), egui::FontId::proportional(theme::legacy::BODY_SIZE), INK).size().x
            + ui.painter().layout_no_wrap("Settings".into(), egui::FontId::proportional(theme::legacy::BODY_SIZE), INK).size().x
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
        self.pairing_settings(ui, snapshot);
        if let Some(update) = snapshot.local.as_ref().and_then(|l| l.update.as_ref()) {
            ui.add_space(8.0);
            ui.label("Updates");
            ui.label(small(format!("This is version {}.", update.current)));
            if ui.button("Check now").clicked() {
                self.client.local_action(snapshot.in_order()[0], serde_json::json!({ "action": "check_update" }));
            }
            if let Some(note) = &update.note {
                ui.label(small(note));
            }
        }
        ui.add_space(8.0);
        if ui.button("Licences").clicked() {
            self.show_licences = true;
            ui.close_menu();
        }
    }

    /// Whether the devices are paired, in the Settings menu. A service that doesn't encrypt has nothing here.
    fn pairing_settings(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        let Some(local) = snapshot.local.as_ref().filter(|l| !l.secure.is_empty()) else { return };
        self.pairing_in_menu = true;
        ui.add_space(8.0);
        if local.secure == "unpaired" {
            if ui.button("Pair").clicked() {
                self.open_pairing(snapshot);
                ui.close_menu();
            }
            return;
        }
        let other = snapshot.title(snapshot.in_order()[1]);
        ui.label(format!("Paired with {other}"));
        ui.label(small(model::paired_line(&local.secure, &other)));
        if let Some(fingerprint) = snapshot.pairing.as_ref().and_then(|p| p.fingerprint.as_deref()) {
            ui.label(small(format!("Fingerprint, the same on both: {fingerprint}")));
        }
        let unpairing = self.pairing.unpairing.is_some();
        if ui.add_enabled(!unpairing, egui::Button::new("Unpair")).clicked() {
            self.pairing.unpair_error = None;
            self.pairing.unpairing = Some(self.send_pairing(ui.ctx(), serde_json::json!({ "action": "unpair" })));
        }
        if unpairing {
            ui.label(small("Still working on it."));
        }
        if let Some(error) = &self.pairing.unpair_error {
            ui.label(small(error).color(RED));
        }
    }

    fn licences_window(&mut self, ctx: &egui::Context) {
        // Wide enough for the licence's own line breaks, as long as the window allows it.
        let width = LICENCES_WIDTH.min(ctx.screen_rect().width() - 2.0 * theme::legacy::MARGIN as f32);
        egui::Window::new("Licences").open(&mut self.show_licences).collapsible(false).default_width(width).show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(ctx.screen_rect().height() * 0.7).show(ui, |ui| {
                ui.label(small(crate::licenses::TEXT));
            });
        });
    }

    /// Where the devices sit and which leads, under the map they belong to.
    fn arrangement_settings(&mut self, ui: &mut Ui, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        let gap = theme::legacy::MARGIN as f32;
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
            Frame::new().inner_margin(Margin::same(theme::legacy::MARGIN)).show(ui, |ui| {
                if columns >= 2 {
                    let gap = 2.0 * theme::legacy::MARGIN as f32;
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
        .frame(Frame::new().fill(fill).inner_margin(Margin::symmetric(theme::legacy::MARGIN, 8)))
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
    use crate::app::fixtures::*;
    use crate::app::model::Control;
    use crate::app::panels::device_details;
    use eframe::egui::{Event, PointerButton, RawInput};

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
    fn the_update_line_fits_the_smallest_window_with_its_button_and_link() {
        for snapshot in update_states() {
            let (ctx, mut window) = (context(), window());
            frame(&ctx, &mut window, &snapshot, MIN_WINDOW_SIZE, vec![], Instant::now());
            let output = frame(&ctx, &mut window, &snapshot, MIN_WINDOW_SIZE, vec![], Instant::now());
            let banner = updates::update_banner(&snapshot).unwrap();
            let text = drawn_text(&output);
            if let Some(button) = banner.button {
                assert!(text.contains(button.label()), "{text}");
            }
            assert_eq!(text.contains(RELEASE_NOTES), banner.notes.is_some(), "{text}");
            let strip = ctx.screen_rect().width();
            let widest = output.shapes.iter().filter_map(|c| match &c.shape {
                egui::Shape::Text(shape) if banner.text.starts_with(shape.galley.text().get(..4).unwrap_or("?")) => Some(shape.visual_bounding_rect().right()),
                _ => None,
            });
            assert!(widest.fold(0.0, f32::max) <= strip, "{}", banner.text);
        }
    }

    #[test]
    fn the_line_stays_while_the_service_it_is_updating_is_gone() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        snapshot.local.as_mut().unwrap().update = Some(update("installing", true));
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let gone = Snapshot::default();
        frame(&ctx, &mut window, &gone, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &gone, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("Updating this device"), "{text}");
    }

    #[test]
    fn a_device_dropping_out_during_an_update_raises_no_alarm() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        snapshot.peer = None;
        snapshot.local.as_mut().unwrap().update = Some(update("waiting_for_peer", true));
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("Waiting for it to come back"), "{text}");
        assert!(!text.contains("out of reach"), "{text}");
    }

    #[test]
    fn settings_shows_the_version_and_any_note() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        let mut status = update("idle", false);
        status.available = None;
        status.note = Some("mesh-kvm hasn't been able to check for updates for 3 days.".into());
        snapshot.local.as_mut().unwrap().update = Some(status);
        let mut text = String::new();
        let _ = ctx.run(RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| window.settings_menu(ui, &snapshot));
        });
        let output = ctx.run(RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| window.settings_menu(ui, &snapshot));
        });
        text.push_str(&drawn_text(&output));
        assert!(text.contains("This is version 0.7.0."), "{text}");
        assert!(text.contains("Check now"), "{text}");
        assert!(text.contains("hasn't been able to check for updates for 3 days"), "{text}");
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

    fn paired_status() -> crate::app::pairing::PairingStatus {
        serde_json::from_str(r#"{"state":"idle","peer_name":"Laptop","fingerprint":"4F2A-91C3"}"#).unwrap()
    }

    /// The Settings menu's contents, drawn on their own.
    fn menu_text(window: &mut Window, snapshot: &Snapshot) -> String {
        let ctx = context();
        let mut text = String::new();
        for _ in 0..2 {
            let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, WINDOW_SIZE.into())), ..Default::default() };
            let output = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| window.settings_menu(ui, snapshot));
            });
            text = drawn_text(&output);
        }
        text
    }

    #[test]
    fn the_settings_menu_shows_the_fingerprint_and_unpair_when_paired() {
        let mut window = window();
        let mut snapshot = secure(linked(), "encrypted");
        snapshot.pairing = Some(paired_status());
        let text = menu_text(&mut window, &snapshot);
        for line in [
            "Paired with windows-host",
            "Everything between these devices is encrypted.",
            "Fingerprint, the same on both: 4F2A-91C3",
            "Unpair",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(!text.contains("\nPair\n"), "{text}");
        assert!(window.pairing_in_menu, "asking for the fingerprint while the menu is open");
    }

    #[test]
    fn the_settings_menu_offers_pair_when_unpaired() {
        let mut window = window();
        let text = menu_text(&mut window, &secure(linked(), "unpaired"));
        assert!(text.contains("\nPair\n"), "{text}");
        assert!(!text.contains("Unpair") && !text.contains("Fingerprint"), "{text}");
        // A service that doesn't encrypt has neither.
        let text = menu_text(&mut window, &linked());
        assert!(!text.contains("Pair") && !text.contains("Unpair"), "{text}");
    }

    #[test]
    fn an_old_peer_shows_the_update_notice_with_the_other_device_offline() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        snapshot.peer = None;
        let local = snapshot.local.as_mut().unwrap();
        local.peer_online = false;
        local.secure = "unpaired".into();
        local.old_peer = true;
        local.link = Some(Default::default());
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("is running an older mesh-kvm that can't encrypt. Update it, then pair."), "{text}");
        assert!(!text.contains("Not paired yet"), "{text}");
    }

    #[test]
    fn the_pair_window_opens_asks_for_the_status_and_closes_when_paired() {
        let (ctx, mut window) = (context(), window());
        let now = Instant::now();
        let mut snapshot = secure(linked(), "unpaired");
        window.open_pairing(&snapshot);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now));
        assert!(text.contains("Show a code here") && text.contains("Enter a code here"), "{text}");
        assert!(window.client.wants_pairing());

        // The service restarts after pairing, so the window may only ever see the result.
        snapshot.pairing = Some(paired_status());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now));
        assert!(text.contains("Paired with Laptop"), "{text}");
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now + Duration::from_secs(3));
        assert!(!window.pairing.open);
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now + Duration::from_secs(4)));
        assert!(!text.contains("Paired with Laptop"), "{text}");
        assert!(!window.client.wants_pairing());
    }

    #[test]
    fn a_pairing_action_never_blocks_the_window_and_keeps_it_working() {
        let (ctx, mut window) = (context(), window());
        let snapshot = secure(linked(), "unpaired");
        window.open_pairing(&snapshot);
        // An action that has not been answered, as a slow one would be.
        let (_answer, waiting) = mpsc::channel();
        window.pairing.pending = Some(waiting);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("Still working on it."), "{text}");
        assert!(window.pairing.pending.is_some());

        // Sent for real, the call runs on its own thread and the window carries on.
        window.pairing.pending = None;
        window.apply_pairing(&ctx, &snapshot, Some(PairingRequest::Show));
        for _ in 0..400 {
            frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
            if window.pairing.pending.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Nothing listens, which a restarting service looks like too, so that is no error.
        assert!(window.pairing.pending.is_none());
        assert!(window.pairing.input.refusal.is_none());
    }

    #[test]
    fn a_refused_code_is_shown_in_the_box_it_came_from() {
        let (ctx, mut window) = (context(), window());
        let snapshot = secure(linked(), "unpaired");
        window.open_pairing(&snapshot);
        window.pairing.input.entering = true;
        window.pairing.input.refusal = Some("Set the other device's address first, then enter the code.".into());
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("Set the other device's address first"), "{text}");
    }
}
