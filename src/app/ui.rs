//! The app window. It draws what the `Client` last polled and sends what the person changes. The
//! decisions live in `model`, `view` and `rows`; the top is drawn in `top`, the settings in
//! `table` and the arrangement in `canvas`.

use super::canvas::Canvas;
use super::client::{Client, PairingReply};
use super::launch::ServiceStarter;
use super::model::{self, Control, ControlsEnabled, Device, Edits, LayoutDraft, ModeChange, Picks, ServiceStart, Snapshot};
use super::pairing::{self, PairingInput, PairingRequest, PairingView};
use super::rows::{self, ActionKind, ChoiceValue, GroupRows, Pick, RowsInput, Setting};
use super::sparkline::Sparklines;
use super::table::{Table, TableRequest};
use super::theme::{
    self, legacy::{AMBER, DEEP, RED}, line, Type, BG, DANGER, LINE, SPACE_2, SPACE_3, SPACE_4, SPACE_5, SURFACE, TEXT,
    TEXT_DIM,
};
use super::top::{self, TopRequest};
use super::updates::{self, Banner, VersionWatch};
use super::view;
use super::widgets::{small, status_square};
use crate::webui::UNKNOWN_ADDRESS;
use eframe::egui::{
    self, vec2, Align, Align2, Frame, Key, Layout, Margin, Modifiers, Rect, RichText, ScrollArea, Sense, Stroke, Ui,
    ViewportCommand,
};
use serde_json::json;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const WINDOW_SIZE: [f32; 2] = [1280.0, 760.0];
const MIN_WINDOW_SIZE: [f32; 2] = [760.0, 560.0];
/// The space around the layout canvas.
const CANVAS_MARGIN: Margin = Margin::same(SPACE_5 as i8);
/// How often to look at how starting the service is going.
const START_CHECK: Duration = Duration::from_millis(250);
const PEER_PROMPT_WIDTH: f32 = 380.0;
const MODE_PROMPT_WIDTH: f32 = 420.0;
const MODE_PROMPT_SECONDS: f32 = crate::displays::REVERT_AFTER.as_secs() as f32;

/// The app size a person can choose, on top of what the window's size does by itself.
pub(super) const UI_SCALES: [f32; 7] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.4, 1.5];
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
    show_licences: bool,
    edits: Edits,
    /// The audio devices just picked, shown ahead of the poll.
    picks: Picks,
    layout: LayoutDraft,
    canvas: Canvas,
    table: Table,
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
    /// The last line about an update under way, for while its service can't be seen.
    update_under_way: Option<Banner>,
    /// The version the device that's behind was last told to look for.
    nudged_for: Option<String>,
    version_watch: VersionWatch,
    /// A Windows copy run from somewhere other than where mesh-kvm is installed.
    not_installed: bool,
    sparklines: Sparklines,
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
            show_licences: false,
            edits: Edits::default(),
            picks: Picks::default(),
            layout: LayoutDraft::default(),
            canvas: Canvas::default(),
            table: Table::default(),
            main_change: None,
            main_error: None,
            main_choice: None,
            main_link_was_down: false,
            ui_scale: DEFAULT_UI_SCALE,
            window_zoom: 1.0,
            smallest_sent: None,
            update_under_way: None,
            nudged_for: None,
            version_watch: VersionWatch::default(),
            not_installed: cfg!(target_os = "windows") && !super::install::runs_from_installed(),
            sparklines: Sparklines::default(),
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
        self.clear_stale_main_error(snapshot, &on);
        let start = self.starter.state();
        self.layout.follow(snapshot.layout.clone(), self.canvas.is_dragging(), now);

        self.sparklines.sample(snapshot, now);
        let line = view::status_line(snapshot, &start, &on);
        let mut requests = Vec::new();
        let bar = Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(SPACE_5 as i8, SPACE_3 as i8));
        egui::TopBottomPanel::top("status bar").frame(bar).show(ctx, |ui| {
            let switch_name = line.switch_to.map(|d| snapshot.title(d));
            requests.extend(top::status_bar(ui, &line, theme::tone(line.square), switch_name.as_deref()));
        });
        let banner = self.follow_update(ctx, snapshot);
        let updating = self.update_under_way.is_some();
        let strip = Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(SPACE_5 as i8, SPACE_2 as i8));
        if let Some(banner) = &banner {
            egui::TopBottomPanel::top("update").frame(strip).show(ctx, |ui| requests.extend(top::update_line(ui, banner)));
        }
        for (index, notice) in view::notices(snapshot, &start, self.client.last_error(), updating).iter().enumerate() {
            let panel = egui::TopBottomPanel::top(egui::Id::new("notice").with(index)).frame(strip);
            panel.show(ctx, |ui| requests.extend(top::notice_line(ui, notice)));
        }
        let rows = view::frame_layout(ctx.screen_rect().width(), ctx.screen_rect().height()).stat_rows;
        egui::TopBottomPanel::top("stats")
            .frame(Frame::new().fill(SURFACE))
            .show(ctx, |ui| top::stats_row(ui, snapshot, &self.sparklines, rows));
        for request in requests {
            self.apply_top(snapshot, request);
        }

        self.body(ctx, snapshot, &on, now);

        let first_run = model::needs_peer(snapshot);
        self.licences_window(ctx);
        if first_run || self.peer_prompt.open {
            self.peer_window(ctx, snapshot, first_run);
        }
        if let Some((device, change)) = snapshot.mode_change() {
            self.mode_prompt(ctx, snapshot, device, change);
        }
        self.pairing_window(ctx, snapshot, now);
        // The table shows the fingerprint whenever the service encrypts.
        let encrypts = snapshot.local.as_ref().is_some_and(|l| !l.secure.is_empty());
        self.client.want_pairing(self.pairing.open || encrypts);

        self.canvas.end_lost_drag(ctx, &mut self.layout, now);
        self.send_changes(now);
        let wake = [self.edits.next_wake(now), self.layout.next_wake(now), self.client.next_row_error_change()];
        let wake = wake.into_iter().flatten().min();
        if let Some(wake) = wake {
            ctx.request_repaint_after(wake);
        }
        if start == ServiceStart::Starting {
            ctx.request_repaint_after(START_CHECK);
        }
    }

    /// What the update line says, and what goes with it. A service that has just updated means
    /// this window is the old version, so it starts again from the new exe.
    fn follow_update(&mut self, ctx: &egui::Context, snapshot: &Snapshot) -> Option<Banner> {
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
        banner
    }

    fn apply_top(&mut self, snapshot: &Snapshot, request: TopRequest) {
        match request {
            TopRequest::Switch => self.client.master_action("toggle_focus"),
            TopRequest::ReleaseAll => self.client.release_all_input(),
            TopRequest::Update(button) => {
                if let Some(here) = snapshot.this_device() {
                    self.client.local_action(here, button.action());
                }
            }
            TopRequest::DismissError => self.client.dismiss_error(),
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
        for key in self.client.take_refused() {
            self.edits.forget_sent(key);
        }
        for ((device, control), value) in self.edits.take_due(now) {
            self.client.send_control(device, control, value);
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

    fn licences_window(&mut self, ctx: &egui::Context) {
        // Wide enough for the licence's own line breaks, as long as the window allows it.
        let width = LICENCES_WIDTH.min(ctx.screen_rect().width() - 2.0 * theme::legacy::MARGIN as f32);
        egui::Window::new("Licences").open(&mut self.show_licences).collapsible(false).default_width(width).show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(ctx.screen_rect().height() * 0.7).show(ui, |ui| {
                ui.label(small(crate::licenses::TEXT));
            });
        });
    }

    /// The settings table and the layout canvas, side by side or one above the other, or why
    /// there are none.
    fn body(&mut self, ctx: &egui::Context, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) {
        if snapshot.local.is_none() {
            self.no_service_body(ctx, snapshot);
            return;
        }
        let screen = ctx.screen_rect();
        let frame = view::frame_layout(screen.width(), screen.height());
        let groups = self.groups(snapshot, on, now);
        let columns = snapshot.in_order().map(|device| (device, snapshot.title(device)));
        let label_width = view::label_width(frame.settings_width);
        let selected = self.canvas.selected.clone();
        let scroll_to_selected = self.canvas.just_selected;
        let settings = Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(SPACE_5 as i8, SPACE_3 as i8));
        let (table, canvas, layout) = (&mut self.table, &mut self.canvas, &mut self.layout);
        let mut draw_table =
            |ui: &mut Ui| table.show(ui, &groups, columns.clone(), label_width, selected.as_ref(), scroll_to_selected, now);
        let mut requests = Vec::new();
        if frame.stacked {
            egui::CentralPanel::default().frame(Frame::new().fill(BG)).show(ctx, |ui| {
                ScrollArea::vertical().auto_shrink(false).drag_to_scroll(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    settings.show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        requests = draw_table(ui);
                    });
                    ui.allocate_ui(vec2(ui.available_width(), view::STACKED_CANVAS_HEIGHT), |ui| {
                        Frame::new().inner_margin(CANVAS_MARGIN).show(ui, |ui| canvas.show(ui, layout, snapshot, now));
                    });
                });
            });
        } else if frame.body_scrolls {
            // Too short for the table to scroll beside a squashed canvas, so both scroll together.
            // The canvas keeps the visible height, so its screens stay in view at the top.
            egui::CentralPanel::default().frame(Frame::new().fill(BG)).show(ctx, |ui| {
                let visible = ui.available_height();
                ScrollArea::vertical().auto_shrink(false).drag_to_scroll(false).show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let size = vec2(frame.settings_width, 0.0);
                        let column = ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                            settings.show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                requests = draw_table(ui);
                            });
                        });
                        let column = column.response.rect;
                        ui.painter().vline(column.right() - 0.5, column.y_range(), line(1.0, LINE));
                        ui.allocate_ui(vec2(ui.available_width(), visible), |ui| {
                            Frame::new().inner_margin(CANVAS_MARGIN).show(ui, |ui| canvas.show(ui, layout, snapshot, now));
                        });
                    });
                });
            });
        } else {
            egui::SidePanel::left("settings")
                .exact_width(frame.settings_width)
                .resizable(false)
                .show_separator_line(true)
                .frame(settings)
                .show(ctx, |ui| {
                    ScrollArea::vertical().auto_shrink(false).drag_to_scroll(false).show(ui, |ui| requests = draw_table(ui));
                });
            egui::CentralPanel::default()
                .frame(Frame::new().fill(BG).inner_margin(CANVAS_MARGIN))
                .show(ctx, |ui| canvas.show(ui, layout, snapshot, now));
        }
        self.apply_table(ctx, snapshot, requests, now);
    }

    /// With no service there is nothing to set, only why.
    fn no_service_body(&self, ctx: &egui::Context, snapshot: &Snapshot) {
        let start = self.starter.state();
        let text = model::start_notice(&start, snapshot).or_else(|| model::status_message(snapshot)).unwrap_or_default();
        let square = if model::notice_is_serious(snapshot, &start) { DANGER } else { TEXT_DIM };
        let frame = Frame::new().fill(BG).inner_margin(Margin::symmetric(SPACE_5 as i8, SPACE_4 as i8));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = SPACE_3;
                ui.vertical(|ui| {
                    // In line with the middle of the first line of text.
                    let line_height = Type::Body.font().size * 1.4;
                    ui.add_space((line_height - theme::STATUS_SQUARE) / 2.0);
                    status_square(ui, square);
                });
                let mut job = Type::Body.job(&text, TEXT);
                job.wrap.max_width = ui.available_width();
                ui.label(job);
            });
        });
    }

    /// Every group of the settings table, from the poll and what the person has just changed.
    fn groups(&self, snapshot: &Snapshot, on: &ControlsEnabled, now: Instant) -> Vec<GroupRows> {
        let client = &self.client;
        let row_error = |key: (Device, Control)| client.row_error(key);
        rows::groups(&RowsInput {
            snapshot,
            layout: self.layout.state.as_ref(),
            edits: &self.edits,
            picks: &self.picks,
            on,
            row_error: &row_error,
            ui_scale: self.ui_scale,
            main_changing: self.main_change.is_some(),
            main_error: self.main_error.as_deref(),
            unpairing: self.pairing.unpairing.is_some(),
            unpair_error: self.pairing.unpair_error.as_deref(),
            now,
        })
    }

    /// Does what the person asked for in the table.
    fn apply_table(&mut self, ctx: &egui::Context, snapshot: &Snapshot, requests: Vec<TableRequest>, now: Instant) {
        for request in requests {
            match request {
                TableRequest::Set(device, control, value) => self.edits.set((device, control), value, now),
                TableRequest::Number(Setting::EdgePush, value) => {
                    if let Some(state) = &mut self.layout.state {
                        state.config.edge_resistance = value.round() as i32;
                    }
                    self.layout.edited(now);
                }
                TableRequest::Number(..) => {}
                TableRequest::Toggle(setting) => self.toggle_game_lock(setting, now),
                TableRequest::Pick(device, pick, value) => self.apply_pick(device, pick, value, now),
                TableRequest::Rename(device, name) => {
                    self.client.local_action(device, json!({ "action": "set_device_name", "name": name }));
                }
                TableRequest::Action(device, action) => self.apply_action(ctx, snapshot, device, action),
                // The canvas was drawn before this came, so it draws again to light the monitor.
                TableRequest::Hover(monitor) => {
                    if self.canvas.hovered_in_controls != monitor {
                        self.canvas.hovered_in_controls = monitor;
                        ctx.request_repaint();
                    }
                }
            }
        }
    }

    /// The game locks are this device's own, set through its layout-wide actions.
    fn toggle_game_lock(&mut self, setting: Setting, now: Instant) {
        let (auto, action) = match setting {
            Setting::AutoGameLock => (true, "toggle_auto_game_lock"),
            Setting::ManualGameLock => (false, "toggle_game_lock"),
            Setting::Device(_) | Setting::EdgePush => return,
        };
        if let Some(state) = &mut self.layout.state {
            let lock = if auto { &mut state.config.auto_game_lock } else { &mut state.config.manual_game_lock };
            *lock = !*lock;
        }
        self.client.master_action(action);
        self.layout.hold(now);
    }

    fn apply_pick(&mut self, device: Device, pick: Pick, value: ChoiceValue, now: Instant) {
        match (pick, value) {
            (Pick::Audio(role), ChoiceValue::Id(id)) => {
                self.client.local_action(device, json!({ "action": "set_audio_device", "role": role.name(), "id": id }));
                self.picks.set((device, role), id, now);
            }
            (Pick::Bitrate, ChoiceValue::Kbps(kbps)) => self.edits.set((device, Control::AudioBitrate), f64::from(kbps), now),
            (Pick::Mode(output), ChoiceValue::Mode(mode)) => self.client.local_action(device, model::mode_action(&output, &mode)),
            (Pick::Placement, ChoiceValue::Placement(place)) => {
                if let Some(state) = &mut self.layout.state {
                    model::apply_placement(&mut state.config, &place);
                }
                self.layout.edited(now);
            }
            (Pick::UiScale, ChoiceValue::Scale(scale)) => self.ui_scale = scale,
            // `rows` pairs each pick with its own kind of value.
            _ => {}
        }
    }

    fn apply_action(&mut self, ctx: &egui::Context, snapshot: &Snapshot, device: Device, action: ActionKind) {
        match action {
            ActionKind::TestSound => self.client.local_action(device, json!({ "action": "test_audio_chime", "target": "peer" })),
            ActionKind::MakeMain if self.main_change.is_none() => self.choose_main(ctx, device),
            ActionKind::MakeMain => {}
            ActionKind::ChangeAddress => self.peer_prompt.open = true,
            ActionKind::Pair => self.open_pairing(snapshot),
            ActionKind::Unpair => self.unpair(ctx),
            ActionKind::ResetSizes => {
                self.client.master_action("reset_native_resolutions");
                self.layout.let_poll_through();
            }
            ActionKind::CheckUpdate => self.client.local_action(device, json!({ "action": "check_update" })),
            ActionKind::ShowLicences => self.show_licences = true,
        }
    }

    /// Unpairs on a background thread, as the service restarts to do it.
    fn unpair(&mut self, ctx: &egui::Context) {
        if self.pairing.unpairing.is_some() {
            return;
        }
        self.pairing.unpair_error = None;
        self.pairing.unpairing = Some(self.send_pairing(ctx, json!({ "action": "unpair" })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::canvas::monitor_id;
    use crate::app::fixtures::*;
    use crate::app::rows::device_details;
    use crate::app::table::cell_id;
    use crate::app::theme::{DEVICE_COLUMN, SETTINGS_WIDTH};
    use eframe::egui::{Event, PointerButton, Pos2, RawInput};
    use std::io::{Read, Write};

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
        for size in [MIN_WINDOW_SIZE, [899.0, 760.0], WINDOW_SIZE, [1280.0, 599.0], [1920.0, 1080.0], [3840.0, 2160.0]] {
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
        // Below 900 wide the arrangement goes under the settings instead.
        for size in [[900.0, 760.0], WINDOW_SIZE] {
            let (ctx, mut window, snapshot) = (context(), window(), linked());
            for _ in 0..4 {
                frame(&ctx, &mut window, &snapshot, size, vec![], Instant::now());
            }
            let screen = ctx.screen_rect();
            for device in Device::BOTH {
                let rect = ctx.read_response(monitor_id(device, 0)).unwrap().rect;
                let settings_width = view::frame_layout(screen.width(), screen.height()).settings_width;
                assert!(rect.left() >= settings_width, "{size:?} {rect:?}");
                assert!(rect.right() <= screen.right() && rect.bottom() <= screen.bottom(), "{size:?} {rect:?}");
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
    fn the_top_shows_the_status_line_and_every_stat_label() {
        let (ctx, mut window, s) = (context(), window(), secure(linked(), "encrypted"));
        frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now()));
        // The fixture's devices report hostnames, which is what the window names them by.
        for line in ["Control", "debian-host", "Switch to windows-host", "Release all", "ROUND TRIP", "LOST", "AUDIO LOST", "AUDIO BUFFER", "LINK", "Secure"] {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[test]
    fn unpaired_shows_dashes_and_no_switch() {
        let (ctx, mut window, s) = (context(), window(), unpaired());
        frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now()));
        for line in ["Not paired.", "Everything stays on each device.", "\nNot paired\n"] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert_eq!(text.lines().filter(|l| *l == "--").count(), 4, "{text}");
        assert!(!text.contains("Switch to"), "{text}");
    }

    #[test]
    fn the_top_fits_every_width_without_cutting_a_figure() {
        let mut old = secure(linked(), "unpaired");
        old.local.as_mut().unwrap().old_peer = true;
        let mut snapshots = states();
        snapshots.extend([unpaired(), old, link_down(secure(linked(), "encrypted"))]);
        for size in [MIN_WINDOW_SIZE, [899.0, 760.0], [900.0, 760.0], WINDOW_SIZE] {
            for snapshot in &snapshots {
                let (ctx, mut window) = (context(), window());
                frame(&ctx, &mut window, snapshot, size, vec![], Instant::now());
                let output = frame(&ctx, &mut window, snapshot, size, vec![], Instant::now());
                let figures: Vec<String> = view::STATS.iter().map(|s| view::stat_value(*s, snapshot).text).collect();
                for clipped in &output.shapes {
                    let egui::Shape::Text(shape) = &clipped.shape else { continue };
                    let text = shape.galley.text();
                    let ours = figures.iter().any(|f| f == text) || view::STATS.iter().any(|s| s.label() == text) || text == "Release all";
                    if ours {
                        assert!(!shape.galley.elided, "{text} cut at {size:?}");
                        assert!(shape.visual_bounding_rect().right() <= size[0], "{text} past the edge at {size:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_long_device_name_is_cut_in_the_status_bar() {
        let (ctx, mut window, mut s) = (context(), window(), secure(linked(), "encrypted"));
        let long = "A desktop with a very long name";
        s.local.as_mut().unwrap().device_name = long.into();
        frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &s, WINDOW_SIZE, vec![], Instant::now()));
        let shown = format!("Control {}", view::fit_name(long).0);
        assert!(text.lines().any(|l| l == shown), "{text}");
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
            assert_eq!(text.contains(top::RELEASE_NOTES), banner.notes.is_some(), "{text}");
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

    /// A window tall enough for every row of the table at once.
    const TALL: [f32; 2] = [1280.0, 1600.0];

    /// Settles a tall window, then shows the tooltip of the cell in `label`'s row and `column`.
    fn hover_text(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, label: &str, column: usize) -> String {
        ctx.style_mut(|s| s.interaction.tooltip_delay = 0.0);
        let now = Instant::now();
        for _ in 0..3 {
            frame(ctx, window, snapshot, TALL, vec![], now);
        }
        let at = ctx.read_response(cell_id(label, column)).unwrap().rect.center();
        let mut text = String::new();
        for _ in 0..4 {
            text = drawn_text(&frame(ctx, window, snapshot, TALL, vec![Event::PointerMoved(at)], now));
        }
        text
    }

    #[test]
    fn a_screen_with_no_working_mode_list_shows_its_mode_and_says_so() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        // The desktop answered nothing, so only the layout's own numbers are left.
        snapshot.local_displays.clear();
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        let text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now()));
        assert!(text.contains("\n2560\u{d7}1440\n") && text.contains("\n360 Hz\n"), "{text}");
        let label = "Screen 2 resolution";
        assert!(hover_text(&ctx, &mut window, &snapshot, label, 0).contains("can't be changed from here right now"));
        // Listed, but only for reading.
        let mut listed = display("DP-3");
        listed.can_change = false;
        snapshot.local_displays = vec![listed];
        let (ctx, mut window) = (context(), self::window());
        assert!(hover_text(&ctx, &mut window, &snapshot, "Screen 1 resolution", 0).contains("can't be changed from here right now"));
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

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> Event {
        Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers }
    }

    /// Clicks Laptop's monitor, which selects it and gives it the keyboard.
    fn click_laptop(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, now: Instant) {
        let at = monitor_on_screen(ctx, window, snapshot, Device::Windows, 0, now).center();
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![Event::PointerMoved(at)], now);
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![pointer(at, true)], now);
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![pointer(at, false)], now);
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![], now);
    }

    #[test]
    fn the_canvas_shows_its_label_and_hint() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        let mut output = frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        for _ in 0..2 {
            output = frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], Instant::now());
        }
        let text = drawn_text(&output);
        assert!(text.contains("LAYOUT"), "{text}");
        assert!(text.contains("Drag a screen to move it. The bright edge is where the mouse crosses."), "{text}");
    }

    #[test]
    fn an_arrow_key_moves_the_selected_screen_by_ten_and_shift_by_one() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (linked(), Instant::now());
        click_laptop(&ctx, &mut window, &snapshot, now);
        let start = box_of(&window, Device::Windows).0;
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![key(egui::Key::ArrowRight, Default::default())], now);
        assert_eq!(box_of(&window, Device::Windows).0, start + 10);
        frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![key(egui::Key::ArrowRight, egui::Modifiers::SHIFT)], now);
        assert_eq!(box_of(&window, Device::Windows).0, start + 11);
        assert!(window.layout.take_due(now + Duration::from_secs(5)).is_some(), "the move should be saved");
    }

    #[test]
    fn the_selected_tile_has_a_text_border_and_the_crossing_edge_is_accent() {
        let (ctx, mut window) = (context(), window());
        let (snapshot, now) = (linked(), Instant::now());
        let screen = monitor_on_screen(&ctx, &mut window, &snapshot, Device::Windows, 0, now);
        let before = frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        let bordered = |output: &egui::FullOutput, colour| {
            output.shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(r) if r.rect == screen && r.stroke.color == colour && r.stroke.width == 1.0))
        };
        assert!(!bordered(&before, theme::TEXT));
        assert!(bordered(&before, theme::LINE_STRONG));
        click_laptop(&ctx, &mut window, &snapshot, now);
        let after = frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now);
        assert!(bordered(&after, theme::TEXT));
        let seam = after.shapes.iter().any(|c| matches!(&c.shape, egui::Shape::LineSegment { stroke, .. } if stroke.color == theme::ACCENT && stroke.width == theme::CROSSING_EDGE));
        assert!(seam, "no accent crossing edge");
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
        // A control's problem is kept for its row, not the window.
        let key = (Device::Debian, Control::Autostart);
        assert!(window.client.row_error(key).is_some());
        assert_eq!(window.edits.take_due(now), vec![]);
        // The error strip draws, and the window keeps working.
        frame(&ctx, &mut window, &snapshot, MIN_WINDOW_SIZE, vec![], now);
    }

    #[test]
    fn device_details_name_this_device() {
        let d = device("debian", None);
        assert_eq!(device_details(&d, true), "This device, debian-host, 192.168.8.2");
        assert_eq!(device_details(&d, false), "debian-host, 192.168.8.2");
        // The Name cells say it on hover.
        let (window, snapshot) = (window(), linked());
        let groups = window.groups(&snapshot, &model::enabled(&snapshot), Instant::now());
        let name = groups.iter().flat_map(|g| &g.rows).find(|r| r.label == "Name").unwrap();
        let tips = name.cells.clone().map(|c| c.tooltip.unwrap());
        assert!(tips[0].ends_with("\nThis device, debian-host, 192.168.8.2"), "{tips:?}");
        assert!(tips[1].ends_with("\nwindows-host, 192.168.8.2"), "{tips:?}");
        let (ctx, mut window) = (context(), self::window());
        let text = hover_text(&ctx, &mut window, &snapshot, "Name", 0);
        assert!(text.contains("This device, debian-host"), "{text}");
    }

    fn paired_status() -> crate::app::pairing::PairingStatus {
        serde_json::from_str(r#"{"state":"idle","peer_name":"Laptop","fingerprint":"4F2A-91C3"}"#).unwrap()
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
        // The table still shows the fingerprint of a service that encrypts, so it keeps asking.
        assert!(window.client.wants_pairing());
        frame(&ctx, &mut window, &linked(), WINDOW_SIZE, vec![], now + Duration::from_secs(4));
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

    /// Every text shape drawn, with where it was drawn.
    fn texts(output: &egui::FullOutput) -> Vec<(String, Rect)> {
        let mut all = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(shape) = &clipped.shape {
                all.push((shape.galley.text().to_string(), shape.visual_bounding_rect()));
            }
        }
        all
    }

    /// Settles the window at `size` and returns the last frame.
    fn settled(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, size: [f32; 2]) -> egui::FullOutput {
        for _ in 0..3 {
            frame(ctx, window, snapshot, size, vec![], Instant::now());
        }
        frame(ctx, window, snapshot, size, vec![], Instant::now())
    }

    /// Clicks the middle of a table cell.
    fn click_cell(ctx: &egui::Context, window: &mut Window, snapshot: &Snapshot, label: &str, column: usize, now: Instant) {
        let at = ctx.read_response(cell_id(label, column)).unwrap().rect.center();
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![Event::PointerMoved(at)], now);
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![pointer(at, true)], now);
        frame(ctx, window, snapshot, WINDOW_SIZE, vec![pointer(at, false)], now);
    }

    #[test]
    fn the_table_header_names_both_devices_and_the_groups_follow() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        let text = drawn_text(&settled(&ctx, &mut window, &snapshot, TALL));
        let mut from = 0;
        for word in ["SETTING", "DEBIAN-HOST", "WINDOWS-HOST", "INPUT", "SCREENS", "CLIPBOARD", "AUDIO", "GAMES", "STARTUP", "BOTH DEVICES", "UPDATES"] {
            let line = format!("\n{word}\n");
            let at = text[from..].find(&line).unwrap_or_else(|| panic!("{word} after {from}\n{text}"));
            from += at + line.len() - 1;
        }
    }

    #[test]
    fn a_group_with_nothing_in_it_has_no_heading() {
        // With no layout there are no screens to list.
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        snapshot.layout = None;
        let text = drawn_text(&settled(&ctx, &mut window, &snapshot, TALL));
        assert!(!text.contains("\nSCREENS\n") && text.contains("\nINPUT\n"), "{text}");
    }

    #[test]
    fn a_short_window_keeps_every_screen_in_view() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        let size = [1280.0, 599.0];
        settled(&ctx, &mut window, &snapshot, size);
        let screen = ctx.screen_rect();
        for (device, index) in [(Device::Debian, 0), (Device::Debian, 1), (Device::Windows, 0)] {
            let rect = ctx.read_response(monitor_id(device, index)).unwrap().rect;
            assert!(screen.contains_rect(rect), "{device:?} {index} {rect:?} outside {screen:?}");
            assert!(rect.left() >= SETTINGS_WIDTH, "{device:?} {index} {rect:?}");
        }
    }

    #[test]
    fn the_settings_column_is_700_wide_and_the_layout_takes_the_rest() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        settled(&ctx, &mut window, &snapshot, WINDOW_SIZE);
        let panel = egui::containers::panel::PanelState::load(&ctx, egui::Id::new("settings")).unwrap();
        assert_eq!(panel.rect.width(), SETTINGS_WIDTH);
        for (device, index) in [(Device::Debian, 0), (Device::Debian, 1), (Device::Windows, 0)] {
            let rect = ctx.read_response(monitor_id(device, index)).unwrap().rect;
            assert!(rect.left() >= SETTINGS_WIDTH, "{device:?} {index} {rect:?}");
        }
    }

    #[test]
    fn a_narrow_window_stacks_the_layout_under_the_settings() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        let size = [899.0, 760.0];
        let output = settled(&ctx, &mut window, &snapshot, size);
        let texts = texts(&output);
        let last_row = texts.iter().find(|(t, _)| t == "Licences").map(|(_, r)| r.bottom()).unwrap();
        for (device, index) in [(Device::Debian, 0), (Device::Debian, 1), (Device::Windows, 0)] {
            let rect = ctx.read_response(monitor_id(device, index)).unwrap().rect;
            assert!(rect.top() > last_row, "{device:?} {index} {rect:?} above {last_row}");
        }
        let top = |label: &str| texts.iter().find(|(t, _)| t == label).map(|(_, r)| r.top()).unwrap();
        let first = top("ROUND TRIP");
        assert!(top("LOST") == first && top("AUDIO LOST") == first, "{texts:?}");
        assert!(top("AUDIO BUFFER") > first && top("LINK") > first, "{texts:?}");
    }

    #[test]
    fn clicking_a_toggle_cell_flips_it_and_sends_once() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        let now = Instant::now();
        settled(&ctx, &mut window, &snapshot, WINDOW_SIZE);
        let key = (Device::Debian, Control::MouseSharing);
        click_cell(&ctx, &mut window, &snapshot, "Share mouse", 0, now);
        assert_eq!(window.edits.shown(key, 1.0, now), 0.0);
        // Sent on the frame of the click, so nothing is left to send.
        assert_eq!(window.edits.take_due(now), vec![]);
    }

    #[test]
    fn the_window_asks_for_no_second_main_change_while_one_is_saving() {
        let (ctx, mut window, snapshot) = (context(), window(), linked());
        let now = Instant::now();
        // A main device change that hasn't been answered yet.
        let (_answer, waiting) = mpsc::channel();
        window.main_change = Some(waiting);
        settled(&ctx, &mut window, &snapshot, TALL);
        let groups = window.groups(&snapshot, &model::enabled(&snapshot), now);
        let main = groups.iter().flat_map(|g| &g.rows).find(|r| r.label == "Main device").unwrap();
        let column = main.cells.iter().position(|c| c.saving).expect("one cell is changing");
        let at = ctx.read_response(cell_id("Main device", column)).unwrap().rect.center();
        for events in [vec![Event::PointerMoved(at)], vec![pointer(at, true)], vec![pointer(at, false)]] {
            frame(&ctx, &mut window, &snapshot, TALL, events, now);
        }
        assert_eq!(window.main_choice, None, "no second change was asked for");
    }

    /// A service on a port of its own, answering this device's status as `local`, with no
    /// other device, and refusing every action.
    fn refusing_service(local: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut data = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let Ok(n) = stream.read(&mut chunk) else { return };
                        if n == 0 {
                            break;
                        }
                        data.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&data);
                        let Some(end) = text.find("\r\n\r\n") else { continue };
                        let length = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0usize);
                        if data.len() >= end + 4 + length {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&data);
                    let path = text.split_whitespace().nth(1).unwrap_or_default();
                    let (status, body) = match path {
                        "/api/local" => (200, local),
                        "/api/local/action" => (200, r#"{"ok":false}"#),
                        _ => (404, "{}"),
                    };
                    let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                });
            }
        });
        port
    }

    #[test]
    fn a_refused_toggle_snaps_back_and_says_why_under_its_row() {
        let local = r#"{"node":"debian","hostname":"debian-host","ip":"192.168.8.2","peer_online":true,"sharing":{"mouse":true,"keyboard":true}}"#;
        let client = Client::start_on(refusing_service(local), Box::new(|| {}));
        let (ctx, mut window) = (context(), Window::new(client, ServiceStarter::default()));
        let mut snapshot = Snapshot::default();
        for _ in 0..400 {
            snapshot = window.client.snapshot();
            if snapshot.local.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let now = Instant::now();
        settled(&ctx, &mut window, &snapshot, WINDOW_SIZE);
        let key = (Device::Debian, Control::MouseSharing);
        click_cell(&ctx, &mut window, &snapshot, "Share mouse", 0, now);
        assert_eq!(window.edits.shown(key, 1.0, now), 0.0, "it flips at once");
        let mut text = String::new();
        for _ in 0..400 {
            text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now));
            if window.client.row_error(key).is_some() && window.edits.shown(key, 1.0, now) == 1.0 {
                text = drawn_text(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now));
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(window.edits.shown(key, 1.0, now), 1.0, "the polled value again");
        assert_eq!(text.matches("didn't accept that change").count(), 1, "{text}");
        let error = texts(&frame(&ctx, &mut window, &snapshot, WINDOW_SIZE, vec![], now));
        let below = error.iter().find(|(t, _)| t.contains("didn't accept that change")).unwrap().1.top();
        let row = error.iter().find(|(t, _)| t == "Share mouse").unwrap().1.bottom();
        let next = error.iter().find(|(t, _)| t == "Share keyboard").unwrap().1.top();
        assert!(row < below && below < next, "under its own row: {row} {below} {next}");
    }

    #[test]
    fn long_names_never_widen_a_device_column() {
        let (ctx, mut window) = (context(), window());
        let mut snapshot = linked();
        let name = "A desktop with a really long nam"; // 32 characters
        assert_eq!(name.chars().count(), 32);
        let speakers = "Speakers (High Definition Audio Device with a long name, rear)";
        let local = snapshot.local.as_mut().unwrap();
        local.device_name = name.into();
        local.audio.outputs[0].name = speakers.into();
        let output = settled(&ctx, &mut window, &snapshot, TALL);
        let texts = texts(&output);
        assert!(texts.iter().any(|(t, _)| t.starts_with("A DESKTOP") && t.ends_with('\u{2026}')), "{texts:?}");
        let columns = SPACE_5 + view::label_width(SETTINGS_WIDTH);
        for (text, rect) in &texts {
            // The device columns, the header's included.
            if rect.left() >= columns - 0.5 && rect.right() <= SETTINGS_WIDTH {
                assert!(rect.width() <= DEVICE_COLUMN, "{text} is {} wide", rect.width());
                assert!(!text.contains(speakers) && !text.contains(name), "{text} was drawn whole");
            }
        }
        assert!(hover_text(&ctx, &mut window, &snapshot, "Name", 0).contains(name));
    }

    #[test]
    fn with_no_service_the_body_says_why() {
        let client = Client::start_on(std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(), Box::new(|| {}));
        let starter = ServiceStarter::begin_for_test(ServiceStart::Starting);
        let mut window = Window::new(client, starter.clone());
        let why = "mesh-kvm couldn't start on this device. See /tmp/x.log";
        starter.finish_for_test(why);
        let ctx = context();
        let output = settled(&ctx, &mut window, &Snapshot::default(), WINDOW_SIZE);
        let stats = egui::containers::panel::PanelState::load(&ctx, egui::Id::new("stats")).unwrap().rect.bottom();
        let texts = texts(&output);
        let said = texts.iter().find(|(t, r)| t == why && r.top() > stats);
        assert!(said.is_some(), "{texts:?}");
        let square = output.shapes.iter().any(|c| match &c.shape {
            egui::Shape::Rect(r) => r.fill == DANGER && r.rect.width() == theme::STATUS_SQUARE && r.rect.top() > stats,
            _ => false,
        });
        assert!(square, "a danger square beside it");
        assert!(!texts.iter().any(|(t, _)| t == "SETTING"), "and nothing else");
    }
}
