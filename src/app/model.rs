//! What the app knows about the two devices, and the layout maths. No egui types in here.

use crate::platform::{DisplayInfo, DisplayMode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a control keeps showing what the person set before the polled value takes over.
const HOLD: Duration = Duration::from_millis(1200);
/// The shortest gap between sends of one control, so a dragged slider doesn't flood the service.
const SEND_GAP: Duration = Duration::from_millis(120);
/// The shortest gap between layout saves. Each save makes the service re-read the monitors.
const SAVE_GAP: Duration = Duration::from_millis(150);

/// The window size all the app's sizes are designed around, which needs no enlarging.
const DESIGN_WINDOW: (f32, f32) = (1280.0, 800.0);
/// How much of a window's extra size goes to enlarging the app. The rest goes to the arrangement.
const WINDOW_GROWTH: f32 = 0.5;
const MOST_WINDOW_ZOOM: f32 = 1.75;
const ZOOM_STEP: f32 = 0.05;
/// The share of a monitor the smallest window may take, leaving room for its frame and a panel.
const MOST_OF_MONITOR: f32 = 0.9;
/// Below this width the two device columns would be too narrow.
const TWO_COLUMNS_FROM: f32 = 1360.0;
/// How far the snap keeps a device on the edge it's dragged along: enough of it to cross at.
const MIN_TOUCH_ALONG_X: i32 = 280;
const MIN_TOUCH_ALONG_Y: i32 = 240;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    Debian,
    Windows,
}

impl Device {
    pub const BOTH: [Device; 2] = [Device::Debian, Device::Windows];

    /// The `node` value in `/api/local`.
    pub fn node(self) -> &'static str {
        match self {
            Device::Debian => "debian",
            Device::Windows => "windows",
        }
    }

    fn from_node(node: &str) -> Option<Device> {
        Device::BOTH.into_iter().find(|d| d.node() == node)
    }

    /// The `device` value of `set_main_device`, and of `main_device` in `/api/local`.
    pub fn id(self) -> u8 {
        match self {
            Device::Debian => 0,
            Device::Windows => 1,
        }
    }

    fn from_id(id: u8) -> Option<Device> {
        Device::BOTH.into_iter().find(|d| d.id() == id)
    }

    pub fn other(self) -> Device {
        match self {
            Device::Debian => Device::Windows,
            Device::Windows => Device::Debian,
        }
    }

    /// How `active_node` and the layout's device list name this device. Never shown: a device is
    /// shown by the name it reports.
    pub fn layout_node(self) -> &'static str {
        match self {
            Device::Debian => "Debian",
            Device::Windows => "Windows",
        }
    }

    /// The device this copy of the app was built for, for when no service has said.
    pub fn of_this_build() -> Device {
        Device::from_node(crate::platform::ROLE).unwrap_or(Device::Debian)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Display {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sharing {
    pub mouse: bool,
    pub keyboard: bool,
}

impl Default for Sharing {
    fn default() -> Self {
        Self { mouse: true, keyboard: true }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClipboardSharing {
    pub send: bool,
    pub receive: bool,
    /// What the clipboard last did on that device.
    pub note: String,
    /// That note says something went wrong.
    pub note_failed: bool,
}

impl Default for ClipboardSharing {
    fn default() -> Self {
        Self { send: true, receive: true, note: String::new(), note_failed: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioStatus {
    /// Something is chosen to send.
    pub tx_enabled: bool,
    /// What's chosen: a device id, `default`, or empty for nothing.
    pub input: String,
    pub output: String,
    pub play: String,
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
    /// Anything the person should know about these choices right now.
    pub notes: Vec<String>,
    /// What this device plays stays out of what it sends. Older services have no way to do that.
    pub echo_safe: bool,
    /// In kbit/s, with zero meaning uncompressed.
    pub bitrate: u32,
    pub tx_volume: u32,
    pub tx_peak: u32,
    /// The other device says real sound from here is coming out of its output.
    pub tx_reaching: bool,
    /// The other device has an output open for it, sound or no sound.
    pub tx_connected: bool,
    /// Percent of what's sent that the other device says isn't arriving.
    pub tx_loss: u32,
    /// Missing from an older service.
    pub play_only_on_other: Option<bool>,
    pub rx_muted: bool,
    pub rx_volume: u32,
    pub rx_peak: u32,
    /// A stream from the other device is alive.
    pub rx_live: bool,
    /// Percent of what's sent here that isn't arriving in time, and what couldn't be rebuilt.
    pub rx_loss: f32,
    pub rx_unrepaired: f32,
    pub rx_buffer_ms: u32,
    /// Audio is arriving from a mesh-kvm of another version, and being ignored.
    pub rx_other_version: bool,
}

/// A microphone, line-in or output, by the id it's saved under and the name it's shown by.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

/// What a device dropdown chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceRole {
    Input,
    Output,
    Play,
}

const SYSTEM_DEFAULT: (&str, &str) = ("default", "System default");
const NOTHING: (&str, &str) = ("", "None");

impl DeviceRole {
    /// The name the service knows it by.
    pub fn name(self) -> &'static str {
        match self {
            DeviceRole::Input => "input",
            DeviceRole::Output => "output",
            DeviceRole::Play => "play",
        }
    }

    /// What's on offer ahead of the real devices. Received audio always plays somewhere.
    pub fn fixed_options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            DeviceRole::Input | DeviceRole::Output => &[NOTHING, SYSTEM_DEFAULT],
            DeviceRole::Play => &[SYSTEM_DEFAULT],
        }
    }
}

/// How a chosen device shows in its dropdown.
pub fn device_label(id: &str, devices: &[AudioDevice]) -> String {
    if id == NOTHING.0 {
        return NOTHING.1.into();
    }
    if id == SYSTEM_DEFAULT.0 {
        return SYSTEM_DEFAULT.1.into();
    }
    devices.iter().find(|device| device.id == id).map_or("Not connected, using the default".into(), |device| device.name.clone())
}

pub fn bitrate_label(kbps: u32) -> String {
    match kbps {
        0 => "Uncompressed, about 1.6 Mbit/s".into(),
        kbps => format!("{kbps} kbit/s"),
    }
}

/// How what's being sent is getting on.
pub fn sending_quality(audio: &AudioStatus, other: &str) -> String {
    if audio.tx_reaching {
        format!("Playing on {other}, {}% lost on the way.", audio.tx_loss)
    } else if audio.tx_connected {
        format!("Connected to {other}, with no sound to send right now.")
    } else {
        format!("Not reaching {other} yet.")
    }
}

/// How what's arriving is getting on, while anything is.
pub fn receiving_quality(audio: &AudioStatus) -> Option<String> {
    audio.rx_live.then(|| {
        format!(
            "Received: {:.1}% lost, {:.1}% couldn't be repaired, {} ms held back to smooth it out.",
            audio.rx_loss, audio.rx_unrepaired, audio.rx_buffer_ms
        )
    })
}

/// The line about the link in a device's section, and whether it should stand out. Never a figure
/// from before the link went down.
pub fn link_line(status: &DeviceStatus, other: &str) -> Option<(String, bool)> {
    let link = status.link.as_ref()?;
    if !status.peer_online {
        let text = if link.returning {
            format!("Link is down. Control goes back to {other} when it returns.")
        } else {
            "Link is down, so each device is using its own keyboard and mouse.".to_string()
        };
        return Some((text, true));
    }
    if link.one_way {
        return Some((format!("Link is one way: {other} can't hear this device."), true));
    }
    if link.returning {
        return Some((format!("Link is back. Control is on its way to {other}."), false));
    }
    if !link.measured {
        return Some(("Link is up, and being measured.".to_string(), false));
    }
    let loss = link.loss();
    let poor = loss.is_some_and(|loss| loss >= POOR_LOSS_PERCENT) || link.round_trip_ms.is_some_and(|ms| ms >= POOR_ROUND_TRIP_MS);
    let text = match (loss, link.round_trip_ms) {
        (Some(loss), Some(ms)) => format!("Link: {loss:.1}% lost, {ms} ms round trip."),
        (Some(loss), None) => format!("Link: {loss:.1}% lost."),
        (None, Some(ms)) => format!("Link: {ms} ms round trip."),
        (None, None) => "Link is up, and being measured.".to_string(),
    };
    Some((text, poor))
}

/// What's behind the figure in the link line.
pub fn link_detail(link: &LinkStatus) -> String {
    let figure = |loss: Option<f32>| loss.map_or("not known".to_string(), |loss| format!("{loss:.1}%"));
    let line = |what: &str, here, there| format!("{what}: {} lost on the way here, {} on the way there.", figure(here), figure(there));
    let mut detail = line("Input", link.loss_in, link.loss_out);
    if link.audio_loss_in.is_some() || link.audio_loss_out.is_some() {
        detail.push('\n');
        detail.push_str(&line("Audio", link.audio_loss_in, link.audio_loss_out));
    }
    detail
}

/// One device's `/api/local`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DeviceStatus {
    /// "debian" or "windows".
    pub node: String,
    pub label: String,
    pub hostname: String,
    pub ip: String,
    pub peer_ip: String,
    pub version: String,
    pub os: String,
    pub peer_online: bool,
    /// "Debian" or "Windows".
    pub active_node: String,
    pub panic_mode: bool,
    pub game_lock: bool,
    pub game_reason: String,
    pub displays: Vec<Display>,
    pub cursor: Option<[i32; 2]>,
    pub autostart: bool,
    pub audio: AudioStatus,
    /// Missing from a service older than 0.2.0, which is how the app tells it apart.
    pub sharing: Option<Sharing>,
    /// False until the service knows the other device's address. Missing from a service that
    /// doesn't save the address, and so keeps the layout only on the Debian desktop.
    pub paired: Option<bool>,
    /// `--peer` set the address, so the app can't change it.
    pub peer_pinned: bool,
    pub clipboard: ClipboardSharing,
    #[serde(default = "default_speed")]
    pub pointer_speed: f32,
    /// 0 is Debian, 1 is the laptop.
    pub main_device: u8,
    pub main_conflict: bool,
    /// What the person calls this device: its given name, or else its hostname.
    pub device_name: String,
    pub display_change: DisplayChanges,
    /// Missing from a service that doesn't measure the link.
    pub link: Option<LinkStatus>,
}

/// `link` in `/api/local`: how the link looks from one device.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LinkStatus {
    /// False until there is a figure to show, and whenever the link is down.
    pub measured: bool,
    /// Missing until there's something to go on.
    pub loss_in: Option<f32>,
    pub loss_out: Option<f32>,
    pub audio_loss_in: Option<f32>,
    pub audio_loss_out: Option<f32>,
    pub round_trip_ms: Option<u32>,
    /// The other device can be heard, and can't hear this one.
    pub one_way: bool,
    /// A dropout took control from the other device, and it's waiting to go back.
    pub returning: bool,
    /// "same", "older", "other", or "unknown" while it can't tell.
    pub peer_version: String,
}

/// From this much on, the link line stands out.
const POOR_LOSS_PERCENT: f32 = 5.0;
const POOR_ROUND_TRIP_MS: u32 = 150;

impl LinkStatus {
    /// The worst loss either way, from the link's own packets and from audio, which sends far more
    /// of them and also counts what came too late to play.
    fn loss(&self) -> Option<f32> {
        [self.loss_in, self.loss_out, self.audio_loss_in, self.audio_loss_out].into_iter().flatten().reduce(f32::max)
    }
}

/// A monitor's mode change that is waiting for an answer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModeChange {
    /// Which change an answer is for.
    pub id: u64,
    pub output: String,
    pub name: String,
    pub from: Option<DisplayMode>,
    pub to: Option<DisplayMode>,
    /// "applying", "waiting" or "going_back".
    pub stage: String,
    pub ms_left: u64,
}

impl ModeChange {
    pub fn is_waiting(&self) -> bool {
        self.stage == "waiting"
    }
}

/// `display_change` in `/api/local`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DisplayChanges {
    pub change: Option<ModeChange>,
    /// How the last change ended, if there is anything to say.
    pub note: Option<String>,
}

impl DeviceStatus {
    /// True for a service from before this version of the app, which can't take the newer controls.
    pub fn is_older(&self) -> bool {
        self.sharing.is_none()
    }

    /// True for a service that can't share a layout with this app.
    fn has_no_shared_layout(&self) -> bool {
        self.paired.is_none()
    }
}

/// What to say about a device whose service is older than the app.
pub fn older_message(title: &str) -> String {
    format!("{title} is running an older mesh-kvm. Update it, or sign out and back in.")
}

fn default_speed() -> f32 {
    1.0
}

/// One device in the layout: where its desktop sits, and the monitors on it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LayoutDevice {
    /// "Debian" or "Windows".
    pub node: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// In the device's own desktop coordinates.
    pub monitors: Vec<Display>,
    pub custom_size: Option<(i32, i32)>,
}

impl LayoutDevice {
    /// The device's screens in the layout.
    pub fn rects(&self) -> Vec<Display> {
        if let Some((w, h)) = self.custom_size {
            return vec![Display { name: self.name.clone(), x: self.x, y: self.y, w, h }];
        }
        let shifted = |m: &Display| Display { x: m.x + self.x, y: m.y + self.y, ..m.clone() };
        self.monitors.iter().map(shifted).collect()
    }
}

/// `(left, top, right, bottom)` around all of `rects`, with right and bottom exclusive.
fn bounds(rects: &[Display]) -> (i32, i32, i32, i32) {
    let left = rects.iter().map(|m| m.x).min().unwrap_or(0);
    let top = rects.iter().map(|m| m.y).min().unwrap_or(0);
    let right = rects.iter().map(|m| m.x + m.w).max().unwrap_or(left);
    let bottom = rects.iter().map(|m| m.y + m.h).max().unwrap_or(top);
    (left, top, right, bottom)
}

/// The layout, as far as the app edits or draws it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    pub devices: Vec<LayoutDevice>,
    /// "Right", "Left", "Bottom" or "Top".
    pub attach_side: String,
    pub edge_resistance: i32,
    #[serde(default = "yes")]
    pub auto_game_lock: bool,
    pub manual_game_lock: bool,
}

impl LayoutConfig {
    pub fn device(&self, device: Device) -> Option<&LayoutDevice> {
        self.devices.iter().find(|d| d.node == device.layout_node())
    }

    fn device_mut(&mut self, device: Device) -> Option<&mut LayoutDevice> {
        self.devices.iter_mut().find(|d| d.node == device.layout_node())
    }

    /// A device's screens in the layout.
    pub fn rects(&self, device: Device) -> Vec<Display> {
        self.device(device).map(LayoutDevice::rects).unwrap_or_default()
    }

    /// The box around a device's screens: `(x, y, w, h)`.
    pub fn device_box(&self, device: Device) -> (i32, i32, i32, i32) {
        let (left, top, right, bottom) = bounds(&self.rects(device));
        (left, top, right - left, bottom - top)
    }

    /// The box around the second device, which placements are worked out for.
    pub fn laptop_box(&self) -> (i32, i32, i32, i32) {
        self.device_box(Device::Windows)
    }

    /// True if a size stands in for a device's real monitors.
    pub fn has_custom_size(&self) -> bool {
        self.devices.iter().any(|d| d.custom_size.is_some())
    }

    /// Moves the laptop so that its box starts at `(x, y)`.
    fn move_laptop(&mut self, x: i32, y: i32) {
        let (left, top, ..) = self.laptop_box();
        if let Some(laptop) = self.device_mut(Device::Windows) {
            laptop.x += x - left;
            laptop.y += y - top;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Telemetry {
    pub active_node: String,
    pub panic_mode: bool,
    pub peer_addr: String,
    pub game_captive_active: bool,
    pub game_reason: String,
    pub peer_online: bool,
    pub main_conflict: bool,
}

/// `/api/state`: this device's copy of the layout, and its live telemetry.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutState {
    pub config: LayoutConfig,
    pub telemetry: Telemetry,
}

impl LayoutState {
    /// `None` for a layout from a service that keeps it in the older form.
    pub fn parse(text: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct State {
            config: Value,
            #[serde(default)]
            telemetry: Telemetry,
        }
        let state: State = serde_json::from_str(text).ok()?;
        state.config.get("devices")?;
        let config = serde_json::from_value(state.config).ok()?;
        Some(Self { config, telemetry: state.telemetry })
    }
}

/// The body for `POST /api/layout`: what differs between `was` and `now`, and nothing else, so
/// that saving can't undo a newer edit from the other device. `None` if nothing does.
pub fn layout_edit(was: &LayoutConfig, now: &LayoutConfig) -> Option<Value> {
    let place = |d: &LayoutDevice| (d.node.clone(), d.x, d.y, d.custom_size);
    let arrangement = |c: &LayoutConfig| (c.devices.iter().map(place).collect::<Vec<_>>(), c.attach_side.clone());
    let mut edit = serde_json::Map::new();
    if arrangement(was) != arrangement(now) {
        let devices: Vec<Value> = now
            .devices
            .iter()
            .map(|d| json!({ "node": d.node, "x": d.x, "y": d.y, "custom_size": d.custom_size }))
            .collect();
        edit.insert("arrangement".into(), json!({ "devices": devices, "attach_side": now.attach_side }));
    }
    if was.edge_resistance != now.edge_resistance {
        edit.insert("edge_resistance".into(), now.edge_resistance.into());
    }
    (!edit.is_empty()).then_some(Value::Object(edit))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub local: Option<DeviceStatus>,
    pub peer: Option<DeviceStatus>,
    pub layout: Option<LayoutState>,
    /// What this device's monitors can do, from `/api/displays`.
    pub local_displays: Vec<DisplayInfo>,
    pub peer_displays: Vec<DisplayInfo>,
}

impl Snapshot {
    /// What `device` last reported, if it can be reached.
    pub fn device(&self, device: Device) -> Option<&DeviceStatus> {
        [&self.local, &self.peer].into_iter().flatten().find(|d| d.node == device.node())
    }

    pub fn this_device(&self) -> Option<Device> {
        Device::from_node(&self.local.as_ref()?.node)
    }

    /// This device first, then the other one.
    pub fn in_order(&self) -> [Device; 2] {
        let here = self.this_device().unwrap_or_else(Device::of_this_build);
        [here, here.other()]
    }

    pub fn is_here(&self, device: Device) -> bool {
        self.in_order()[0] == device
    }

    /// What `device` calls itself, from its own status or else what the layout last heard.
    fn known_name(&self, device: Device) -> Option<String> {
        let status = self.device(device);
        let reported = status.map(|d| d.device_name.as_str()).filter(|n| !n.is_empty());
        let host = status.map(|d| d.hostname.as_str()).filter(|n| !n.is_empty());
        let layout = self.layout.as_ref().and_then(|l| l.config.device(device)).map(|d| d.name.as_str());
        reported.or(host).or(layout.filter(|n| !n.is_empty())).map(str::to_string)
    }

    /// `device`'s name, for the middle of a sentence.
    pub fn name(&self, device: Device) -> String {
        self.known_name(device).unwrap_or_else(|| {
            if self.is_here(device) { "this device" } else { "the other device" }.to_string()
        })
    }

    /// `device`'s name, for headings and the start of a sentence.
    pub fn title(&self, device: Device) -> String {
        self.known_name(device).unwrap_or_else(|| {
            if self.is_here(device) { "This device" } else { "The other device" }.to_string()
        })
    }

    /// What `device`'s monitors can do, as far as is known.
    pub fn displays(&self, device: Device) -> &[DisplayInfo] {
        if self.this_device() == Some(device) {
            &self.local_displays
        } else {
            &self.peer_displays
        }
    }

    /// The mode change that wants an answer, on either device.
    pub fn mode_change(&self) -> Option<(Device, &ModeChange)> {
        Device::BOTH.into_iter().find_map(|d| Some((d, self.device(d)?.display_change.change.as_ref()?)))
    }

    /// True when both devices answer and the link between them is up.
    pub fn linked(&self) -> bool {
        let online = match (&self.layout, &self.local) {
            (Some(layout), _) => layout.telemetry.peer_online,
            (None, Some(local)) => local.peer_online,
            (None, None) => false,
        };
        online && Device::BOTH.iter().all(|d| self.device(*d).is_some())
    }

    /// The device that has the keyboard and mouse. The layout's telemetry is the better source.
    pub fn active(&self) -> Option<Device> {
        let node = match (&self.layout, &self.local) {
            (Some(layout), _) => &layout.telemetry.active_node,
            (None, Some(local)) => &local.active_node,
            (None, None) => return None,
        };
        Device::BOTH.into_iter().find(|d| d.layout_node() == node)
    }

    /// True when either reachable device runs a service older than the app.
    fn any_older(&self) -> bool {
        [&self.local, &self.peer].into_iter().flatten().any(DeviceStatus::is_older)
    }

    /// True while the router on either device has fallen back to local input over two mains.
    fn main_conflict(&self) -> bool {
        [&self.local, &self.peer].into_iter().flatten().any(|d| d.main_conflict)
            || self.layout.as_ref().is_some_and(|l| l.telemetry.main_conflict)
    }

    /// The main device, or `None` while the devices disagree or can't say.
    pub fn main_device(&self) -> Option<Device> {
        let local = self.local.as_ref()?;
        match &self.peer {
            Some(peer) if peer.main_device != local.main_device => None,
            _ => Device::from_id(local.main_device),
        }
    }
}

/// Which controls can be used right now. Device panels depend only on `Snapshot::device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlsEnabled {
    pub switch: bool,
    pub main_device: bool,
    pub layout: bool,
    pub release: bool,
    pub test_sound: bool,
}

pub fn enabled(snapshot: &Snapshot) -> ControlsEnabled {
    let linked = snapshot.linked();
    ControlsEnabled {
        // The router ignores a manual switch during a main conflict.
        switch: linked && !snapshot.main_conflict(),
        main_device: linked && !snapshot.any_older(),
        layout: snapshot.layout.is_some(),
        release: snapshot.local.is_some(),
        test_sound: linked,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Light {
    Off,
    Connected,
    Active,
}

/// The status light and wording for a device in the top bar.
pub fn device_badge(snapshot: &Snapshot, device: Device) -> (Light, &'static str) {
    if snapshot.device(device).is_none() {
        return (Light::Off, "Not connected");
    }
    let has_control = snapshot.active() == Some(device)
        && (snapshot.linked() || snapshot.this_device() == Some(device));
    if has_control {
        (Light::Active, "Has keyboard and mouse")
    } else if snapshot.linked() || snapshot.this_device() == Some(device) {
        (Light::Connected, "Connected")
    } else {
        // Its service may still answer the app, but no input can reach it.
        (Light::Off, "Link down")
    }
}

/// True when this device's service is waiting to be told the other device's address.
pub fn needs_peer(snapshot: &Snapshot) -> bool {
    snapshot.local.as_ref().is_some_and(|d| d.paired == Some(false))
}

/// How starting this device's service went, for an app that found none running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ServiceStart {
    /// Nothing to report: it was running already, or it has started.
    #[default]
    Done,
    Starting,
    Failed(String),
}

/// What to say about the service while it is starting or after that failed. A service that
/// answers settles it, however it came to be running.
pub fn start_notice(start: &ServiceStart, snapshot: &Snapshot) -> Option<String> {
    if snapshot.local.is_some() {
        return None;
    }
    match start {
        ServiceStart::Done => None,
        ServiceStart::Starting => Some("Starting mesh-kvm on this device.".into()),
        ServiceStart::Failed(reason) => Some(reason.clone()),
    }
}

/// True when the notice is about this device's service rather than the link.
pub fn notice_is_serious(snapshot: &Snapshot, start: &ServiceStart) -> bool {
    snapshot.local.is_none() && *start != ServiceStart::Starting
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Through this device's layout-wide action, which tells the other device too.
    Everywhere,
    /// This device's service is too old to have that action, so only this device lets go.
    Only(Device),
}

/// Where Release all input has to go.
pub fn release_route(snapshot: &Snapshot) -> Option<Release> {
    if snapshot.layout.is_some() {
        return Some(Release::Everywhere);
    }
    snapshot.this_device().map(Release::Only)
}

/// Where the cursor is in layout coordinates. Each device reports its own position.
pub fn cursor_position(snapshot: &Snapshot, cfg: &LayoutConfig) -> Option<(i32, i32)> {
    let active = snapshot.active()?;
    let [x, y] = snapshot.device(active)?.cursor?;
    let device = cfg.device(active)?;
    Some((device.x + x, device.y + y))
}

pub fn game_lock_hint(telemetry: &Telemetry) -> Option<String> {
    telemetry.game_captive_active.then(|| match telemetry.game_reason.as_str() {
        "" => "The mouse is being held on this screen.".to_string(),
        reason => format!("The mouse is being held on this screen: {reason}."),
    })
}

/// A setting on one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Control {
    MouseSharing,
    KeyboardSharing,
    PointerSpeed,
    PlayOnlyOnOther,
    AudioBitrate,
    SendVolume,
    AudioPlay,
    PlayVolume,
    ClipboardSend,
    ClipboardReceive,
    Autostart,
}

fn flag(on: bool) -> f64 {
    f64::from(u8::from(on))
}

impl Control {
    /// The control's value in a device's status. Switches are 0 or 1.
    pub fn read(self, d: &DeviceStatus) -> f64 {
        match self {
            Control::MouseSharing => flag(d.sharing.as_ref().is_none_or(|s| s.mouse)),
            Control::KeyboardSharing => flag(d.sharing.as_ref().is_none_or(|s| s.keyboard)),
            Control::PointerSpeed => d.pointer_speed as f64,
            Control::PlayOnlyOnOther => flag(d.audio.play_only_on_other.unwrap_or(false)),
            Control::AudioBitrate => d.audio.bitrate as f64,
            Control::SendVolume => d.audio.tx_volume as f64,
            Control::AudioPlay => flag(!d.audio.rx_muted),
            Control::PlayVolume => d.audio.rx_volume as f64,
            Control::ClipboardSend => flag(d.clipboard.send),
            Control::ClipboardReceive => flag(d.clipboard.receive),
            Control::Autostart => flag(d.autostart),
        }
    }

    /// The service action that sets the control to `value`.
    pub fn action(self, value: f64) -> Value {
        let on = value != 0.0;
        match self {
            Control::MouseSharing => json!({ "action": "set_sharing", "mouse": on }),
            Control::KeyboardSharing => json!({ "action": "set_sharing", "keyboard": on }),
            Control::PointerSpeed => {
                json!({ "action": "set_pointer_speed", "speed": (value * 100.0).round() / 100.0 })
            }
            Control::PlayOnlyOnOther => json!({ "action": "set_play_only_on_other", "enabled": on }),
            Control::AudioBitrate => json!({ "action": "set_audio_bitrate", "kbps": value as u32 }),
            Control::SendVolume => json!({ "action": "set_audio_tx_volume", "volume": value as u32 }),
            Control::AudioPlay => json!({ "action": "set_audio_rx_muted", "muted": !on }),
            Control::PlayVolume => json!({ "action": "set_audio_rx_volume", "volume": value as u32 }),
            Control::ClipboardSend => json!({ "action": "set_clipboard", "send": on }),
            Control::ClipboardReceive => json!({ "action": "set_clipboard", "receive": on }),
            Control::Autostart => json!({ "action": "set_autostart", "enabled": on }),
        }
    }
}

struct Edit {
    value: f64,
    held_until: Instant,
    unsent: bool,
    sent_at: Option<Instant>,
}

impl Edit {
    fn next_send(&self) -> Option<Instant> {
        self.sent_at.map(|at| at + SEND_GAP)
    }
}

/// What the person just set on each device. A control shows this rather than the poll for a
/// moment, so it doesn't flick back before the service has caught up.
#[derive(Default)]
pub struct Edits {
    entries: HashMap<(Device, Control), Edit>,
}

impl Edits {
    pub fn set(&mut self, key: (Device, Control), value: f64, now: Instant) {
        let sent_at = self.entries.get(&key).and_then(|e| e.sent_at);
        self.entries.insert(key, Edit { value, held_until: now + HOLD, unsent: true, sent_at });
    }

    pub fn shown(&self, key: (Device, Control), polled: f64, now: Instant) -> f64 {
        match self.entries.get(&key) {
            Some(e) if e.unsent || now < e.held_until => e.value,
            _ => polled,
        }
    }

    /// The edits ready to send. A value edited again within the gap waits, and only its latest
    /// value goes.
    pub fn take_due(&mut self, now: Instant) -> Vec<((Device, Control), f64)> {
        self.entries.retain(|_, e| e.unsent || now < e.held_until);
        let mut due = Vec::new();
        for (key, e) in &mut self.entries {
            if e.unsent && e.next_send().is_none_or(|at| at <= now) {
                e.unsent = false;
                e.sent_at = Some(now);
                due.push((*key, e.value));
            }
        }
        due
    }

    /// How long until something here needs another look, so the window can stay idle until then.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        self.entries
            .values()
            .map(|e| if e.unsent { e.next_send().unwrap_or(now) } else { e.held_until })
            .min()
            .map(|at| at.saturating_duration_since(now))
    }
}

/// The layout the window shows and edits. It follows the poll, except during a drag and for a
/// moment after an edit.
#[derive(Debug, Default)]
pub struct LayoutDraft {
    pub state: Option<LayoutState>,
    /// The layout as the service last knew it, which edits are measured from.
    base: Option<LayoutConfig>,
    held_until: Option<Instant>,
    unsaved: bool,
    saved_at: Option<Instant>,
}

impl LayoutDraft {
    pub fn follow(&mut self, polled: Option<LayoutState>, dragging: bool, now: Instant) {
        let held = dragging || self.unsaved || self.held_until.is_some_and(|t| now < t);
        if !held || self.state.is_none() {
            self.base = polled.as_ref().map(|p| p.config.clone());
        }
        self.state = merge_poll(self.state.take(), polled, held);
    }

    /// Keeps the draft as it is for a moment, for a change the service makes from an action.
    pub fn hold(&mut self, now: Instant) {
        self.held_until = Some(now + HOLD);
    }

    /// Drops any edit waiting to save and lets the next poll through, for a change that only the
    /// service can work out.
    pub fn let_poll_through(&mut self) {
        self.unsaved = false;
        self.held_until = None;
    }

    /// Records that the draft has changed and needs saving.
    pub fn edited(&mut self, now: Instant) {
        self.unsaved = true;
        self.hold(now);
    }

    /// The edit to save now, if one is waiting and the last save was long enough ago. It holds
    /// only what changed, so an edit that reached this copy while the draft was held stays.
    pub fn take_due(&mut self, now: Instant) -> Option<Value> {
        if !self.unsaved || self.saved_at.is_some_and(|at| now < at + SAVE_GAP) {
            return None;
        }
        self.unsaved = false;
        self.saved_at = Some(now);
        self.hold(now);
        let config = &self.state.as_ref()?.config;
        let edit = layout_edit(self.base.as_ref()?, config);
        self.base = Some(config.clone());
        edit
    }

    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        let at = if self.unsaved {
            self.saved_at.map_or(now, |at| at + SAVE_GAP)
        } else {
            self.held_until.filter(|t| now < *t)?
        };
        Some(at.saturating_duration_since(now))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub side: String,
}

/// Screen to map: `screen = world * scale + offset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapView {
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

impl MapView {
    pub fn to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale + self.offset_x, y * self.scale + self.offset_y)
    }

    pub fn to_world(&self, x: f32, y: f32) -> (f32, f32) {
        ((x - self.offset_x) / self.scale, (y - self.offset_y) / self.scale)
    }
}

/// JavaScript's `Math.round`, which the page's layout maths relied on.
fn js_round(v: f64) -> i32 {
    (v + 0.5).floor() as i32
}

fn overlaps_any(rects: &[Display], (x, y, w, h): (i32, i32, i32, i32)) -> bool {
    rects.iter().any(|m| x < m.x + m.w && m.x < x + w && y < m.y + m.h && m.y < y + h)
}

/// The named places the second device can go, one set per monitor of the first.
pub fn placements(cfg: &LayoutConfig, displays: &[DisplayInfo]) -> Vec<Placement> {
    let (.., w, h) = cfg.laptop_box();
    let desktop = cfg.rects(Device::Debian);
    let (left, _, right, _) = bounds(&desktop);
    let mut out = Vec::new();
    for m in &desktop {
        let label = monitor_label(m, display_for(displays, m)).0;
        let cx = js_round(m.x as f64 + (m.w - w) as f64 / 2.0);
        let cy = js_round(m.y as f64 + (m.h - h) as f64 / 2.0);
        let mut add = |prefix: &str, x: i32, y: i32, side: &str| {
            if !overlaps_any(&desktop, (x, y, w, h)) {
                out.push(Placement { label: format!("{prefix} {label}"), x, y, side: side.into() });
            }
        };
        add("Below", cx, m.y + m.h, "Bottom");
        add("Above", cx, m.y - h, "Top");
        if m.x <= left {
            add("Left of", m.x - w, cy, "Left");
        }
        if m.x + m.w >= right {
            add("Right of", m.x + m.w, cy, "Right");
        }
    }
    out
}

/// Pulls `value` onto the nearest of `lines`, if one is within `magnet`.
fn pull_to(value: i32, lines: [i32; 3], magnet: i32) -> i32 {
    let nearest = lines.into_iter().min_by_key(|line| (line - value).abs()).unwrap_or(value);
    if (nearest - value).abs() <= magnet {
        nearest
    } else {
        value
    }
}

/// The closest edge placement to a dragged laptop position.
///
/// The device always lands against an edge, with enough of it alongside to cross at, and never
/// on top of a monitor. Within `magnet` of lining up with a monitor's start, middle or end along
/// that edge, it lines up. A device moves only as a whole, so its own monitors keep the places
/// their operating system gave them.
pub fn snap(cfg: &LayoutConfig, x: i32, y: i32, magnet: i32) -> Option<Placement> {
    let (.., w, h) = cfg.laptop_box();
    let desktop = cfg.rects(Device::Debian);
    let (left, _, right, _) = bounds(&desktop);
    let mut best: Option<(f64, Placement)> = None;
    let mut consider = |px: i32, py: i32, side: &str| {
        let d = ((x - px) as f64).hypot((y - py) as f64);
        if !overlaps_any(&desktop, (px, py, w, h)) && best.as_ref().is_none_or(|(b, _)| d < *b) {
            best = Some((d, Placement { label: "Custom".into(), x: px, y: py, side: side.into() }));
        }
    };
    // `max` then `min`, as the page did, so an inverted range doesn't panic.
    let clamp = |v: i32, lo: i32, hi: i32| v.min(hi).max(lo);
    for m in &desktop {
        let (low_y, high_y) = (m.y - h + MIN_TOUCH_ALONG_Y, m.y + m.h - MIN_TOUCH_ALONG_Y);
        let (low_x, high_x) = (m.x - w + MIN_TOUCH_ALONG_X, m.x + m.w - MIN_TOUCH_ALONG_X);
        let cy = pull_to(clamp(y, low_y, high_y), [m.y, m.y + (m.h - h) / 2, m.y + m.h - h], magnet);
        let cx = pull_to(clamp(x, low_x, high_x), [m.x, m.x + (m.w - w) / 2, m.x + m.w - w], magnet);
        let mut along_edge = |px: i32, py: i32, side: &str| {
            consider(px, py, side);
            // Blocked here by a monitor alongside, the nearest places clear of it are next.
            for o in &desktop {
                match side {
                    "Bottom" | "Top" => {
                        consider(clamp(o.x - w, low_x, high_x), py, side);
                        consider(clamp(o.x + o.w, low_x, high_x), py, side);
                    }
                    _ => {
                        consider(px, clamp(o.y - h, low_y, high_y), side);
                        consider(px, clamp(o.y + o.h, low_y, high_y), side);
                    }
                }
            }
        };
        if m.x + m.w >= right {
            along_edge(m.x + m.w, cy, "Right");
        }
        if m.x <= left {
            along_edge(m.x - w, cy, "Left");
        }
        along_edge(cx, m.y + m.h, "Bottom");
        along_edge(cx, m.y - h, "Top");
    }
    best.map(|(_, p)| p)
}

/// Moves `device` as a whole towards `(x, y)`, the corner of its box, and snaps it. What matters
/// is where the devices sit against each other, so the first device moving is worked out as the
/// second moving the other way, and the second stays where it is on the map.
pub fn drag_device(cfg: &mut LayoutConfig, device: Device, x: i32, y: i32, magnet: i32) {
    if device == Device::Windows {
        if let Some(place) = snap(cfg, x, y, magnet) {
            apply_placement(cfg, &place);
        }
        return;
    }
    let (from_x, from_y, ..) = cfg.device_box(Device::Debian);
    let (laptop_x, laptop_y, ..) = cfg.laptop_box();
    let Some(place) = snap(cfg, laptop_x - (x - from_x), laptop_y - (y - from_y), magnet) else { return };
    if let Some(first) = cfg.device_mut(Device::Debian) {
        first.x += laptop_x - place.x;
        first.y += laptop_y - place.y;
    }
    cfg.attach_side = place.side;
}

/// A stretch where the devices meet, as two points in layout coordinates.
pub type Seam = ((i32, i32), (i32, i32));

/// Where the mouse crosses from one device to the other: the closest monitors along the side the
/// second device is attached to, as the service works it out.
pub fn seams(cfg: &LayoutConfig) -> Vec<Seam> {
    let (first, second) = (cfg.rects(Device::Debian), cfg.rects(Device::Windows));
    let side = cfg.attach_side.as_str();
    let mut found: Vec<(i32, Seam)> = Vec::new();
    for m in &first {
        for o in &second {
            let (gap, seam) = match side {
                "Right" | "Left" => {
                    let (top, bottom) = (m.y.max(o.y), (m.y + m.h).min(o.y + o.h));
                    let (mine, theirs) = if side == "Right" { (m.x + m.w, o.x) } else { (m.x, o.x + o.w) };
                    if top >= bottom {
                        continue;
                    }
                    ((mine - theirs).abs(), ((mine, top), (mine, bottom)))
                }
                "Bottom" | "Top" => {
                    let (start, end) = (m.x.max(o.x), (m.x + m.w).min(o.x + o.w));
                    let (mine, theirs) = if side == "Bottom" { (m.y + m.h, o.y) } else { (m.y, o.y + o.h) };
                    if start >= end {
                        continue;
                    }
                    ((mine - theirs).abs(), ((start, mine), (end, mine)))
                }
                _ => continue,
            };
            found.push((gap, seam));
        }
    }
    let nearest = found.iter().map(|f| f.0).min().unwrap_or(0);
    found.into_iter().filter(|f| f.0 == nearest).map(|f| f.1).collect()
}

/// Where a laptop grabbed at `grab` (its offset from the laptop's corner, in layout units) is
/// being asked to go, for a pointer at `pointer` in the map.
pub fn drag_target(view: &MapView, pointer: (f32, f32), grab: (f32, f32)) -> (i32, i32) {
    let (x, y) = view.to_world(pointer.0, pointer.1);
    (js_round((x - grab.0) as f64), js_round((y - grab.1) as f64))
}

pub fn apply_placement(cfg: &mut LayoutConfig, placement: &Placement) {
    cfg.move_laptop(placement.x, placement.y);
    cfg.attach_side = placement.side.clone();
}

pub fn size_label((w, h): (i32, i32)) -> String {
    format!("{w} \u{d7} {h}")
}

/// The name of where the laptop is now: a named place, or the edge it was dragged along.
pub fn position_label(cfg: &LayoutConfig, placements: &[Placement]) -> String {
    let (x, y, ..) = cfg.laptop_box();
    match placements.iter().find(|p| p.x == x && p.y == y) {
        Some(p) => p.label.clone(),
        None => format!("Custom ({} edge)", cfg.attach_side.to_lowercase()),
    }
}

/// Scale and offset that fit every screen, with a margin, into a map of this size.
pub fn fit(cfg: &LayoutConfig, width: f32, height: f32) -> MapView {
    const PAD: f32 = 16.0;
    let all: Vec<Display> = cfg.devices.iter().flat_map(LayoutDevice::rects).collect();
    let (x0, y0, x1, y1) = bounds(&all);
    let (span_w, span_h) = (((x1 - x0) as f32).max(1.0), ((y1 - y0) as f32).max(1.0));
    let (w, h) = ((width - PAD * 2.0).max(1.0), (height - PAD * 2.0).max(1.0));
    let scale = (w / span_w).min(h / span_h);
    MapView {
        scale,
        offset_x: PAD + (w - span_w * scale) / 2.0 - x0 as f32 * scale,
        offset_y: PAD + (h - span_h * scale) / 2.0 - y0 as f32 * scale,
    }
}

/// True if a monitor in the layout, called `rect_name` there, is the one the system calls `id`.
/// The layout's name starts with that, and may go on to say more, as in `DP-2 (360Hz Native)`.
pub fn same_monitor(rect_name: &str, id: &str) -> bool {
    rect_name.strip_prefix(id).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// The monitor behind a screen in the layout, if `displays` has it. `own` is where the layout puts
/// it on its device's own desktop.
pub fn display_for<'a>(displays: &'a [DisplayInfo], rect: &Display) -> Option<&'a DisplayInfo> {
    displays.iter().find(|d| same_monitor(&rect.name, &d.id))
}

/// How to label a monitor: its name, its size and its rate, the last if known.
pub fn monitor_label(rect: &Display, display: Option<&DisplayInfo>) -> (String, String, Option<String>) {
    let name = display.map_or_else(|| rect.name.split(" (").next().unwrap_or("").to_string(), |d| d.name.clone());
    let mode = display.and_then(|d| d.current.as_ref());
    let size = mode.map_or((rect.w, rect.h), |m| (m.width, m.height));
    let rate = mode.filter(|m| m.millihertz > 0).map(|m| format!("{} Hz", m.whole_hertz()));
    (name, size_label(size), rate.or_else(|| rate_in_layout_name(&rect.name)))
}

/// The rate in a layout name like `DP-2 (360Hz Native)`, which is known even when the desktop
/// isn't answering about modes.
fn rate_in_layout_name(name: &str) -> Option<String> {
    let inside = name.split_once(" (")?.1;
    let hertz = inside.split_once("Hz")?.0;
    hertz.parse::<u32>().ok().map(|hz| format!("{hz} Hz"))
}

/// The smallest the window may be, in points at `zoom`. It grows with the person's app size, but
/// never past what the monitor can show, or the window couldn't fit on it.
pub fn smallest_window(base: (f32, f32), ui_scale: f32, zoom: f32, monitor: Option<(f32, f32)>) -> (f32, f32) {
    let (width, height) = (base.0 * ui_scale / zoom, base.1 * ui_scale / zoom);
    match monitor {
        Some((w, h)) => (width.min(w * MOST_OF_MONITOR), height.min(h * MOST_OF_MONITOR)),
        None => (width, height),
    }
}

/// The sizes a monitor offers, largest first.
pub fn resolutions(display: &DisplayInfo) -> Vec<(i32, i32)> {
    let mut sizes: Vec<(i32, i32)> = Vec::new();
    for mode in &display.modes {
        if !sizes.contains(&(mode.width, mode.height)) {
            sizes.push((mode.width, mode.height));
        }
    }
    sizes
}

/// The modes a monitor offers at one size, fastest first.
pub fn rates(display: &DisplayInfo, (width, height): (i32, i32)) -> Vec<&DisplayMode> {
    display.modes.iter().filter(|m| (m.width, m.height) == (width, height)).collect()
}

/// A rate as a menu shows it: whole hertz, unless two at this size would then read the same.
pub fn rate_label(mode: &DisplayMode, among: &[&DisplayMode]) -> String {
    let ambiguous = among.iter().filter(|m| m.whole_hertz() == mode.whole_hertz()).count() > 1;
    if ambiguous {
        format!("{:.2} Hz", mode.millihertz as f64 / 1000.0)
    } else {
        format!("{} Hz", mode.whole_hertz())
    }
}

/// The mode to use when the person picks a size: the rate in use if it has one, else its fastest.
pub fn mode_for_size<'a>(display: &'a DisplayInfo, size: (i32, i32)) -> Option<&'a DisplayMode> {
    let at_size = rates(display, size);
    let in_use = display.current.as_ref().map(DisplayMode::whole_hertz);
    at_size.iter().find(|m| Some(m.whole_hertz()) == in_use).or(at_size.first()).copied()
}

/// The action that answers a mode change, naming it so a late answer can't reach the next one.
pub fn mode_answer(change: &ModeChange, keep: bool) -> Value {
    let action = if keep { "keep_display_mode" } else { "undo_display_mode" };
    json!({ "action": action, "change_id": change.id })
}

/// The action that asks a device to switch a monitor to `mode`.
pub fn mode_action(output: &str, mode: &DisplayMode) -> Value {
    json!({
        "action": "set_display_mode",
        "output": output,
        "width": mode.width,
        "height": mode.height,
        "millihertz": mode.millihertz,
    })
}

/// How much bigger everything gets in a big window, in steps so that a resize doesn't blur the
/// text at every pixel. `(width, height)` is the window at a zoom of one. A small window never
/// shrinks: it reflows instead.
pub fn window_zoom(width: f32, height: f32) -> f32 {
    let room = (width / DESIGN_WINDOW.0).min(height / DESIGN_WINDOW.1).max(1.0);
    let zoom = (1.0 + (room - 1.0) * WINDOW_GROWTH).min(MOST_WINDOW_ZOOM);
    (zoom / ZOOM_STEP).round() * ZOOM_STEP
}

/// How the window's body divides up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyLayout {
    /// The width of the controls beside the arrangement.
    pub rail: f32,
    /// How many device columns the controls have: 2 puts the devices side by side.
    pub columns: usize,
}

/// The arrangement gets what the controls leave, and always the full height. `width` is in points.
pub fn body_layout(width: f32) -> BodyLayout {
    if width >= TWO_COLUMNS_FROM {
        BodyLayout { rail: (width * 0.44).clamp(620.0, 920.0), columns: 2 }
    } else {
        BodyLayout { rail: (width * 0.36).clamp(300.0, 420.0), columns: 1 }
    }
}

/// Takes the polled layout, except that while the laptop is being dragged the layout the person
/// is editing stays as it is.
pub fn merge_poll(
    current: Option<LayoutState>,
    polled: Option<LayoutState>,
    dragging: bool,
) -> Option<LayoutState> {
    if dragging && current.is_some() {
        current
    } else {
        polled
    }
}

/// What to tell the person when something is wrong, or `None` when all is well.
pub fn status_message(snapshot: &Snapshot) -> Option<String> {
    let Some(local) = &snapshot.local else {
        return Some(
            "mesh-kvm isn't running on this device. Start mesh-kvm, and this window reconnects by itself."
                .into(),
        );
    };
    if needs_peer(snapshot) {
        return Some("mesh-kvm needs the other device's address before it can share anything.".into());
    }
    let other = snapshot.title(snapshot.in_order()[1]);
    let link = local.link.as_ref();
    if link.is_some_and(|l| l.returning) {
        return Some(format!(
            "The link to {other} dropped, so each device is using its own keyboard and mouse for now. Control goes back to {other} once the link is back."
        ));
    }
    if snapshot.peer.is_none() {
        return Some(format!(
            "{other} is out of reach, so each device is working on its own. Check that mesh-kvm is running on it, and that its firewall allows TCP port 24880."
        ));
    }
    let older = |d: &DeviceStatus| d.is_older() || d.has_no_shared_layout();
    if let Some(device) = Device::BOTH.into_iter().find(|d| snapshot.device(*d).is_some_and(older)) {
        return Some(older_message(&snapshot.title(device)));
    }
    if !local.peer_online {
        return Some(format!(
            "{other} is found but not linked yet, so each device is working on its own. Wait a few seconds, or restart mesh-kvm on both devices."
        ));
    }
    if local.peer_online && link.is_some_and(|l| l.one_way) {
        return Some(format!(
            "{other} can't hear this device, though this device hears it. A firewall on {other} is the usual reason. Each device stays on its own keyboard and mouse until that's fixed."
        ));
    }
    match link.map(|l| l.peer_version.as_str()) {
        Some("older") => {
            return Some(format!(
                "{other} is running an older mesh-kvm, so a lost key press isn't corrected and control won't come back after a dropout. Update both devices together."
            ));
        }
        Some("other") => {
            return Some("The two devices are running different versions of mesh-kvm and can't read all of each other's packets. Update both devices together.".into());
        }
        _ => {}
    }
    if let Some(message) = snapshot.peer.as_ref().and_then(|peer| split_message(local, peer)) {
        return Some(message);
    }
    let panic = snapshot.layout.as_ref().is_some_and(|l| l.telemetry.panic_mode)
        || local.panic_mode
        || snapshot.peer.as_ref().is_some_and(|p| p.panic_mode);
    if panic {
        return Some(
            "All input is released, and each device is using its own keyboard and mouse. Use the switch to reconnect."
                .into(),
        );
    }
    snapshot.peer.as_ref().and_then(|peer| echo_message(local, peer))
}

/// Both devices sending output sound only echoes if one can't keep what it plays out of what it
/// sends. A microphone can't be helped from here: that's a room, not a loop in the software.
fn echo_message(local: &DeviceStatus, peer: &DeviceStatus) -> Option<String> {
    let sends_output = |d: &DeviceStatus| !d.audio.output.is_empty();
    (sends_output(local) && sends_output(peer) && !(local.audio.echo_safe && peer.audio.echo_safe)).then(|| {
        "Both devices are sending output sound, and one can't leave out what it plays, so you may hear an echo. Choose None under Output to send on one of them."
            .into()
    })
}

/// Explains two devices that disagree about the main device. Only Main device ends this: the
/// switch and the hotkeys can't.
fn split_message(local: &DeviceStatus, peer: &DeviceStatus) -> Option<String> {
    let claims_main = |d: &DeviceStatus| Device::from_node(&d.node).is_some_and(|n| n.id() == d.main_device);
    if local.main_conflict || peer.main_conflict || (claims_main(local) && claims_main(peer)) {
        return Some(
            "Both devices think they are the main device, so each one is using its own keyboard and mouse. Choose Main device to pick one."
                .into(),
        );
    }
    (local.main_device != peer.main_device).then(|| {
        "Each device thinks the other one is the main device, so sharing can't start. Choose Main device to pick one."
            .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_JSON: &str = r#"{"node":"debian","label":"Debian Desktop","hostname":"bread","ip":"192.168.1.10","peer_ip":"192.168.1.20","version":"0.1.0","os":"Debian 13","peer_online":true,"active_node":"Debian","panic_mode":false,"game_lock":false,"game_reason":"","displays":[{"name":"DP-3","x":0,"y":360,"w":1920,"h":1080}],"cursor":[3200,720],"autostart":true,"input_devices":["Physical mouse (evdev)"],"audio":{"tx_enabled":false,"tx_volume":100,"tx_peak":0,"tx_packets":0,"tx_source":0,"rx_muted":false,"rx_volume":100,"rx_peak":0,"rx_packets":0},"sharing":{"mouse":true,"keyboard":false},"paired":true,"clipboard":{"send":true,"receive":false},"pointer_speed":0.35,"main_device":1,"main_conflict":false}"#;

    const STATE_JSON: &str = r#"{"config":{"revision":3,"editor":"Debian","devices":[{"node":"Debian","name":"desk","x":0,"y":0,"monitors":[{"name":"DP-3 (240Hz Native)","x":0,"y":360,"w":1920,"h":1080},{"name":"DP-2 (360Hz Native)","x":1920,"y":0,"w":2560,"h":1440}],"custom_size":null},{"node":"Windows","name":"laptop","x":2240,"y":1440,"monitors":[{"name":"Display 1","x":0,"y":0,"w":2560,"h":1600}],"custom_size":null,"future_device_field":5}],"attach_side":"Bottom","edge_resistance":55,"auto_game_lock":true,"manual_game_lock":false,"future_field":{"a":1}},"telemetry":{"active_node":"Debian","panic_mode":false,"cursor_x":3200,"cursor_y":720,"peer_addr":"192.168.1.20:24800","game_captive_active":false,"game_reason":"","peer_online":true,"main_conflict":false}}"#;

    fn cfg() -> LayoutConfig {
        LayoutState::parse(STATE_JSON).unwrap().config
    }

    fn has(ps: &[Placement], label: &str) -> bool {
        ps.iter().any(|p| p.label == label)
    }

    #[test]
    fn offers_below_each_monitor_and_outer_sides() {
        let ps = placements(&cfg(), &[]);
        assert!(has(&ps, "Below DP-2"));
        assert!(has(&ps, "Below DP-3"));
        assert!(has(&ps, "Left of DP-3"));
        assert!(has(&ps, "Right of DP-2"));
        assert!(!has(&ps, "Left of DP-2"));
        let below = ps.iter().find(|p| p.label == "Below DP-2").unwrap();
        assert_eq!((below.x, below.y, below.side.as_str()), (1920, 1440, "Bottom"));
    }

    #[test]
    fn snap_lands_on_nearest_edge() {
        let s = snap(&cfg(), 2240, 1400, 0).unwrap();
        assert_eq!(s.side, "Bottom");
        assert_eq!(s.y, 1440);
    }

    #[test]
    fn fit_keeps_all_screens_in_view() {
        let c = cfg();
        let (w, h) = (800.0_f32, 500.0_f32);
        let v = fit(&c, w, h);
        let mut rects: Vec<(i32, i32, i32, i32)> =
            c.rects(Device::Debian).iter().map(|m| (m.x, m.y, m.w, m.h)).collect();
        rects.push(c.laptop_box());
        for (x, y, rw, rh) in rects {
            let (x0, y0) = (v.offset_x + x as f32 * v.scale, v.offset_y + y as f32 * v.scale);
            let (x1, y1) = (x0 + rw as f32 * v.scale, y0 + rh as f32 * v.scale);
            assert!(x0 >= -0.01 && y0 >= -0.01 && x1 <= w + 0.01 && y1 <= h + 0.01, "{x0} {y0} {x1} {y1}");
        }
    }

    #[test]
    fn poll_does_not_move_a_dragged_laptop() {
        let current = LayoutState::parse(STATE_JSON).unwrap();
        let mut moved = current.clone();
        moved.config.move_laptop(0, 1440);
        let kept = merge_poll(Some(current.clone()), Some(moved.clone()), true).unwrap();
        assert_eq!(kept.config.laptop_box().0, 2240);
        let taken = merge_poll(Some(current), Some(moved), false).unwrap();
        assert_eq!(taken.config.laptop_box().0, 0);
    }

    #[test]
    fn starting_the_service_is_reported_until_it_answers() {
        let none = Snapshot::default();
        assert_eq!(start_notice(&ServiceStart::Done, &none), None);
        assert_eq!(start_notice(&ServiceStart::Starting, &none).as_deref(), Some("Starting mesh-kvm on this device."));
        assert!(!notice_is_serious(&none, &ServiceStart::Starting));
        let failed = ServiceStart::Failed("mesh-kvm couldn't start.".into());
        assert_eq!(start_notice(&failed, &none).as_deref(), Some("mesh-kvm couldn't start."));
        assert!(notice_is_serious(&none, &failed));
        assert!(notice_is_serious(&none, &ServiceStart::Done));
        // Once something answers, a failure from earlier no longer applies.
        let up = pair(0, 0);
        assert_eq!(start_notice(&failed, &up), None);
        assert!(!notice_is_serious(&up, &failed));
    }

    #[test]
    fn service_down_message_says_what_to_do() {
        let msg = status_message(&Snapshot::default()).unwrap();
        assert!(msg.contains("Start mesh-kvm"), "{msg}");
    }

    #[test]
    fn peer_down_message_names_firewall_port() {
        let local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        let snap = Snapshot { local: Some(local), ..Default::default() };
        let msg = status_message(&snap).unwrap();
        assert!(msg.contains("TCP port 24880"), "{msg}");
        assert!(msg.starts_with("The other device is out of reach"), "{msg}");
    }

    #[test]
    fn main_conflict_message_says_how_to_fix_it() {
        let mut local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        local.main_conflict = true;
        let snap = Snapshot { local: Some(local.clone()), peer: Some(local), ..Default::default() };
        let msg = status_message(&snap).unwrap();
        assert!(msg.contains("Both devices"), "{msg}");
        assert!(msg.contains("own keyboard and mouse"), "{msg}");
        assert!(msg.contains("Main device"), "{msg}");
    }

    #[test]
    fn healthy_link_has_no_message() {
        let local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        let snap = Snapshot { local: Some(local.clone()), peer: Some(local), ..Default::default() };
        assert_eq!(status_message(&snap), None);
    }

    #[test]
    fn parses_real_api_json() {
        let d: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        assert_eq!(d.node, "debian");
        assert!(d.peer_online);
        assert!(!d.is_older());
        let sharing = d.sharing.as_ref().unwrap();
        assert!(sharing.mouse && !sharing.keyboard);
        assert!(d.clipboard.send && !d.clipboard.receive);
        assert_eq!(d.main_device, 1);
        assert_eq!(d.cursor, Some([3200, 720]));
        // An older service that lacks the newer fields still parses.
        let old: DeviceStatus = serde_json::from_str(r#"{"node":"windows"}"#).unwrap();
        assert!(old.clipboard.send && old.sharing.is_none() && old.is_older());
        let l = LayoutState::parse(STATE_JSON).unwrap();
        assert_eq!(l.config.rects(Device::Debian).len(), 2);
        assert_eq!(l.config.attach_side, "Bottom");
        assert_eq!(l.telemetry.active_node, "Debian");
    }

    #[test]
    fn an_edit_holds_only_what_changed() {
        let was = cfg();
        assert_eq!(layout_edit(&was, &was), None);
        let mut now = was.clone();
        now.edge_resistance = 90;
        assert_eq!(layout_edit(&was, &now), Some(json!({ "edge_resistance": 90 })));
        let mut now = was.clone();
        now.move_laptop(5, 1440);
        now.attach_side = "Right".into();
        let edit = layout_edit(&was, &now).unwrap();
        assert_eq!(edit["arrangement"]["attach_side"], "Right");
        assert_eq!(edit["arrangement"]["devices"][1]["node"], "Windows");
        assert_eq!(edit["arrangement"]["devices"][1]["x"], 5);
        assert!(edit.get("edge_resistance").is_none());
        assert!(edit["arrangement"]["devices"][1].get("monitors").is_none());
    }

    #[test]
    fn a_held_draft_does_not_send_back_what_the_other_device_just_changed() {
        let (mut draft, t) = (LayoutDraft::default(), Instant::now());
        draft.follow(layout_at(1), false, t);
        // The person moves the slider here, and the draft holds while that saves.
        draft.state.as_mut().unwrap().config.edge_resistance = 90;
        draft.edited(t);
        assert_eq!(draft.take_due(t), Some(json!({ "edge_resistance": 90 })));
        // The other device moves the laptop, which arrives as a poll the draft doesn't take yet.
        draft.follow(layout_at(9), false, t + HOLD / 2);
        // A second edit here still says nothing about where the laptop is.
        draft.state.as_mut().unwrap().config.edge_resistance = 95;
        draft.edited(t + HOLD / 2);
        assert_eq!(draft.take_due(t + HOLD / 2), Some(json!({ "edge_resistance": 95 })));
        // Once it settles the draft shows the other device's move.
        draft.follow(layout_at(9), false, t + HOLD * 2);
        assert_eq!(drafted_x(&draft), 9);
    }

    #[test]
    fn going_back_to_the_first_value_while_held_is_still_saved() {
        let (mut draft, t) = (LayoutDraft::default(), Instant::now());
        draft.follow(layout_at(1), false, t);
        let first = draft.state.as_ref().unwrap().config.edge_resistance;
        draft.state.as_mut().unwrap().config.edge_resistance = first + 10;
        draft.edited(t);
        assert!(draft.take_due(t).is_some());
        draft.state.as_mut().unwrap().config.edge_resistance = first;
        draft.edited(t + SAVE_GAP);
        assert_eq!(draft.take_due(t + SAVE_GAP), Some(json!({ "edge_resistance": first })));
    }

    #[test]
    fn a_layout_in_the_older_form_is_not_taken_for_an_empty_one() {
        let old = r#"{"config":{"debian_monitors":[],"win_x":0,"attach_side":"Right"},"telemetry":{}}"#;
        assert_eq!(LayoutState::parse(old), None);
    }

    #[test]
    fn an_older_service_is_named_when_it_cannot_share_the_layout() {
        let mut snap = pair(0, 0);
        snap.peer.as_mut().unwrap().paired = None;
        let msg = status_message(&snap).unwrap();
        assert_eq!(msg, "windows-box is running an older mesh-kvm. Update it, or sign out and back in.");
        // Its own controls still work, as it does have those actions.
        assert!(!snap.peer.as_ref().unwrap().is_older());
    }

    #[test]
    fn with_no_address_the_app_says_so_and_nothing_else_matters_yet() {
        let mut local = device("windows", 0);
        local.paired = Some(false);
        let snap = Snapshot { local: Some(local), layout: LayoutState::parse(STATE_JSON), ..Default::default() };
        assert!(needs_peer(&snap));
        assert!(status_message(&snap).unwrap().contains("the other device's address"), "not the firewall advice");
        assert!(!needs_peer(&pair(0, 0)));
        assert!(!needs_peer(&Snapshot::default()));
        let mut older = pair(0, 0);
        older.local.as_mut().unwrap().paired = None;
        assert!(!needs_peer(&older), "an older service can't take an address");
    }

    #[test]
    fn a_laptop_with_a_monitor_left_of_its_main_one_is_boxed_and_moved_as_one() {
        let mut c = cfg();
        c.device_mut(Device::Windows).unwrap().monitors = vec![
            Display { name: "Display 2".into(), x: -1000, y: -200, w: 1000, h: 800 },
            Display { name: "Display 1".into(), x: 0, y: 0, w: 2560, h: 1600 },
        ];
        assert_eq!(c.laptop_box(), (1240, 1240, 3560, 1800));
        let place = Placement { label: "Custom".into(), x: 0, y: 1440, side: "Bottom".into() };
        apply_placement(&mut c, &place);
        assert_eq!(c.laptop_box(), (0, 1440, 3560, 1800));
        // The device's own origin is where its main monitor is, not where the box starts.
        let laptop = c.device(Device::Windows).unwrap();
        assert_eq!((laptop.x, laptop.y), (1000, 1640));
        let v = fit(&c, 800.0, 500.0);
        let (sx, _) = v.to_screen(0.0, 1440.0);
        assert!(sx >= 0.0);
    }

    fn place_of(c: &LayoutConfig, x: i32, y: i32) -> (i32, i32, String) {
        let p = snap(c, x, y, 0).unwrap();
        (p.x, p.y, p.side)
    }

    // The snap values below were worked out by hand from the page's JavaScript, with the laptop at
    // 2560x1600 beside DP-3 (0,360 1920x1080) and DP-2 (1920,0 2560x1440).

    #[test]
    fn snap_right_of_the_last_monitor() {
        assert_eq!(place_of(&cfg(), 4400, 100), (4480, 100, "Right".into()));
        // y is limited to 1440 - 240, so at least 240 pixels of the edges touch.
        assert_eq!(place_of(&cfg(), 4700, 1300), (4480, 1200, "Right".into()));
    }

    #[test]
    fn snap_left_of_the_first_monitor() {
        assert_eq!(place_of(&cfg(), -2600, 500), (-2560, 500, "Left".into()));
        // y is limited to 360 + 1080 - 240.
        assert_eq!(place_of(&cfg(), -2700, 1500), (-2560, 1200, "Left".into()));
    }

    #[test]
    fn snap_above_a_monitor() {
        assert_eq!(place_of(&cfg(), 3000, -1650), (3000, -1600, "Top".into()));
        // Above DP-3, which starts lower than DP-2: 360 - 1600. DP-2 stands in the way at -500, so
        // it goes as far along as clears DP-2's left edge.
        assert_eq!(place_of(&cfg(), -500, -1300), (-640, -1240, "Top".into()));
    }

    #[test]
    fn placements_round_half_widths_as_javascript_does() {
        let mut c = cfg();
        let at = |c: &LayoutConfig, label: &str| {
            let ps = placements(c, &[]);
            let p = ps.iter().find(|p| p.label == label).unwrap();
            (p.x, p.y)
        };
        c.device_mut(Device::Windows).unwrap().custom_size = Some((1921, 1081));
        // 1920 + 639 / 2 = 2239.5 rounds up.
        assert_eq!(at(&c, "Below DP-2"), (2240, 1440));
        // 0 + -1 / 2 = -0.5 rounds up to 0.
        assert_eq!(at(&c, "Below DP-3"), (0, 1440));
        // 360 + -1 / 2 = 359.5, and 0 + 359 / 2 = 179.5.
        assert_eq!(at(&c, "Left of DP-3"), (-1921, 360));
        assert_eq!(at(&c, "Right of DP-2"), (4480, 180));
        // -1.5 rounds up to -1, not away from zero.
        c.device_mut(Device::Windows).unwrap().custom_size = Some((1923, 1081));
        assert_eq!(at(&c, "Below DP-3"), (-1, 1440));
    }

    #[test]
    fn dragging_rounds_to_whole_pixels_before_snapping() {
        let view = MapView { scale: 0.1, offset_x: 10.0, offset_y: 20.0 };
        // (110.04 - 10) / 0.1 - 40 = 960.4 and (70.06 - 20) / 0.1 - 100.5 = 400.1.
        assert_eq!(drag_target(&view, (110.04, 70.06), (40.0, 100.5)), (960, 400));
    }

    #[test]
    fn map_view_round_trips() {
        let view = MapView { scale: 0.25, offset_x: 8.0, offset_y: -4.0 };
        let (x, y) = view.to_screen(400.0, 800.0);
        assert_eq!((x, y), (108.0, 196.0));
        assert_eq!(view.to_world(x, y), (400.0, 800.0));
    }

    #[test]
    fn a_device_lines_up_with_a_monitor_when_dragged_near_to_it() {
        // The laptop's left edge is 30 pixels off DP-2's.
        assert_eq!(place_of(&cfg(), 1950, 1500), (1950, 1440, "Bottom".into()));
        let lined_up = snap(&cfg(), 1950, 1500, 40).unwrap();
        assert_eq!((lined_up.x, lined_up.y), (1920, 1440));
        // Further off than the magnet reaches, it stays where it was put.
        assert_eq!(snap(&cfg(), 2100, 1500, 40).unwrap().x, 2100);
    }

    #[test]
    fn a_device_never_lands_on_a_monitor() {
        let c = cfg();
        let rects = c.rects(Device::Debian);
        let (.., w, h) = c.laptop_box();
        for x in (-4000..6000).step_by(250) {
            for y in (-3000..3000).step_by(250) {
                let p = snap(&c, x, y, 30).unwrap();
                assert!(!overlaps_any(&rects, (p.x, p.y, w, h)), "{x},{y} gave {p:?}");
            }
        }
        for place in placements(&c, &[]) {
            assert!(!overlaps_any(&rects, (place.x, place.y, w, h)), "{place:?}");
        }
    }

    #[test]
    fn either_device_can_be_dragged_and_the_snap_is_the_same_both_ways() {
        // The laptop dragged down and left of DP-2, and DP-2's device dragged the other way.
        let mut by_laptop = cfg();
        let (lx, ly, ..) = by_laptop.laptop_box();
        drag_device(&mut by_laptop, Device::Windows, lx - 300, ly + 60, 0);
        let mut by_desktop = cfg();
        let (dx, dy, ..) = by_desktop.device_box(Device::Debian);
        drag_device(&mut by_desktop, Device::Debian, dx + 300, dy - 60, 0);
        let gap = |c: &LayoutConfig| {
            let (a, b) = (c.device_box(Device::Debian), c.laptop_box());
            (b.0 - a.0, b.1 - a.1)
        };
        assert_eq!(gap(&by_laptop), gap(&by_desktop));
        assert_eq!(by_desktop.attach_side, by_laptop.attach_side);
        assert_eq!(by_desktop.laptop_box().0, lx, "the other device stays put on the map");
    }

    #[test]
    fn a_devices_own_monitors_keep_their_places_when_it_is_dragged() {
        let mut c = cfg();
        let own = |c: &LayoutConfig| c.device(Device::Debian).unwrap().monitors.clone();
        let spaced = |c: &LayoutConfig| {
            let r = c.rects(Device::Debian);
            (r[1].x - r[0].x, r[1].y - r[0].y)
        };
        let (before, gap) = (own(&c), spaced(&c));
        let (dx, dy, ..) = c.device_box(Device::Debian);
        drag_device(&mut c, Device::Debian, dx + 500, dy - 900, 0);
        assert_eq!(own(&c), before);
        assert_eq!(spaced(&c), gap);
    }

    #[test]
    fn an_edit_from_dragging_the_first_device_moves_it_in_the_save() {
        let was = cfg();
        let mut now = was.clone();
        let (dx, dy, ..) = now.device_box(Device::Debian);
        drag_device(&mut now, Device::Debian, dx + 900, dy, 0);
        let edit = layout_edit(&was, &now).unwrap();
        assert_ne!(edit["arrangement"]["devices"][0]["x"], 0);
    }

    #[test]
    fn the_seam_is_where_the_two_devices_touch() {
        let c = cfg();
        // The laptop at x 2240..4800 under DP-2, which ends at 4480.
        assert_eq!(seams(&c), vec![((2240, 1440), (4480, 1440))]);
        let mut right = cfg();
        apply_placement(&mut right, &Placement { label: "".into(), x: 4480, y: 100, side: "Right".into() });
        assert_eq!(seams(&right), vec![((4480, 100), (4480, 1440))]);
    }

    fn display(modes: &[(i32, i32, u32)], current: usize) -> DisplayInfo {
        let modes: Vec<DisplayMode> = modes
            .iter()
            .map(|&(width, height, millihertz)| DisplayMode { width, height, millihertz, id: String::new() })
            .collect();
        DisplayInfo { id: "DP-2".into(), name: "Big".into(), current: modes.get(current).cloned(), modes, can_change: true, ..Default::default() }
    }

    #[test]
    fn a_monitor_is_matched_by_the_start_of_its_layout_name() {
        assert!(same_monitor("DP-2 (360Hz Native)", "DP-2"));
        assert!(same_monitor("Display 1", "Display 1"));
        assert!(!same_monitor("DP-21 (60Hz Native)", "DP-2"));
        assert!(!same_monitor("Display 12", "Display 1"));
    }

    #[test]
    fn a_label_comes_from_the_monitor_or_failing_that_the_layout() {
        let rect = Display { name: "DP-2 (360Hz Native)".into(), w: 2560, h: 1440, ..Default::default() };
        assert_eq!(monitor_label(&rect, None), ("DP-2".into(), "2560 \u{d7} 1440".into(), Some("360 Hz".into())));
        let d = display(&[(1920, 1080, 143_912)], 0);
        assert_eq!(monitor_label(&rect, Some(&d)), ("Big".into(), "1920 \u{d7} 1080".into(), Some("144 Hz".into())));
    }

    #[test]
    fn a_rate_still_shows_when_the_desktop_isnt_answering_about_modes() {
        let rect = Display { name: "DP-2 (360Hz Native)".into(), w: 2560, h: 1440, ..Default::default() };
        assert_eq!(monitor_label(&rect, None).2.as_deref(), Some("360 Hz"));
        assert_eq!(rate_in_layout_name("DP-2 (Primary Native)"), None);
        assert_eq!(rate_in_layout_name("Display 1"), None);
    }

    #[test]
    fn the_smallest_window_grows_with_app_size_but_fits_the_monitor() {
        let base = (760.0, 560.0);
        assert_eq!(smallest_window(base, 1.0, 1.0, Some((1920.0, 1080.0))), base);
        assert_eq!(smallest_window(base, 1.5, 1.5, None), base);
        // At 150% on a small laptop screen, in points at that zoom: 1366x768 is 910x512.
        let (w, h) = smallest_window(base, 1.5, 1.5, Some((1366.0 / 1.5, 768.0 / 1.5)));
        assert_eq!(w, 760.0);
        assert!(h <= 768.0 / 1.5 * 0.9 + 0.01, "{h}");
        assert_eq!(smallest_window((1000.0, 800.0), 1.0, 1.0, Some((800.0, 600.0))), (720.0, 540.0));
    }

    #[test]
    fn an_answer_names_the_change_it_is_for() {
        let change = ModeChange { id: 42, ..Default::default() };
        assert_eq!(mode_answer(&change, true), json!({ "action": "keep_display_mode", "change_id": 42 }));
        assert_eq!(mode_answer(&change, false), json!({ "action": "undo_display_mode", "change_id": 42 }));
    }

    #[test]
    fn menus_list_each_size_once_and_its_rates_fastest_first() {
        let d = display(&[(2560, 1440, 144_000), (2560, 1440, 60_000), (1920, 1080, 60_000)], 0);
        assert_eq!(resolutions(&d), [(2560, 1440), (1920, 1080)]);
        assert_eq!(rates(&d, (2560, 1440)).len(), 2);
        assert_eq!(rates(&d, (800, 600)).len(), 0);
    }

    #[test]
    fn rates_that_would_read_the_same_show_their_decimals() {
        let d = display(&[(1920, 1080, 60_000), (1920, 1080, 59_940), (1920, 1080, 50_000)], 0);
        let all = rates(&d, (1920, 1080));
        assert_eq!(rate_label(all[0], &all), "60.00 Hz");
        assert_eq!(rate_label(all[1], &all), "59.94 Hz");
        assert_eq!(rate_label(all[2], &all), "50 Hz");
    }

    #[test]
    fn a_new_size_keeps_the_rate_if_it_can() {
        let d = display(&[(2560, 1440, 144_000), (2560, 1440, 60_000), (1920, 1080, 144_000), (1280, 720, 60_000)], 0);
        assert_eq!(mode_for_size(&d, (1920, 1080)).unwrap().millihertz, 144_000);
        assert_eq!(mode_for_size(&d, (1280, 720)).unwrap().millihertz, 60_000);
        assert_eq!(mode_for_size(&d, (800, 600)), None);
    }

    #[test]
    fn a_mode_change_names_the_mode_by_its_numbers() {
        let mode = DisplayMode { width: 1920, height: 1080, millihertz: 59_940, id: "9".into() };
        assert_eq!(
            mode_action("DP-2", &mode),
            json!({"action": "set_display_mode", "output": "DP-2", "width": 1920, "height": 1080, "millihertz": 59940})
        );
    }

    #[test]
    fn a_big_window_enlarges_things_a_little_and_a_small_one_not_at_all() {
        assert_eq!(window_zoom(760.0, 560.0), 1.0);
        assert_eq!(window_zoom(1280.0, 800.0), 1.0);
        let full_hd = window_zoom(1920.0, 1080.0);
        assert!(full_hd > 1.1 && full_hd < 1.25, "{full_hd}");
        assert!(window_zoom(7680.0, 4320.0) <= 1.75 + f32::EPSILON);
        // Wide but short doesn't count as big.
        assert_eq!(window_zoom(3000.0, 700.0), 1.0);
    }

    #[test]
    fn the_devices_go_side_by_side_only_with_room_for_both() {
        assert_eq!(body_layout(760.0), BodyLayout { rail: 300.0, columns: 1 });
        assert_eq!(body_layout(1100.0).columns, 1);
        let wide = body_layout(1700.0);
        assert_eq!(wide.columns, 2);
        assert!(1700.0 - wide.rail > 900.0, "the arrangement stays the larger part");
    }

    #[test]
    fn names_come_from_status_then_the_layout_then_plain_words() {
        let mut snap = pair(0, 0);
        snap.local.as_mut().unwrap().device_name = "Desk tower".into();
        assert_eq!(snap.name(Device::Debian), "Desk tower");
        snap.local.as_mut().unwrap().device_name.clear();
        snap.local.as_mut().unwrap().hostname = "bread".into();
        assert_eq!(snap.name(Device::Debian), "bread");
        // Out of reach, the other device goes by what the layout last heard.
        snap.peer = None;
        assert_eq!(snap.name(Device::Windows), "laptop");
        snap.layout = None;
        assert_eq!(snap.name(Device::Windows), "the other device");
        assert_eq!(snap.title(Device::Windows), "The other device");
    }

    #[test]
    fn with_no_service_the_devices_still_have_words_to_go_by() {
        let snap = Snapshot::default();
        let [here, there] = snap.in_order();
        assert_eq!(here, Device::of_this_build());
        assert_eq!((snap.title(here).as_str(), snap.title(there).as_str()), ("This device", "The other device"));
    }

    #[test]
    fn a_pending_mode_change_is_found_on_either_device() {
        let mut snap = pair(0, 0);
        assert!(snap.mode_change().is_none());
        snap.peer.as_mut().unwrap().display_change.change =
            Some(ModeChange { stage: "waiting".into(), ms_left: 9000, ..Default::default() });
        let (device, change) = snap.mode_change().unwrap();
        assert_eq!(device, Device::Windows);
        assert!(change.is_waiting());
    }

    #[test]
    fn a_mode_change_parses_from_status() {
        let text = r#"{"node":"debian","display_change":{"change":{"output":"DP-2","name":"Big","stage":"waiting","ms_left":14000,
            "from":{"width":2560,"height":1440,"millihertz":144000,"id":"1"},"to":{"width":1920,"height":1080,"millihertz":60000,"id":"2"}},"note":null}}"#;
        let d: DeviceStatus = serde_json::from_str(text).unwrap();
        let change = d.display_change.change.unwrap();
        assert_eq!(change.to.unwrap().label(), "1920 \u{d7} 1080 at 60 Hz");
        assert_eq!(change.ms_left, 14000);
    }

    #[test]
    fn position_label_names_a_place_or_the_edge() {
        let mut c = cfg();
        c.move_laptop(1920, 1440);
        assert_eq!(position_label(&c, &placements(&c, &[])), "Below DP-2");
        c.move_laptop(2240, 1440);
        assert_eq!(position_label(&c, &placements(&c, &[])), "Custom (bottom edge)");
    }

    #[test]
    fn an_older_service_is_named_and_the_controls_that_need_it_are_off() {
        let mut snap = pair(1, 1);
        snap.peer.as_mut().unwrap().sharing = None;
        let msg = status_message(&snap).unwrap();
        assert_eq!(msg, "windows-box is running an older mesh-kvm. Update it, or sign out and back in.");
        assert!(!enabled(&snap).main_device);
        assert!(enabled(&snap).test_sound);
        let mut snap = pair(1, 1);
        snap.local.as_mut().unwrap().sharing = None;
        assert!(status_message(&snap).unwrap().starts_with("debian-box is running an older mesh-kvm."));
        assert!(!enabled(&snap).main_device);
        // A current pair is unaffected.
        assert!(enabled(&pair(1, 1)).main_device);
    }

    #[test]
    fn the_older_text_names_the_device() {
        assert_eq!(older_message("windows-box"), "windows-box is running an older mesh-kvm. Update it, or sign out and back in.");
    }

    #[test]
    fn the_panic_notice_shows_when_either_device_reports_it() {
        let mut snap = pair(1, 1);
        snap.local.as_mut().unwrap().main_device = 1;
        assert_eq!(status_message(&snap), None);
        snap.peer.as_mut().unwrap().panic_mode = true;
        assert!(status_message(&snap).unwrap().starts_with("All input is released"));
        let mut snap = pair(1, 1);
        snap.local.as_mut().unwrap().panic_mode = true;
        assert!(status_message(&snap).unwrap().starts_with("All input is released"));
    }

    fn device(node: &str, main_device: u8) -> DeviceStatus {
        let mut d: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        d.node = node.into();
        d.device_name = format!("{node}-box");
        d.main_device = main_device;
        d
    }

    fn pair(debian_main: u8, windows_main: u8) -> Snapshot {
        Snapshot {
            local: Some(device("debian", debian_main)),
            peer: Some(device("windows", windows_main)),
            layout: Some(LayoutState::parse(STATE_JSON).unwrap()),
            ..Default::default()
        }
    }

    fn assert_only_main_device_is_offered(msg: &str) {
        assert!(msg.contains("Choose Main device"), "{msg}");
        assert!(!msg.contains("F11") && !msg.contains("F12") && !msg.contains("switch"), "{msg}");
    }

    #[test]
    fn neither_device_being_main_is_explained() {
        // Debian says the laptop is main, and the laptop says Debian is.
        let msg = status_message(&pair(1, 0)).unwrap();
        assert!(msg.contains("the other one"), "{msg}");
        assert_only_main_device_is_offered(&msg);
    }

    #[test]
    fn two_mains_are_explained_before_the_router_flags_a_conflict() {
        let msg = status_message(&pair(0, 1)).unwrap();
        assert!(msg.contains("Both devices"), "{msg}");
        assert_only_main_device_is_offered(&msg);
    }

    #[test]
    fn devices_that_agree_have_no_main_message() {
        assert_eq!(status_message(&pair(0, 0)), None);
        assert_eq!(status_message(&pair(1, 1)), None);
    }

    #[test]
    fn a_split_outranks_released_input_because_the_switch_cannot_fix_it() {
        let mut snap = pair(1, 0);
        snap.layout.as_mut().unwrap().telemetry.panic_mode = true;
        assert_only_main_device_is_offered(&status_message(&snap).unwrap());
    }

    #[test]
    fn released_input_points_at_the_switch() {
        let mut snap = pair(0, 0);
        snap.layout.as_mut().unwrap().telemetry.panic_mode = true;
        assert!(status_message(&snap).unwrap().contains("Use the switch"));
    }

    #[test]
    fn an_unreachable_peer_is_explained_even_with_input_released() {
        // The switch is disabled here, so its advice would be no use.
        let mut snap = pair(0, 0);
        snap.peer = None;
        snap.local.as_mut().unwrap().panic_mode = true;
        snap.layout = None;
        assert!(status_message(&snap).unwrap().contains("TCP port 24880"));
    }

    #[test]
    fn nothing_works_without_the_service() {
        let on = enabled(&Snapshot::default());
        assert_eq!(
            on,
            ControlsEnabled { switch: false, main_device: false, layout: false, release: false, test_sound: false }
        );
    }

    #[test]
    fn everything_works_when_linked() {
        let on = enabled(&pair(0, 0));
        assert_eq!(
            on,
            ControlsEnabled { switch: true, main_device: true, layout: true, release: true, test_sound: true }
        );
    }

    #[test]
    fn only_local_controls_work_without_the_peer() {
        // Either device can still edit its own copy of the layout.
        for this_device in ["debian", "windows"] {
            let snap = Snapshot {
                local: Some(device(this_device, 0)),
                layout: LayoutState::parse(STATE_JSON),
                ..Default::default()
            };
            let on = enabled(&snap);
            assert!(on.layout && on.release, "{this_device}");
            assert!(!on.switch && !on.main_device && !on.test_sound, "{this_device}");
        }
    }

    #[test]
    fn a_link_that_is_down_disables_the_switch_and_main_device() {
        let mut snap = pair(0, 0);
        snap.layout.as_mut().unwrap().telemetry.peer_online = false;
        let on = enabled(&snap);
        assert!(!on.switch && !on.main_device);
    }

    #[test]
    fn a_main_conflict_leaves_main_device_as_the_way_out() {
        let mut snap = pair(0, 1);
        snap.peer.as_mut().unwrap().main_conflict = true;
        let on = enabled(&snap);
        assert!(!on.switch);
        assert!(on.main_device);
    }

    #[test]
    fn main_device_is_named_only_when_the_devices_agree() {
        assert_eq!(pair(1, 1).main_device(), Some(Device::Windows));
        assert_eq!(pair(0, 1).main_device(), None);
        let alone = Snapshot { local: Some(device("debian", 0)), ..Default::default() };
        assert_eq!(alone.main_device(), Some(Device::Debian));
        assert_eq!(Snapshot::default().main_device(), None);
    }

    #[test]
    fn badges_follow_the_active_device() {
        let snap = pair(0, 0);
        assert_eq!(device_badge(&snap, Device::Debian), (Light::Active, "Has keyboard and mouse"));
        assert_eq!(device_badge(&snap, Device::Windows), (Light::Connected, "Connected"));
        assert_eq!(device_badge(&Snapshot::default(), Device::Debian), (Light::Off, "Not connected"));
    }

    #[test]
    fn a_device_the_link_cannot_reach_is_not_shown_as_connected() {
        // Its service still answers the app, but no input can get to it.
        let snap = link_snapshot(false, LinkStatus::default(), true);
        assert_eq!(device_badge(&snap, Device::Windows), (Light::Off, "Link down"));
        assert_eq!(device_badge(&snap, Device::Debian), (Light::Active, "Has keyboard and mouse"));
    }

    #[test]
    fn an_unlinked_peer_never_claims_the_keyboard() {
        let mut snap = pair(0, 0);
        let layout = snap.layout.as_mut().unwrap();
        layout.telemetry.peer_online = false;
        layout.telemetry.active_node = "Windows".into();
        assert_eq!(device_badge(&snap, Device::Windows).0, Light::Off);
    }

    #[test]
    fn release_reaches_both_devices_when_this_service_can_do_that() {
        assert_eq!(release_route(&pair(0, 0)), Some(Release::Everywhere));
        let mut laptop_here = pair(0, 0);
        laptop_here.local.as_mut().unwrap().node = "windows".into();
        assert_eq!(release_route(&laptop_here), Some(Release::Everywhere));
        let alone = Snapshot { local: Some(device("windows", 0)), ..Default::default() };
        assert_eq!(release_route(&alone), Some(Release::Only(Device::Windows)));
        assert_eq!(release_route(&Snapshot::default()), None);
    }

    #[test]
    fn cursor_is_placed_on_whichever_device_is_active() {
        let mut snap = pair(0, 0);
        let c = snap.layout.as_ref().unwrap().config.clone();
        assert_eq!(cursor_position(&snap, &c), Some((3200, 720)));
        snap.layout.as_mut().unwrap().telemetry.active_node = "Windows".into();
        snap.peer.as_mut().unwrap().cursor = Some([100, 200]);
        // The laptop's own cursor (100, 200), from its origin at (2240, 1440).
        assert_eq!(cursor_position(&snap, &c), Some((2340, 1640)));
        // A monitor to the left of the laptop's main one has negative coordinates.
        snap.peer.as_mut().unwrap().cursor = Some([-100, 200]);
        assert_eq!(cursor_position(&snap, &c), Some((2140, 1640)));
        snap.peer.as_mut().unwrap().cursor = None;
        assert_eq!(cursor_position(&snap, &c), None);
    }

    #[test]
    fn controls_read_and_write_the_service_fields() {
        let d: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        assert_eq!(Control::MouseSharing.read(&d), 1.0);
        assert_eq!(Control::KeyboardSharing.read(&d), 0.0);
        assert_eq!(Control::AudioPlay.read(&d), 1.0, "not muted");
        assert_eq!(Control::MouseSharing.action(0.0), json!({"action": "set_sharing", "mouse": false}));
        assert_eq!(Control::ClipboardReceive.action(1.0), json!({"action": "set_clipboard", "receive": true}));
        assert_eq!(Control::AudioPlay.action(0.0), json!({"action": "set_audio_rx_muted", "muted": true}));
        assert_eq!(Control::PlayVolume.action(85.0), json!({"action": "set_audio_rx_volume", "volume": 85}));
        assert_eq!(Control::PointerSpeed.action(0.35000000149), json!({"action": "set_pointer_speed", "speed": 0.35}));
    }

    #[test]
    fn both_devices_sending_is_called_out_only_when_an_echo_could_happen() {
        let linked = || {
            let local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
            let peer = DeviceStatus { node: "windows".into(), ..local.clone() };
            Snapshot { local: Some(local), peer: Some(peer), layout: LayoutState::parse(STATE_JSON), ..Default::default() }
        };
        const WARNING: &str = "Both devices are sending output sound, and one can't leave out what it plays, so you may hear an echo. Choose None under Output to send on one of them.";
        let mut snap = linked();
        snap.local.as_mut().unwrap().audio.output = "default".into();
        assert_eq!(status_message(&snap), None, "one device sending is the normal case");
        snap.peer.as_mut().unwrap().audio.output = "speakers".into();
        assert_eq!(status_message(&snap).as_deref(), Some(WARNING), "an older service can't leave it out");
        snap.local.as_mut().unwrap().audio.echo_safe = true;
        assert_eq!(status_message(&snap).as_deref(), Some(WARNING), "both have to");
        snap.peer.as_mut().unwrap().audio.echo_safe = true;
        assert_eq!(status_message(&snap), None);
        snap.peer.as_mut().unwrap().audio.echo_safe = false;
        snap.peer.as_mut().unwrap().audio.output.clear();
        snap.peer.as_mut().unwrap().audio.input = "mic".into();
        assert_eq!(status_message(&snap), None, "a microphone isn't a loop");
    }

    #[test]
    fn play_only_on_the_other_device_reads_and_writes_the_service_field() {
        let mut d: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        assert_eq!(d.audio.play_only_on_other, None, "an older service has no such field");
        assert_eq!(Control::PlayOnlyOnOther.read(&d), 0.0);
        d.audio.play_only_on_other = Some(true);
        assert_eq!(Control::PlayOnlyOnOther.read(&d), 1.0);
        assert_eq!(
            Control::PlayOnlyOnOther.action(0.0),
            json!({"action": "set_play_only_on_other", "enabled": false})
        );
        let newer = LOCAL_JSON.replace(r#""tx_source":0,"#, r#""play_only_on_other":false,"#);
        let d: DeviceStatus = serde_json::from_str(&newer).unwrap();
        assert_eq!(d.audio.play_only_on_other, Some(false));
    }

    #[test]
    fn the_bitrate_reads_and_writes_the_service_field() {
        let mut d: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        d.audio.bitrate = 128;
        assert_eq!(Control::AudioBitrate.read(&d), 128.0);
        assert_eq!(Control::AudioBitrate.action(0.0), json!({"action": "set_audio_bitrate", "kbps": 0}));
        assert_eq!(bitrate_label(64), "64 kbit/s");
        assert_eq!(bitrate_label(0), "Uncompressed, about 1.6 Mbit/s");
    }

    #[test]
    fn device_lists_come_from_the_status_with_readable_names() {
        let text = LOCAL_JSON.replace(
            r#""tx_source":0,"#,
            r#""output":"usb-1","outputs":[{"id":"usb-1","name":"USB speakers"}],"inputs":[],"notes":["Hello"],"#,
        );
        let d: DeviceStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(device_label(&d.audio.output, &d.audio.outputs), "USB speakers");
        assert_eq!(d.audio.notes, ["Hello"]);
        assert_eq!(device_label("", &d.audio.outputs), "None");
        assert_eq!(device_label("default", &d.audio.outputs), "System default");
        assert_eq!(device_label("usb-2", &d.audio.outputs), "Not connected, using the default");
    }

    #[test]
    fn received_audio_can_t_be_set_to_nowhere() {
        assert!(DeviceRole::Input.fixed_options().iter().any(|(id, _)| id.is_empty()));
        assert!(DeviceRole::Output.fixed_options().iter().any(|(id, _)| id.is_empty()));
        assert!(!DeviceRole::Play.fixed_options().iter().any(|(id, _)| id.is_empty()));
    }

    #[test]
    fn quality_lines_read_plainly() {
        let mut audio = AudioStatus { tx_loss: 3, ..Default::default() };
        assert_eq!(sending_quality(&audio, "Laptop"), "Not reaching Laptop yet.");
        audio.tx_connected = true;
        assert_eq!(sending_quality(&audio, "Laptop"), "Connected to Laptop, with no sound to send right now.");
        audio.tx_reaching = true;
        assert_eq!(sending_quality(&audio, "Laptop"), "Playing on Laptop, 3% lost on the way.");
        assert_eq!(receiving_quality(&audio), None, "nothing is arriving");
        audio.rx_live = true;
        audio.rx_loss = 2.5;
        audio.rx_unrepaired = 0.4;
        audio.rx_buffer_ms = 30;
        assert_eq!(
            receiving_quality(&audio).unwrap(),
            "Received: 2.5% lost, 0.4% couldn't be repaired, 30 ms held back to smooth it out."
        );
        audio.rx_live = false;
        assert_eq!(receiving_quality(&audio), None, "the stream ended, so its figures are gone");
    }

    fn linked_status(link: LinkStatus) -> DeviceStatus {
        DeviceStatus { peer_online: true, link: Some(link), ..DeviceStatus::default() }
    }

    #[test]
    fn the_link_line_gives_the_worst_loss_and_the_round_trip() {
        let link = LinkStatus { measured: true, loss_in: Some(1.2), loss_out: Some(0.4), round_trip_ms: Some(23), ..LinkStatus::default() };
        assert_eq!(link_line(&linked_status(link), "Laptop"), Some(("Link: 1.2% lost, 23 ms round trip.".to_string(), false)));
    }

    #[test]
    fn audio_loss_counts_towards_the_link_line() {
        let link = LinkStatus { measured: true, loss_in: Some(0.5), audio_loss_in: Some(2.5), audio_loss_out: Some(7.0), round_trip_ms: Some(40), ..LinkStatus::default() };
        assert_eq!(link_line(&linked_status(link.clone()), "Laptop"), Some(("Link: 7.0% lost, 40 ms round trip.".to_string(), true)));
        let detail = link_detail(&link);
        assert!(detail.contains("Audio: 2.5% lost on the way here, 7.0% on the way there."), "{detail}");
    }

    #[test]
    fn a_slow_link_stands_out_too() {
        let link = LinkStatus { measured: true, round_trip_ms: Some(POOR_ROUND_TRIP_MS), ..LinkStatus::default() };
        assert!(link_line(&linked_status(link), "Laptop").unwrap().1);
    }

    #[test]
    fn the_link_line_says_when_it_has_nothing_to_go_on() {
        let line = link_line(&linked_status(LinkStatus::default()), "Laptop");
        assert_eq!(line, Some(("Link is up, and being measured.".to_string(), false)));
        let link = LinkStatus { measured: true, loss_in: Some(1.0), ..LinkStatus::default() };
        assert_eq!(link_line(&linked_status(link), "Laptop"), Some(("Link: 1.0% lost.".to_string(), false)));
    }

    #[test]
    fn a_link_that_is_down_shows_no_figures() {
        let link = LinkStatus { measured: true, loss_in: Some(3.0), round_trip_ms: Some(23), ..LinkStatus::default() };
        let status = DeviceStatus { peer_online: false, link: Some(link), ..DeviceStatus::default() };
        assert_eq!(link_line(&status, "Laptop"), Some(("Link is down, so each device is using its own keyboard and mouse.".to_string(), true)));
    }

    #[test]
    fn a_dropout_says_where_control_is_going() {
        let link = LinkStatus { returning: true, ..LinkStatus::default() };
        let status = DeviceStatus { peer_online: false, link: Some(link.clone()), ..DeviceStatus::default() };
        assert_eq!(link_line(&status, "Laptop").unwrap().0, "Link is down. Control goes back to Laptop when it returns.");
        assert_eq!(link_line(&linked_status(link), "Laptop").unwrap().0, "Link is back. Control is on its way to Laptop.");
    }

    #[test]
    fn a_one_way_link_says_so_with_no_figures() {
        let link = LinkStatus { measured: true, loss_in: Some(0.0), one_way: true, ..LinkStatus::default() };
        assert_eq!(link_line(&linked_status(link.clone()), "Laptop"), Some(("Link is one way: Laptop can't hear this device.".to_string(), true)));
        let message = status_message(&link_snapshot(true, link, true)).unwrap();
        assert!(message.contains("can't hear this device"), "{message}");
    }

    #[test]
    fn a_loss_that_is_not_known_yet_is_not_shown_as_none() {
        let link = LinkStatus { measured: true, round_trip_ms: Some(23), ..LinkStatus::default() };
        assert_eq!(link_line(&linked_status(link.clone()), "Laptop").unwrap().0, "Link: 23 ms round trip.");
        assert!(link_detail(&link).contains("not known"), "{}", link_detail(&link));
    }

    #[test]
    fn a_service_that_does_not_measure_the_link_gets_no_line() {
        assert_eq!(link_line(&DeviceStatus::default(), "Laptop"), None);
    }

    fn link_snapshot(online: bool, link: LinkStatus, peer: bool) -> Snapshot {
        let mut snap = pair(0, 0);
        let local = snap.local.as_mut().unwrap();
        local.peer_online = online;
        local.link = Some(link);
        if !peer {
            snap.peer = None;
        }
        if let Some(layout) = snap.layout.as_mut() {
            layout.telemetry.peer_online = online;
        }
        snap
    }

    #[test]
    fn a_dropout_is_explained_even_with_the_other_device_out_of_reach() {
        let link = LinkStatus { returning: true, ..LinkStatus::default() };
        let message = status_message(&link_snapshot(false, link, false)).unwrap();
        assert!(message.contains("dropped"), "{message}");
        assert!(message.contains("Control goes back"), "{message}");
    }

    #[test]
    fn an_older_device_on_the_link_is_named() {
        let link = LinkStatus { peer_version: "older".into(), ..LinkStatus::default() };
        let message = status_message(&link_snapshot(true, link, true)).unwrap();
        assert!(message.contains("older mesh-kvm"), "{message}");
        assert!(message.contains("Update both devices together"), "{message}");
    }

    #[test]
    fn a_device_on_another_version_is_said_plainly() {
        let link = LinkStatus { peer_version: "other".into(), ..LinkStatus::default() };
        let message = status_message(&link_snapshot(true, link, true)).unwrap();
        assert!(message.contains("different versions"), "{message}");
    }

    #[test]
    fn devices_on_one_version_have_nothing_to_say() {
        let link = LinkStatus { peer_version: "same".into(), ..LinkStatus::default() };
        assert_eq!(status_message(&link_snapshot(true, link, true)), None);
    }

    #[test]
    fn link_status_reads_from_the_service() {
        let d: DeviceStatus = serde_json::from_str(r#"{"link":{"measured":true,"loss_in":1.5,"loss_out":0.5,"round_trip_ms":12,"returning":false,"peer_version":"same","audio_loss_in":null,"audio_loss_out":2.0}}"#).unwrap();
        let link = d.link.unwrap();
        assert_eq!((link.loss_in, link.round_trip_ms, link.audio_loss_in, link.audio_loss_out), (Some(1.5), Some(12), None, Some(2.0)));
        assert_eq!(link.peer_version, "same");
    }

    const KEY: (Device, Control) = (Device::Windows, Control::SendVolume);

    #[test]
    fn an_edit_shows_until_the_service_has_had_time_to_catch_up() {
        let (mut edits, t) = (Edits::default(), Instant::now());
        assert_eq!(edits.shown(KEY, 10.0, t), 10.0);
        edits.set(KEY, 50.0, t);
        assert_eq!(edits.shown(KEY, 10.0, t + HOLD / 2), 50.0);
        assert_eq!(edits.take_due(t).len(), 1);
        assert_eq!(edits.shown(KEY, 10.0, t + HOLD), 10.0);
    }

    #[test]
    fn a_dragged_control_sends_at_a_limited_rate_and_ends_on_the_last_value() {
        let (mut edits, t) = (Edits::default(), Instant::now());
        edits.set(KEY, 50.0, t);
        assert_eq!(edits.take_due(t), vec![(KEY, 50.0)]);
        let soon = t + SEND_GAP / 2;
        edits.set(KEY, 55.0, soon);
        edits.set(KEY, 60.0, soon);
        assert_eq!(edits.take_due(soon), vec![]);
        assert_eq!(edits.next_wake(soon), Some(SEND_GAP / 2));
        assert_eq!(edits.take_due(t + SEND_GAP), vec![(KEY, 60.0)]);
        assert_eq!(edits.take_due(t + SEND_GAP), vec![]);
    }

    #[test]
    fn edits_leave_the_window_idle_once_they_are_done() {
        let (mut edits, t) = (Edits::default(), Instant::now());
        assert_eq!(edits.next_wake(t), None);
        edits.set(KEY, 50.0, t);
        assert_eq!(edits.next_wake(t), Some(Duration::ZERO));
        edits.take_due(t);
        assert_eq!(edits.next_wake(t), Some(HOLD));
        edits.take_due(t + HOLD);
        assert_eq!(edits.next_wake(t + HOLD), None);
    }

    fn layout_at(x: i32) -> Option<LayoutState> {
        let mut l = LayoutState::parse(STATE_JSON).unwrap();
        l.config.move_laptop(x, 1440);
        Some(l)
    }

    fn drafted_x(draft: &LayoutDraft) -> i32 {
        draft.state.as_ref().unwrap().config.laptop_box().0
    }

    #[test]
    fn a_draft_follows_the_poll_except_during_a_drag() {
        let (mut draft, t) = (LayoutDraft::default(), Instant::now());
        draft.follow(layout_at(1), false, t);
        assert_eq!(drafted_x(&draft), 1);
        draft.follow(layout_at(2), true, t);
        assert_eq!(drafted_x(&draft), 1);
        draft.follow(layout_at(2), false, t);
        assert_eq!(drafted_x(&draft), 2);
    }

    #[test]
    fn a_saved_edit_is_not_undone_by_a_poll_from_before_the_save() {
        let (mut draft, t) = (LayoutDraft::default(), Instant::now());
        draft.follow(layout_at(1), false, t);
        draft.state.as_mut().unwrap().config.move_laptop(5, 1440);
        draft.edited(t);
        assert_eq!(draft.take_due(t).unwrap()["arrangement"]["devices"][1]["x"], 5);
        draft.follow(layout_at(1), false, t + HOLD / 2);
        assert_eq!(drafted_x(&draft), 5);
        draft.follow(layout_at(5), false, t + HOLD);
        assert_eq!(drafted_x(&draft), 5);
        draft.follow(layout_at(9), false, t + HOLD);
        assert_eq!(drafted_x(&draft), 9);
    }

    #[test]
    fn saves_are_spaced_out_and_the_last_edit_is_never_lost() {
        let (mut draft, t) = (LayoutDraft::default(), Instant::now());
        draft.follow(layout_at(1), false, t);
        assert!(draft.take_due(t).is_none());
        let resist = |d: &mut LayoutDraft, value| d.state.as_mut().unwrap().config.edge_resistance = value;
        resist(&mut draft, 70);
        draft.edited(t);
        assert!(draft.take_due(t).is_some());
        resist(&mut draft, 80);
        draft.edited(t);
        assert!(draft.take_due(t).is_none());
        assert_eq!(draft.next_wake(t), Some(SAVE_GAP));
        // Unsaved edits outlast the hold.
        draft.follow(layout_at(9), false, t + HOLD * 2);
        assert_eq!(drafted_x(&draft), 1);
        assert!(draft.take_due(t + SAVE_GAP).is_some());
    }

    #[test]
    fn a_draft_stops_asking_for_repaints_when_settled() {
        let (mut draft, t) = (LayoutDraft::default(), Instant::now());
        draft.follow(layout_at(1), false, t);
        assert_eq!(draft.next_wake(t), None);
        draft.hold(t);
        assert_eq!(draft.next_wake(t), Some(HOLD));
        assert_eq!(draft.next_wake(t + HOLD), None);
    }
}
