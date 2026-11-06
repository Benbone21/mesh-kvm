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
}

impl Default for ClipboardSharing {
    fn default() -> Self {
        Self { send: true, receive: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioStatus {
    pub tx_enabled: bool,
    pub tx_volume: u32,
    pub tx_peak: u32,
    pub tx_source: Option<u32>,
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
    pub sharing: Sharing,
    pub clipboard: ClipboardSharing,
    #[serde(default = "default_speed")]
    pub pointer_speed: f32,
    /// 0 is Debian, 1 is the laptop.
    pub main_device: u8,
    pub main_conflict: bool,
}

fn default_speed() -> f32 {
    1.0
}

/// The layout, as far as the app edits or draws it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    pub debian_monitors: Vec<Display>,
    pub deb_total_w: i32,
    pub deb_total_h: i32,
    pub win_x: i32,
    pub win_y: i32,
    pub win_w: i32,
    pub win_h: i32,
    pub win_native_w: i32,
    pub win_native_h: i32,
    pub resolution_mode: String,
    /// "Right", "Left", "Bottom" or "Top".
    pub attach_side: String,
    pub mouse_speed: f32,
    pub edge_resistance: i32,
    #[serde(default = "yes")]
    pub auto_game_lock: bool,
    pub manual_game_lock: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Telemetry {
    pub active_node: String,
    pub panic_mode: bool,
    pub deb_cursor_x: i32,
    pub deb_cursor_y: i32,
    pub win_cursor_x: i32,
    pub win_cursor_y: i32,
    pub peer_addr: String,
    pub game_captive_active: bool,
    pub game_reason: String,
    pub peer_online: bool,
    pub main_conflict: bool,
}

/// `/api/state`: the layout and live telemetry from the Debian desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutState {
    pub config: LayoutConfig,
    pub telemetry: Telemetry,
    /// The config exactly as the service sent it, so saving keeps fields this app doesn't know.
    raw_config: Value,
}

impl LayoutState {
    pub fn parse(text: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct State {
            config: Value,
            #[serde(default)]
            telemetry: Telemetry,
        }
        let state: State = serde_json::from_str(text).ok()?;
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
        out["win_x"] = c.win_x.into();
        out["win_y"] = c.win_y.into();
        out["win_w"] = c.win_w.into();
        out["win_h"] = c.win_h.into();
        out["resolution_mode"] = c.resolution_mode.clone().into();
        out["attach_side"] = c.attach_side.clone().into();
        // The service sent a double and the config holds an f32, so writing it back unchanged
        // would save float noise.
        if out["mouse_speed"].as_f64().unwrap_or(0.0) as f32 != c.mouse_speed {
            out["mouse_speed"] = ((c.mouse_speed as f64 * 100.0).round() / 100.0).into();
        }
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

    fn this_device(&self) -> Option<Device> {
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
        main_device: linked,
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

/// True when the notice is about this device's service rather than the link.
pub fn notice_is_serious(snapshot: &Snapshot) -> bool {
    snapshot.local.is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Through the Debian desktop, which owns the layout-wide actions.
    Everywhere,
    /// The Debian desktop can't be reached, so only this device lets go.
    Only(Device),
}

/// Where Release all input has to go.
pub fn release_route(snapshot: &Snapshot) -> Option<Release> {
    if snapshot.layout.is_some() {
        return Some(Release::Everywhere);
    }
    snapshot.this_device().map(Release::Only)
}

/// Where the cursor is in layout coordinates. The laptop reports its own position.
pub fn cursor_position(snapshot: &Snapshot, cfg: &LayoutConfig) -> Option<(i32, i32)> {
    let telemetry = &snapshot.layout.as_ref()?.telemetry;
    if telemetry.active_node == Device::Windows.short_name() {
        let [x, y] = snapshot.device(Device::Windows)?.cursor?;
        Some((cfg.win_x + x, cfg.win_y + y))
    } else {
        Some((telemetry.deb_cursor_x, telemetry.deb_cursor_y))
    }
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
            Control::MouseSharing => flag(d.sharing.mouse),
            Control::KeyboardSharing => flag(d.sharing.keyboard),
            Control::PointerSpeed => d.pointer_speed as f64,
            Control::AudioSend => flag(d.audio.tx_enabled),
            Control::AudioSource => d.audio.tx_source.unwrap_or(0) as f64,
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

    /// The body for a layout save. The pointer speed isn't part of what a layout edit changes, so
    /// it comes from the latest poll, or from the slider while a change to it hasn't caught up.
    /// A draft made before the person moved the slider would otherwise put the old speed back.
    pub fn layout_save_body(&self, draft: &LayoutState, latest: &Snapshot, now: Instant) -> Value {
        let polled = latest.layout.as_ref().unwrap_or(draft).config.mouse_speed;
        let speed = self.shown((Device::Debian, Control::PointerSpeed), polled as f64, now) as f32;
        let mut layout = draft.clone();
        layout.config.mouse_speed = speed;
        layout.to_save_json()
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
    let (w, h) = (cfg.win_w, cfg.win_h);
    let mut out = Vec::new();
    for m in &cfg.debian_monitors {
        let label = m.name.split(' ').next().unwrap_or("");
        let cx = js_round(m.x as f64 + (m.w - w) as f64 / 2.0);
        let cy = js_round(m.y as f64 + (m.h - h) as f64 / 2.0);
        let mut add = |prefix: &str, x: i32, y: i32, side: &str| {
            out.push(Placement { label: format!("{prefix} {label}"), x, y, side: side.into() });
        };
        add("Below", cx, m.y + m.h, "Bottom");
        add("Above", cx, m.y - h, "Top");
        if m.x <= 0 {
            add("Left of", m.x - w, cy, "Left");
        }
        if m.x + m.w >= cfg.deb_total_w {
            add("Right of", m.x + m.w, cy, "Right");
        }
    }
    out
}

/// The closest edge placement to a dragged laptop position.
pub fn snap(cfg: &LayoutConfig, x: i32, y: i32) -> Option<Placement> {
    let (w, h) = (cfg.win_w, cfg.win_h);
    let mut best: Option<(f64, Placement)> = None;
    let mut consider = |px: i32, py: i32, side: &str| {
        let d = ((x - px) as f64).hypot((y - py) as f64);
        if best.as_ref().is_none_or(|(b, _)| d < *b) {
            best = Some((d, Placement { label: "Custom".into(), x: px, y: py, side: side.into() }));
        }
    };
    // `max` then `min`, as the page did, so an inverted range doesn't panic.
    let clamp = |v: i32, lo: i32, hi: i32| v.min(hi).max(lo);
    for m in &cfg.debian_monitors {
        let cy = clamp(y, m.y - h + 240, m.y + m.h - 240);
        let cx = clamp(x, m.x - w + 280, m.x + m.w - 280);
        if m.x + m.w >= cfg.deb_total_w {
            consider(m.x + m.w, cy, "Right");
        }
        if m.x <= 0 {
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
    cfg.win_x = placement.x;
    cfg.win_y = placement.y;
    cfg.attach_side = placement.side.clone();
}

/// Sets the laptop's size, or its native size for `None`, and keeps it on an edge.
pub fn set_resolution(cfg: &mut LayoutConfig, size: Option<(i32, i32)>) {
    let native = (cfg.win_native_w, cfg.win_native_h);
    cfg.resolution_mode = if size.is_some() { "custom" } else { "native" }.into();
    (cfg.win_w, cfg.win_h) = size.unwrap_or(native);
    if let Some(placement) = snap(cfg, cfg.win_x, cfg.win_y) {
        apply_placement(cfg, &placement);
    }
}

pub fn size_label((w, h): (i32, i32)) -> String {
    format!("{w} \u{d7} {h}")
}

pub fn resolution_label(cfg: &LayoutConfig) -> String {
    if cfg.resolution_mode == "native" {
        "Native".into()
    } else {
        size_label((cfg.win_w, cfg.win_h))
    }
}

/// The name of where the laptop is now: a named place, or the edge it was dragged along.
pub fn position_label(cfg: &LayoutConfig, placements: &[Placement]) -> String {
    match placements.iter().find(|p| p.x == cfg.win_x && p.y == cfg.win_y) {
        Some(p) => p.label.clone(),
        None => format!("Custom ({} edge)", cfg.attach_side.to_lowercase()),
    }
}

/// Scale and offset that fit every screen, with a margin, into a map of this size.
pub fn fit(cfg: &LayoutConfig, width: f32, height: f32) -> MapView {
    const PAD: f32 = 16.0;
    let (mut x0, mut y0) = (cfg.win_x, cfg.win_y);
    let (mut x1, mut y1) = (cfg.win_x + cfg.win_w, cfg.win_y + cfg.win_h);
    for m in &cfg.debian_monitors {
        x0 = x0.min(m.x);
        y0 = y0.min(m.y);
        x1 = x1.max(m.x + m.w);
        y1 = y1.max(m.y + m.h);
    }
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
    let other = if local.node == "debian" { "laptop" } else { "desktop" };
    if snapshot.peer.is_none() {
        return Some(format!(
            "The {other} is out of reach, so each device is working on its own. Check that mesh-kvm is running on it, and that its firewall allows TCP port 24880."
        ));
    }
    if !local.peer_online {
        return Some(format!(
            "The {other} is found but not linked yet, so each device is working on its own. Wait a few seconds, or restart mesh-kvm on both devices."
        ));
    }
    if let Some(message) = snapshot.peer.as_ref().and_then(|peer| split_message(local, peer)) {
        return Some(message);
    }
    let panic = snapshot.layout.as_ref().map_or(local.panic_mode, |l| l.telemetry.panic_mode);
    if panic {
        return Some(
            "All input is released, and each device is using its own keyboard and mouse. Use the switch to reconnect."
                .into(),
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

    const LOCAL_JSON: &str = r#"{"node":"debian","label":"Debian Desktop","hostname":"bread","ip":"192.168.1.10","peer_ip":"192.168.1.20","version":"0.1.0","os":"Debian 13","peer_online":true,"active_node":"Debian","panic_mode":false,"game_lock":false,"game_reason":"","displays":[{"name":"DP-3","x":0,"y":360,"w":1920,"h":1080}],"cursor":[100,200],"autostart":true,"input_devices":["Physical mouse (evdev)"],"audio":{"tx_enabled":false,"tx_volume":100,"tx_peak":0,"tx_packets":0,"tx_source":0,"rx_muted":false,"rx_volume":100,"rx_peak":0,"rx_packets":0},"sharing":{"mouse":true,"keyboard":false},"clipboard":{"send":true,"receive":false},"pointer_speed":0.35,"main_device":1,"main_conflict":false}"#;

    const STATE_JSON: &str = r#"{"config":{"debian_monitors":[{"name":"DP-3 (240Hz Native)","x":0,"y":360,"w":1920,"h":1080},{"name":"DP-2 (360Hz Native)","x":1920,"y":0,"w":2560,"h":1440}],"deb_total_w":4480,"deb_total_h":1440,"win_x":2240,"win_y":1440,"win_w":2560,"win_h":1600,"win_native_w":2560,"win_native_h":1600,"resolution_mode":"native","attach_side":"Bottom","mouse_speed":0.35,"edge_resistance":55,"auto_game_lock":true,"manual_game_lock":false,"future_field":{"a":1}},"telemetry":{"active_node":"Debian","panic_mode":false,"deb_cursor_x":3200,"deb_cursor_y":720,"win_cursor_x":960,"win_cursor_y":540,"peer_addr":"192.168.1.20:24800","game_captive_active":false,"game_reason":"","audio_muted":false,"peer_online":true,"main_conflict":false}}"#;

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
            c.debian_monitors.iter().map(|m| (m.x, m.y, m.w, m.h)).collect();
        rects.push((c.win_x, c.win_y, c.win_w, c.win_h));
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
        moved.config.win_x = 0;
        let kept = merge_poll(Some(current.clone()), Some(moved.clone()), true).unwrap();
        assert_eq!(kept.config.win_x, 2240);
        let taken = merge_poll(Some(current), Some(moved), false).unwrap();
        assert_eq!(taken.config.win_x, 0);
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
        assert!(d.sharing.mouse && !d.sharing.keyboard);
        assert!(d.clipboard.send && !d.clipboard.receive);
        assert_eq!(d.main_device, 1);
        assert_eq!(d.cursor, Some([100, 200]));
        // An older service that lacks the newer fields still parses.
        let old: DeviceStatus = serde_json::from_str(r#"{"node":"windows"}"#).unwrap();
        assert!(old.sharing.mouse && old.sharing.keyboard && old.clipboard.send);
        let l = LayoutState::parse(STATE_JSON).unwrap();
        assert_eq!(l.config.debian_monitors.len(), 2);
        assert_eq!(l.config.attach_side, "Bottom");
        assert_eq!(l.telemetry.deb_cursor_x, 3200);
    }

    #[test]
    fn saving_keeps_fields_the_app_does_not_know() {
        let mut l = LayoutState::parse(STATE_JSON).unwrap();
        l.config.win_x = 5;
        l.config.attach_side = "Right".into();
        let out = l.to_save_json();
        assert_eq!(out["win_x"], 5);
        assert_eq!(out["attach_side"], "Right");
        assert_eq!(out["future_field"]["a"], 1);
        assert_eq!(out["debian_monitors"][1]["name"], "DP-2 (360Hz Native)");
        assert_eq!(out["win_native_w"], 2560);
    }

    #[test]
    fn saving_leaves_an_unchanged_mouse_speed_as_the_service_sent_it() {
        let text = STATE_JSON.replace("\"mouse_speed\":0.35", "\"mouse_speed\":0.1");
        let saved = LayoutState::parse(&text).unwrap().to_save_json();
        assert_eq!(saved["mouse_speed"].to_string(), "0.1");
    }

    #[test]
    fn a_layout_save_keeps_the_pointer_speed_just_set() {
        let draft = LayoutState::parse(STATE_JSON).unwrap();
        let latest = Snapshot { layout: Some(draft.clone()), ..Default::default() };
        let now = Instant::now();
        let mut edits = Edits::default();
        let key = (Device::Debian, Control::PointerSpeed);
        edits.set(key, 0.8, now);
        assert_eq!(edits.layout_save_body(&draft, &latest, now)["mouse_speed"].to_string(), "0.8");
        // Once the poll has caught up, the save follows the poll rather than the stale draft.
        let mut newer = draft.clone();
        newer.config.mouse_speed = 0.5;
        let latest = Snapshot { layout: Some(newer), ..Default::default() };
        let later = now + Duration::from_secs(60);
        edits.take_due(later);
        assert_eq!(edits.layout_save_body(&draft, &latest, later)["mouse_speed"].to_string(), "0.5");
    }

    #[test]
    fn saving_a_changed_mouse_speed_rounds_it() {
        let mut l = LayoutState::parse(STATE_JSON).unwrap();
        l.config.mouse_speed = 0.7;
        assert_eq!(l.to_save_json()["mouse_speed"].to_string(), "0.7");
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
        (c.win_w, c.win_h) = (1921, 1081);
        // 1920 + 639 / 2 = 2239.5 rounds up.
        assert_eq!(at(&c, "Below DP-2"), (2240, 1440));
        // 0 + -1 / 2 = -0.5 rounds up to 0.
        assert_eq!(at(&c, "Below DP-3"), (0, 1440));
        // 360 + -1 / 2 = 359.5, and 0 + 359 / 2 = 179.5.
        assert_eq!(at(&c, "Left of DP-3"), (-1921, 360));
        assert_eq!(at(&c, "Right of DP-2"), (4480, 180));
        // -1.5 rounds up to -1, not away from zero.
        c.win_w = 1923;
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
        assert_eq!((c.win_w, c.win_h, c.resolution_mode.as_str()), (1920, 1080, "custom"));
        assert_eq!((c.win_x, c.win_y, c.attach_side.as_str()), (2240, 1440, "Bottom"));
        assert_eq!(resolution_label(&c), "1920 \u{d7} 1080");
        set_resolution(&mut c, None);
        assert_eq!((c.win_w, c.win_h), (2560, 1600));
        assert_eq!(resolution_label(&c), "Native");
    }

    #[test]
    fn position_label_names_a_place_or_the_edge() {
        let mut c = cfg();
        c.win_x = 1920;
        assert_eq!(position_label(&c, &placements(&c)), "Below DP-2");
        c.win_x = 2240;
        assert_eq!(position_label(&c, &placements(&c)), "Custom (bottom edge)");
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
        // On the laptop, with the Debian desktop gone there is no layout either.
        let snap = Snapshot { local: Some(device("windows", 0)), peer: None, layout: None };
        let on = enabled(&snap);
        assert_eq!(
            on,
            ControlsEnabled { switch: false, main_device: false, layout: false, release: true, test_sound: false }
        );
        // On the desktop, the layout is still there to edit.
        let mut snap = pair(0, 0);
        snap.peer = None;
        let on = enabled(&snap);
        assert!(on.layout && on.release);
        assert!(!on.switch && !on.main_device && !on.test_sound);
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
    fn release_goes_through_debian_when_it_can() {
        assert_eq!(release_route(&pair(0, 0)), Some(Release::Everywhere));
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
        // The laptop's own cursor (100, 200), from its corner at (2240, 1440).
        assert_eq!(cursor_position(&snap, &c), Some((2340, 1640)));
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

    fn layout_at(win_x: i32) -> Option<LayoutState> {
        let mut l = LayoutState::parse(STATE_JSON).unwrap();
        l.config.win_x = win_x;
        Some(l)
    }

    fn drafted_x(draft: &LayoutDraft) -> i32 {
        draft.state.as_ref().unwrap().config.win_x
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
        draft.state.as_mut().unwrap().config.win_x = 5;
        draft.edited(t);
        assert_eq!(draft.take_due(t).unwrap().config.win_x, 5);
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
