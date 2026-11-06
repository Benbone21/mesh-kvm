//! What the app knows about the two devices, and the layout maths. No egui types in here.

use serde::Deserialize;
use serde_json::Value;

fn yes() -> bool {
    true
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
        out["mouse_speed"] = c.mouse_speed.into();
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
    let panic = snapshot.layout.as_ref().map_or(local.panic_mode, |l| l.telemetry.panic_mode);
    if panic {
        return Some(
            "All input is released, and each device is using its own keyboard and mouse. Use the switch to reconnect."
                .into(),
        );
    }
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
    let conflict = local.main_conflict || snapshot.peer.as_ref().is_some_and(|p| p.main_conflict);
    if conflict {
        return Some(
            "Both devices think they are the main device, so each one is using its own keyboard and mouse. Choose Main device to pick one."
                .into(),
        );
    }
    None
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
}
