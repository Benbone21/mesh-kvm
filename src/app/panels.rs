//! The controls beside the arrangement: one column of sections for each device.

use super::canvas::{Canvas, MonitorKey};
use super::model::{self, AudioDevice, Control, ControlsEnabled, Device, DeviceRole, DeviceStatus, Display, Edits, LayoutDraft, Snapshot};
use super::theme::{self, line, AMBER, DEEP, DIM, EDGE, INK};
use super::widgets::{meter, slider_row, small, toggle_row, wide_button};
use crate::audio::BITRATES;
use crate::platform::{DisplayInfo, DisplayMode};
use crate::settings::MAX_DEVICE_NAME;
use crate::webui::UNKNOWN_ADDRESS;
use eframe::egui::{self, vec2, Align, FontId, RichText, Sense, Ui};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

pub const MIN_POINTER_SPEED: f64 = 0.05;
pub const MAX_POINTER_SPEED: f64 = 2.0;
const VOLUME_RANGE: RangeInclusive<f64> = 0.0..=150.0;
/// How long a new name stays on show before the polled one takes over.
const NAME_HOLD: Duration = Duration::from_secs(3);
const NAME_BOX_ROOM: f32 = 2.0;
/// How long a picked device stays on show before the polled choice takes over.
const PICK_HOLD: Duration = Duration::from_secs(1);

const CANT_CHANGE: &str = "Its resolution and refresh rate can't be changed from here right now.";

/// A device's name while it is being changed.
#[derive(Default)]
struct NameEdit {
    /// What's in the box. `None` shows the polled name.
    text: Option<String>,
    sent_at: Option<Instant>,
}

/// What the controls keep between frames.
#[derive(Default)]
pub struct Panels {
    names: HashMap<Device, NameEdit>,
    picks: HashMap<(Device, DeviceRole), (String, Instant)>,
}

/// What the person asked for in one device's section, for the window to send.
#[derive(Default)]
pub struct Requests {
    pub actions: Vec<(Device, Value)>,
    pub toggle_auto_game_lock: bool,
    pub toggle_manual_game_lock: bool,
}

pub struct Shared<'a> {
    pub edits: &'a mut Edits,
    pub layout: &'a LayoutDraft,
    pub canvas: &'a mut Canvas,
    pub snapshot: &'a Snapshot,
    pub on: &'a ControlsEnabled,
    pub now: Instant,
}

/// The heading of a part of a device's section.
fn subheading(ui: &mut Ui, text: &str) {
    ui.add_space(theme::SECTION_GAP);
    ui.label(RichText::new(text).size(theme::BODY_SIZE + 1.0).color(DIM));
    ui.add_space(theme::HEADING_GAP - ui.spacing().item_spacing.y);
}

impl Panels {
    /// One device's sections, in the width `ui` has.
    pub fn device(&mut self, ui: &mut Ui, device: Device, shared: &mut Shared, requests: &mut Requests) {
        self.heading(ui, device, shared, requests);
        let Some(status) = shared.snapshot.device(device) else {
            ui.add_space(theme::HEADING_GAP);
            let other = !shared.snapshot.is_here(device);
            let text = if other {
                "Its controls show up here once it's connected."
            } else {
                "This device's controls show up here once mesh-kvm is running."
            };
            ui.label(small(text));
            return;
        };
        let mut panel = DevicePanel { device, status, shared, requests, picks: &mut self.picks };
        panel.screens(ui);
        panel.input(ui);
        panel.clipboard(ui);
        panel.audio(ui);
        panel.startup(ui);
    }

    /// The device's name, which can be typed over, and how it's connected.
    fn heading(&mut self, ui: &mut Ui, device: Device, shared: &Shared, requests: &mut Requests) {
        let snapshot = shared.snapshot;
        let status = snapshot.device(device);
        let polled = snapshot.title(device);
        let edit = self.names.entry(device).or_default();
        let settled = edit.sent_at.is_none_or(|at| shared.now.duration_since(at) >= NAME_HOLD);
        let mut shown = match &edit.text {
            Some(text) => text.clone(),
            None => polled.clone(),
        };
        // The swatch ties these controls to the same device's screens on the map.
        let swatch = theme::screen_colour(device == Device::Debian);
        let height = theme::TITLE_SIZE + 4.0;
        let row = ui.horizontal(|ui| {
            let (mark, _) = ui.allocate_exact_size(vec2(6.0, height), Sense::hover());
            ui.painter().rect_filled(mark, 2.0, swatch);
            let width = ui.available_width() - NAME_BOX_ROOM;
            // A long name gets smaller to fit, down to what can still be read.
            let full = ui.painter().layout_no_wrap(shown.clone(), FontId::proportional(theme::TITLE_SIZE), INK).size().x;
            let size = (theme::TITLE_SIZE * width / full.max(1.0)).clamp(theme::SMALL_SIZE, theme::TITLE_SIZE);
            let field = egui::TextEdit::singleline(&mut shown)
                .id(egui::Id::new(("device name", device)))
                .font(FontId::proportional(size))
                .text_color(INK)
                .frame(false)
                .desired_width(width)
                .char_limit(MAX_DEVICE_NAME)
                .margin(egui::Margin::ZERO);
            ui.add_enabled(status.is_some(), field)
        });
        let field = row.inner;
        let mark = if field.has_focus() { AMBER } else if field.hovered() { INK } else { EDGE };
        let rule = field.rect.bottom() + 1.0;
        ui.painter().hline(field.rect.x_range(), rule, line(1.0, mark));
        let field = field.on_hover_text("Type here to rename this device. Leave it empty to use its network name.");
        if field.changed() {
            edit.text = Some(shown.clone());
        }
        if field.lost_focus() {
            if let Some(text) = edit.text.clone() {
                if text.trim() != polled {
                    requests.actions.push((device, json!({ "action": "set_device_name", "name": text.trim() })));
                    edit.sent_at = Some(shared.now);
                } else {
                    edit.text = None;
                }
            }
        }
        if !field.has_focus() && settled && edit.sent_at.is_some() {
            *edit = NameEdit::default();
        }
        ui.add_space(2.0);
        ui.label(small(device_details(status, snapshot.is_here(device))));
    }
}

/// The controls of one reachable device.
struct DevicePanel<'a, 'b> {
    device: Device,
    status: &'a DeviceStatus,
    shared: &'a mut Shared<'b>,
    requests: &'a mut Requests,
    picks: &'a mut HashMap<(Device, DeviceRole), (String, Instant)>,
}

impl DevicePanel<'_, '_> {
    fn value(&self, control: Control) -> f64 {
        self.shared.edits.shown((self.device, control), control.read(self.status), self.shared.now)
    }

    fn set(&mut self, control: Control, value: f64) {
        self.shared.edits.set((self.device, control), value, self.shared.now);
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

    fn other_name(&self) -> String {
        self.shared.snapshot.name(self.device.other())
    }

    fn screens(&mut self, ui: &mut Ui) {
        let rects = self.shared.layout.state.as_ref().map(|s| s.config.rects(self.device)).unwrap_or_default();
        if rects.is_empty() {
            return;
        }
        subheading(ui, if rects.len() == 1 { "Screen" } else { "Screens" });
        let snapshot = self.shared.snapshot;
        let mut hovered = None;
        for rect in &rects {
            let display = model::display_for(snapshot.displays(self.device), rect);
            if self.screen_row(ui, rect, display) {
                hovered = Some((self.device, rect.name.clone()));
            }
        }
        // Each device's section speaks only for its own monitors.
        if self.shared.canvas.hovered_in_controls.as_ref().is_some_and(|k| k.0 == self.device) || hovered.is_some() {
            self.shared.canvas.hovered_in_controls = hovered;
        }
    }

    /// One monitor's name and its menus. True while the pointer is on it.
    fn screen_row(&mut self, ui: &mut Ui, rect: &Display, display: Option<&DisplayInfo>) -> bool {
        let key: MonitorKey = (self.device, rect.name.clone());
        let selected = self.shared.canvas.selected.as_ref() == Some(&key);
        let (name, size, rate) = model::monitor_label(rect, display);
        let group = ui.scope(|ui| {
            let text = RichText::new(name).color(if selected { AMBER } else { INK });
            ui.add(egui::Label::new(text).truncate());
            match display {
                Some(display) if display.can_change && !display.modes.is_empty() => self.mode_menus(ui, display),
                _ => {
                    // The desktop isn't answering about modes, so nothing is offered that
                    // couldn't be undone. What's known of the screen still shows.
                    let rate = rate.map_or_else(String::new, |rate| format!(" at {rate}"));
                    ui.label(small(format!("{size}{rate}")));
                    ui.label(small(CANT_CHANGE));
                }
            }
        });
        ui.add_space(4.0);
        if selected && self.shared.canvas.just_selected {
            group.response.scroll_to_me(Some(Align::Center));
        }
        ui.rect_contains_pointer(group.response.rect)
    }

    fn mode_menus(&mut self, ui: &mut Ui, display: &DisplayInfo) {
        let Some(current) = &display.current else { return };
        let size = (current.width, current.height);
        let changing = self.shared.snapshot.mode_change().is_some();
        let mut chosen: Option<&DisplayMode> = None;
        ui.add_enabled_ui(!changing, |ui| {
            ui.columns(2, |columns| {
                let width = columns[0].available_width();
                egui::ComboBox::from_id_salt((self.device, &display.id, "resolution"))
                    .selected_text(model::size_label(size))
                    .width(width)
                    .show_ui(&mut columns[0], |ui| {
                        for option in model::resolutions(display) {
                            if ui.selectable_label(option == size, model::size_label(option)).clicked() && option != size {
                                chosen = model::mode_for_size(display, option);
                            }
                        }
                    });
                let rates = model::rates(display, size);
                egui::ComboBox::from_id_salt((self.device, &display.id, "rate"))
                    .selected_text(model::rate_label(current, &rates))
                    .width(width)
                    .show_ui(&mut columns[1], |ui| {
                        for mode in &rates {
                            let selected = mode.same_as(current);
                            if ui.selectable_label(selected, model::rate_label(mode, &rates)).clicked() && !selected {
                                chosen = Some(*mode);
                            }
                        }
                    });
            });
        });
        if let Some(mode) = chosen {
            self.requests.actions.push((self.device, model::mode_action(&display.id, mode)));
        }
    }

    fn input(&mut self, ui: &mut Ui) {
        subheading(ui, "Input");
        let other = self.other_name();
        // An older service has no actions for these, so they stay off rather than failing.
        ui.add_enabled_ui(!self.status.is_older(), |ui| {
            self.toggle(ui, Control::MouseSharing, "Share mouse");
            self.toggle(ui, Control::KeyboardSharing, "Share keyboard");
            let label = format!("Pointer speed on {other}");
            let range = MIN_POINTER_SPEED..=MAX_POINTER_SPEED;
            self.slider(ui, Control::PointerSpeed, &label, |v| format!("{v:.2}\u{d7}"), range, 0.05);
        });
        if self.shared.snapshot.is_here(self.device) {
            self.game_locks(ui);
        }
    }

    /// Only for the device the app is on: the locks are its own, and don't travel.
    fn game_locks(&mut self, ui: &mut Ui) {
        let Some(state) = &self.shared.layout.state else { return };
        let (auto, manual) = (state.config.auto_game_lock, state.config.manual_game_lock);
        self.requests.toggle_auto_game_lock = toggle_row(ui, "Keep the mouse on the screen while a game runs", auto).clicked();
        self.requests.toggle_manual_game_lock = toggle_row(ui, "Lock the mouse to this screen now", manual).clicked();
        let telemetry = self.shared.snapshot.layout.as_ref().map(|l| &l.telemetry);
        if let Some(hint) = telemetry.and_then(model::game_lock_hint) {
            ui.label(small(hint));
        }
    }

    fn clipboard(&mut self, ui: &mut Ui) {
        subheading(ui, "Clipboard");
        let other = self.other_name();
        ui.add_enabled_ui(!self.status.is_older(), |ui| {
            self.toggle(ui, Control::ClipboardSend, &format!("Send clipboard to {other}"));
            self.toggle(ui, Control::ClipboardReceive, &format!("Receive clipboard from {other}"));
            if !self.status.clipboard.note.is_empty() {
                ui.label(small(&self.status.clipboard.note));
            }
        });
    }

    fn audio(&mut self, ui: &mut Ui) {
        subheading(ui, "Audio");
        ui.add_enabled_ui(self.status.audio.play_only_on_other.is_some(), |ui| {
            self.audio_sent(ui);
            ui.add_space(theme::HEADING_GAP);
            self.audio_received(ui);
        });
        for note in &self.status.audio.notes {
            ui.label(small(note));
        }
        if self.status.audio.rx_other_version {
            ui.label(small("Audio from the other device is being ignored. Update both devices to the same version."));
        }
        let label = format!("Play a test sound on {}", self.other_name());
        let can_test = self.shared.on.test_sound;
        if ui.add_enabled_ui(can_test, |ui| wide_button(ui, egui::Button::new(label)).clicked()).inner {
            self.requests.actions.push((self.device, json!({ "action": "test_audio_chime", "target": "peer" })));
        }
    }

    fn audio_sent(&mut self, ui: &mut Ui) {
        let audio = &self.status.audio;
        let (inputs, outputs) = (audio.inputs.clone(), audio.outputs.clone());
        let (input, output) = (audio.input.clone(), audio.output.clone());
        self.device_picker(ui, DeviceRole::Input, "Microphone or line-in to send", &inputs, &input);
        self.device_picker(ui, DeviceRole::Output, "Output to send", &outputs, &output);
        meter(ui, self.status.audio.tx_peak);
        self.toggle(ui, Control::PlayOnlyOnOther, "Play only on the other device");
        self.bitrate_picker(ui);
        self.slider(ui, Control::SendVolume, "Send volume", |v| format!("{v:.0}%"), VOLUME_RANGE, 5.0);
        if self.status.audio.tx_enabled {
            ui.label(small(&model::sending_quality(&self.status.audio, &self.other_name())));
        }
    }

    fn audio_received(&mut self, ui: &mut Ui) {
        let other = self.other_name();
        self.toggle(ui, Control::AudioPlay, &format!("Play audio from {other}"));
        let (outputs, play) = (self.status.audio.outputs.clone(), self.status.audio.play.clone());
        self.device_picker(ui, DeviceRole::Play, "Play received audio on", &outputs, &play);
        meter(ui, self.status.audio.rx_peak);
        self.slider(ui, Control::PlayVolume, "Playback volume", |v| format!("{v:.0}%"), VOLUME_RANGE, 5.0);
        if let Some(quality) = model::receiving_quality(&self.status.audio) {
            ui.label(small(&quality));
        }
    }

    /// A dropdown of real devices. A new pick shows at once, ahead of the next status.
    fn device_picker(&mut self, ui: &mut Ui, role: DeviceRole, label: &str, devices: &[AudioDevice], polled: &str) {
        let key = (self.device, role);
        let held = self.picks.get(&key).filter(|(_, at)| self.shared.now.duration_since(*at) < PICK_HOLD);
        let current = held.map_or(polled.to_string(), |(id, _)| id.clone());
        ui.label(label);
        let mut chosen = None;
        egui::ComboBox::from_id_salt((self.device, label))
            .selected_text(model::device_label(&current, devices))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                let options = role.fixed_options().iter().map(|(id, name)| (*id, *name));
                let options = options.chain(devices.iter().map(|d| (d.id.as_str(), d.name.as_str())));
                for (id, name) in options {
                    if ui.selectable_label(id == current, name).clicked() {
                        chosen = Some(id.to_string());
                    }
                }
            });
        if let Some(id) = chosen {
            self.requests.actions.push((self.device, json!({ "action": "set_audio_device", "role": role.name(), "id": id })));
            self.picks.insert(key, (id, self.shared.now));
        }
    }

    fn bitrate_picker(&mut self, ui: &mut Ui) {
        let current = self.value(Control::AudioBitrate) as u32;
        ui.label("Audio quality");
        let mut chosen = None;
        egui::ComboBox::from_id_salt((self.device, "audio quality"))
            .selected_text(model::bitrate_label(current))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for kbps in BITRATES {
                    if ui.selectable_label(kbps == current, model::bitrate_label(kbps)).clicked() {
                        chosen = Some(kbps);
                    }
                }
            });
        if let Some(kbps) = chosen {
            self.set(Control::AudioBitrate, f64::from(kbps));
        }
    }

    fn startup(&mut self, ui: &mut Ui) {
        subheading(ui, "Startup");
        self.toggle(ui, Control::Autostart, "Start mesh-kvm when this device boots");
    }
}

pub fn device_details(status: Option<&DeviceStatus>, here: bool) -> String {
    let Some(d) = status else { return "Not connected".into() };
    let address = if d.ip == UNKNOWN_ADDRESS { String::new() } else { format!(", {}", d.ip) };
    let place = if here { "This device, " } else { "" };
    format!("{place}{}{address}", d.hostname)
}

/// A plain line between the two devices when they are stacked.
pub fn device_divider(ui: &mut Ui) {
    ui.add_space(theme::SECTION_GAP);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 2.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, DEEP);
    ui.add_space(theme::SECTION_GAP);
}
