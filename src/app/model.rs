//! What the app knows about the two devices, and the layout maths. No egui types in here.

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

/// The laptop sizes offered besides its native one.
pub const RESOLUTIONS: [(i32, i32); 6] =
    [(1920, 1080), (1920, 1200), (2560, 1440), (2560, 1600), (2880, 1800), (3840, 2160)];

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

    /// For the middle of a sentence.
    pub fn name(self) -> &'static str {
        match self {
            Device::Debian => "the Debian desktop",
            Device::Windows => "the Windows laptop",
        }
    }

    /// For headings and labels.
    pub fn title(self) -> &'static str {
        match self {
            Device::Debian => "Debian desktop",
            Device::Windows => "Windows laptop",
        }
    }

    /// The everyday name, for a device already known to be the other one.
    pub fn nickname(self) -> &'static str {
        match self {
            Device::Debian => "the desktop",
            Device::Windows => "the laptop",
        }
    }

    /// The `active_node` value, also shown on the slide switch.
    pub fn short_name(self) -> &'static str {
        match self {
            Device::Debian => "Debian",
            Device::Windows => "Windows",
        }
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
}

impl Default for ClipboardSharing {
    fn default() -> Self {
        Self { send: true, receive: true, note: String::new() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioStatus {
    pub tx_enabled: bool,
    pub tx_volume: u32,
    pub tx_peak: u32,
    pub tx_source: Option<u32>,
    /// Missing from an older service, which can't mute itself.
    pub mute_while_sending: Option<bool>,
    pub rx_muted: bool,
    pub rx_volume: u32,
    pub rx_peak: u32,
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
pub fn older_message(device: Device) -> String {
    let name = device.name();
    let mut c = name.chars();
    let name: String = c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect());
    format!("{name} is running an older mesh-kvm. Update it, or sign out and back in.")
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
        self.devices.iter().find(|d| d.node == device.short_name())
    }

    fn device_mut(&mut self, device: Device) -> Option<&mut LayoutDevice> {
        self.devices.iter_mut().find(|d| d.node == device.short_name())
    }

    /// A device's screens in the layout.
    pub fn rects(&self, device: Device) -> Vec<Display> {
        self.device(device).map(LayoutDevice::rects).unwrap_or_default()
    }

    /// The box around the laptop's screens: `(x, y, w, h)`.
    pub fn laptop_box(&self) -> (i32, i32, i32, i32) {
        let (left, top, right, bottom) = bounds(&self.rects(Device::Windows));
        (left, top, right - left, bottom - top)
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
    /// The config exactly as the service sent it, so saving keeps fields this app doesn't know.
    raw_config: Value,
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
        let config = serde_json::from_value(state.config.clone()).ok()?;
        Some(Self { config, telemetry: state.telemetry, raw_config: state.config })
    }

    /// The body for `POST /api/layout`: every field the service sent, with the ones the app
    /// edits replaced by their current values.
    pub fn to_save_json(&self) -> Value {
        let mut out = self.raw_config.clone();
        if !out.is_object() {
            out = Value::Object(Default::default());
        }
        let c = &self.config;
        if let Some(devices) = out["devices"].as_array_mut() {
            for (saved, device) in devices.iter_mut().zip(&c.devices) {
                saved["x"] = device.x.into();
                saved["y"] = device.y.into();
                saved["custom_size"] = json!(device.custom_size);
            }
        }
        out["attach_side"] = c.attach_side.clone().into();
        out["edge_resistance"] = c.edge_resistance.into();
        out["auto_game_lock"] = c.auto_game_lock.into();
        out["manual_game_lock"] = c.manual_game_lock.into();
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub local: Option<DeviceStatus>,
    pub peer: Option<DeviceStatus>,
    pub layout: Option<LayoutState>,
}

impl Snapshot {
    /// What `device` last reported, if it can be reached.
    pub fn device(&self, device: Device) -> Option<&DeviceStatus> {
        [&self.local, &self.peer].into_iter().flatten().find(|d| d.node == device.node())
    }

    pub fn this_device(&self) -> Option<Device> {
        Device::from_node(&self.local.as_ref()?.node)
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
        Device::BOTH.into_iter().find(|d| d.short_name() == node)
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
    } else {
        (Light::Connected, "Connected")
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
    AudioSend,
    AudioSource,
    MuteWhileSending,
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
            Control::AudioSend => flag(d.audio.tx_enabled),
            Control::AudioSource => d.audio.tx_source.unwrap_or(0) as f64,
            Control::MuteWhileSending => flag(d.audio.mute_while_sending.unwrap_or(false)),
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
            Control::AudioSend => json!({ "action": "set_audio_tx", "enabled": on }),
            Control::AudioSource => json!({ "action": "set_audio_tx_source", "mode": value as u32 }),
            Control::MuteWhileSending => json!({ "action": "set_mute_while_sending", "enabled": on }),
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
    held_until: Option<Instant>,
    unsaved: bool,
    saved_at: Option<Instant>,
}

impl LayoutDraft {
    pub fn follow(&mut self, polled: Option<LayoutState>, dragging: bool, now: Instant) {
        let held = dragging || self.unsaved || self.held_until.is_some_and(|t| now < t);
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

    /// The layout to save now, if an edit is waiting and the last save was long enough ago.
    pub fn take_due(&mut self, now: Instant) -> Option<&LayoutState> {
        if !self.unsaved || self.saved_at.is_some_and(|at| now < at + SAVE_GAP) {
            return None;
        }
        self.unsaved = false;
        self.saved_at = Some(now);
        self.hold(now);
        self.state.as_ref()
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

/// The named places the laptop can go, one set per Debian monitor.
pub fn placements(cfg: &LayoutConfig) -> Vec<Placement> {
    let (.., w, h) = cfg.laptop_box();
    let desktop = cfg.rects(Device::Debian);
    let (left, _, right, _) = bounds(&desktop);
    let mut out = Vec::new();
    for m in &desktop {
        let label = m.name.split(' ').next().unwrap_or("");
        let cx = js_round(m.x as f64 + (m.w - w) as f64 / 2.0);
        let cy = js_round(m.y as f64 + (m.h - h) as f64 / 2.0);
        let mut add = |prefix: &str, x: i32, y: i32, side: &str| {
            out.push(Placement { label: format!("{prefix} {label}"), x, y, side: side.into() });
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

/// The closest edge placement to a dragged laptop position.
pub fn snap(cfg: &LayoutConfig, x: i32, y: i32) -> Option<Placement> {
    let (.., w, h) = cfg.laptop_box();
    let desktop = cfg.rects(Device::Debian);
    let (left, _, right, _) = bounds(&desktop);
    let mut best: Option<(f64, Placement)> = None;
    let mut consider = |px: i32, py: i32, side: &str| {
        let d = ((x - px) as f64).hypot((y - py) as f64);
        if best.as_ref().is_none_or(|(b, _)| d < *b) {
            best = Some((d, Placement { label: "Custom".into(), x: px, y: py, side: side.into() }));
        }
    };
    // `max` then `min`, as the page did, so an inverted range doesn't panic.
    let clamp = |v: i32, lo: i32, hi: i32| v.min(hi).max(lo);
    for m in &desktop {
        let cy = clamp(y, m.y - h + 240, m.y + m.h - 240);
        let cx = clamp(x, m.x - w + 280, m.x + m.w - 280);
        if m.x + m.w >= right {
            consider(m.x + m.w, cy, "Right");
        }
        if m.x <= left {
            consider(m.x - w, cy, "Left");
        }
        consider(cx, m.y + m.h, "Bottom");
        consider(cx, m.y - h, "Top");
    }
    best.map(|(_, p)| p)
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

/// Sets the laptop's size, or its native size for `None`, and keeps it on an edge.
pub fn set_resolution(cfg: &mut LayoutConfig, size: Option<(i32, i32)>) {
    let (x, y, ..) = cfg.laptop_box();
    if let Some(laptop) = cfg.device_mut(Device::Windows) {
        laptop.custom_size = size;
    }
    if let Some(placement) = snap(cfg, x, y) {
        apply_placement(cfg, &placement);
    }
}

pub fn size_label((w, h): (i32, i32)) -> String {
    format!("{w} \u{d7} {h}")
}

pub fn resolution_label(cfg: &LayoutConfig) -> String {
    match cfg.device(Device::Windows).and_then(|d| d.custom_size) {
        Some(size) => size_label(size),
        None => "Native".into(),
    }
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
    let other = if local.node == "debian" { "laptop" } else { "desktop" };
    if snapshot.peer.is_none() {
        return Some(format!(
            "The {other} is out of reach, so each device is working on its own. Check that mesh-kvm is running on it, and that its firewall allows TCP port 24880."
        ));
    }
    let older = |d: &DeviceStatus| d.is_older() || d.has_no_shared_layout();
    if let Some(device) = Device::BOTH.into_iter().find(|d| snapshot.device(*d).is_some_and(older)) {
        return Some(older_message(device));
    }
    if !local.peer_online {
        return Some(format!(
            "The {other} is found but not linked yet, so each device is working on its own. Wait a few seconds, or restart mesh-kvm on both devices."
        ));
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
    if local.audio.tx_enabled && snapshot.peer.as_ref().is_some_and(|peer| peer.audio.tx_enabled) {
        return Some(
            "Both devices are sending audio, which causes an echo. Turn off Send on one of them.".into(),
        );
    }
    None
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
        let ps = placements(&cfg());
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
        let s = snap(&cfg(), 2240, 1400).unwrap();
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
        let snap = Snapshot { local: Some(local), peer: None, layout: None };
        let msg = status_message(&snap).unwrap();
        assert!(msg.contains("TCP port 24880"), "{msg}");
        assert!(msg.contains("laptop"), "{msg}");
    }

    #[test]
    fn main_conflict_message_says_how_to_fix_it() {
        let mut local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        local.main_conflict = true;
        let snap = Snapshot { local: Some(local.clone()), peer: Some(local), layout: None };
        let msg = status_message(&snap).unwrap();
        assert!(msg.contains("Both devices"), "{msg}");
        assert!(msg.contains("own keyboard and mouse"), "{msg}");
        assert!(msg.contains("Main device"), "{msg}");
    }

    #[test]
    fn healthy_link_has_no_message() {
        let local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        let snap = Snapshot { local: Some(local.clone()), peer: Some(local), layout: None };
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
    fn saving_keeps_fields_the_app_does_not_know() {
        let mut l = LayoutState::parse(STATE_JSON).unwrap();
        l.config.move_laptop(5, 1440);
        l.config.attach_side = "Right".into();
        let out = l.to_save_json();
        assert_eq!(out["devices"][1]["x"], 5);
        assert_eq!(out["attach_side"], "Right");
        assert_eq!(out["future_field"]["a"], 1);
        assert_eq!(out["devices"][1]["future_device_field"], 5);
        assert_eq!(out["devices"][0]["monitors"][1]["name"], "DP-2 (360Hz Native)");
        assert_eq!(out["revision"], 3);
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
        assert_eq!(msg, "The Windows laptop is running an older mesh-kvm. Update it, or sign out and back in.");
        // Its own controls still work, as it does have those actions.
        assert!(!snap.peer.as_ref().unwrap().is_older());
    }

    #[test]
    fn with_no_address_the_app_says_so_and_nothing_else_matters_yet() {
        let mut local = device("windows", 0);
        local.paired = Some(false);
        let snap = Snapshot { local: Some(local), peer: None, layout: LayoutState::parse(STATE_JSON) };
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
        let p = snap(c, x, y).unwrap();
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
        // Above DP-3, which starts lower than DP-2: 360 - 1600.
        assert_eq!(place_of(&cfg(), -500, -1300), (-500, -1240, "Top".into()));
    }

    #[test]
    fn placements_round_half_widths_as_javascript_does() {
        let mut c = cfg();
        let at = |c: &LayoutConfig, label: &str| {
            let ps = placements(c);
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
    fn resolution_change_keeps_the_laptop_on_an_edge() {
        let mut c = cfg();
        set_resolution(&mut c, Some((1920, 1080)));
        assert_eq!(c.laptop_box(), (2240, 1440, 1920, 1080));
        assert_eq!(c.attach_side, "Bottom");
        assert_eq!(resolution_label(&c), "1920 \u{d7} 1080");
        set_resolution(&mut c, None);
        assert_eq!((c.laptop_box().2, c.laptop_box().3), (2560, 1600));
        assert_eq!(resolution_label(&c), "Native");
    }

    #[test]
    fn position_label_names_a_place_or_the_edge() {
        let mut c = cfg();
        c.move_laptop(1920, 1440);
        assert_eq!(position_label(&c, &placements(&c)), "Below DP-2");
        c.move_laptop(2240, 1440);
        assert_eq!(position_label(&c, &placements(&c)), "Custom (bottom edge)");
    }

    #[test]
    fn an_older_service_is_named_and_the_controls_that_need_it_are_off() {
        let mut snap = pair(1, 1);
        snap.peer.as_mut().unwrap().sharing = None;
        let msg = status_message(&snap).unwrap();
        assert_eq!(msg, "The Windows laptop is running an older mesh-kvm. Update it, or sign out and back in.");
        assert!(!enabled(&snap).main_device);
        assert!(enabled(&snap).test_sound);
        let mut snap = pair(1, 1);
        snap.local.as_mut().unwrap().sharing = None;
        assert!(status_message(&snap).unwrap().starts_with("The Debian desktop is running an older mesh-kvm."));
        assert!(!enabled(&snap).main_device);
        // A current pair is unaffected.
        assert!(enabled(&pair(1, 1)).main_device);
    }

    #[test]
    fn the_older_text_names_the_device() {
        assert_eq!(older_message(Device::Windows), "The Windows laptop is running an older mesh-kvm. Update it, or sign out and back in.");
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
        d.main_device = main_device;
        d
    }

    fn pair(debian_main: u8, windows_main: u8) -> Snapshot {
        Snapshot {
            local: Some(device("debian", debian_main)),
            peer: Some(device("windows", windows_main)),
            layout: Some(LayoutState::parse(STATE_JSON).unwrap()),
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
                peer: None,
                layout: LayoutState::parse(STATE_JSON),
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
        let alone = Snapshot { local: Some(device("debian", 0)), peer: None, layout: None };
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
    fn an_unlinked_peer_never_claims_the_keyboard() {
        let mut snap = pair(0, 0);
        let layout = snap.layout.as_mut().unwrap();
        layout.telemetry.peer_online = false;
        layout.telemetry.active_node = "Windows".into();
        assert_eq!(device_badge(&snap, Device::Windows).0, Light::Connected);
    }

    #[test]
    fn release_reaches_both_devices_when_this_service_can_do_that() {
        assert_eq!(release_route(&pair(0, 0)), Some(Release::Everywhere));
        let mut laptop_here = pair(0, 0);
        laptop_here.local.as_mut().unwrap().node = "windows".into();
        assert_eq!(release_route(&laptop_here), Some(Release::Everywhere));
        let alone = Snapshot { local: Some(device("windows", 0)), peer: None, layout: None };
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
    fn both_devices_sending_is_called_out() {
        let linked = || {
            let local: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
            let peer = DeviceStatus { node: "windows".into(), ..local.clone() };
            Snapshot { local: Some(local), peer: Some(peer), layout: LayoutState::parse(STATE_JSON) }
        };
        let mut snap = linked();
        snap.local.as_mut().unwrap().audio.tx_enabled = true;
        assert_eq!(status_message(&snap), None, "one device sending is the normal case");
        snap.peer.as_mut().unwrap().audio.tx_enabled = true;
        assert_eq!(
            status_message(&snap).as_deref(),
            Some("Both devices are sending audio, which causes an echo. Turn off Send on one of them.")
        );
    }

    #[test]
    fn mute_while_sending_reads_and_writes_the_service_field() {
        let mut d: DeviceStatus = serde_json::from_str(LOCAL_JSON).unwrap();
        assert_eq!(d.audio.mute_while_sending, None, "an older service has no such field");
        assert_eq!(Control::MuteWhileSending.read(&d), 0.0);
        d.audio.mute_while_sending = Some(true);
        assert_eq!(Control::MuteWhileSending.read(&d), 1.0);
        assert_eq!(
            Control::MuteWhileSending.action(0.0),
            json!({"action": "set_mute_while_sending", "enabled": false})
        );
        let newer = LOCAL_JSON.replace(r#""tx_source":0,"#, r#""tx_source":0,"mute_while_sending":false,"#);
        let d: DeviceStatus = serde_json::from_str(&newer).unwrap();
        assert_eq!(d.audio.mute_while_sending, Some(false));
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
        assert_eq!(draft.take_due(t).unwrap().config.laptop_box().0, 5);
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
        draft.edited(t);
        assert!(draft.take_due(t).is_some());
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
