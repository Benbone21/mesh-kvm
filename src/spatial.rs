use crate::protocol::SwitchEdge;
use crate::router::{EdgeSpan, KEEP_CURSOR};
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;
use std::sync::{Arc, RwLock};

/// How far in from a screen edge a cursor enters, on both machines.
const ENTRY_INSET: i32 = 28;
const LAYOUT_FILE: &str = "/home/bread/mesh-kvm/layout.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchSide {
    /// Windows is to the RIGHT of Debian (crosses Debian's Right edge <-> Windows' Left edge)
    Right,
    /// Windows is to the LEFT of Debian (crosses Debian's Left edge <-> Windows' Right edge)
    Left,
    /// Windows is BELOW Debian (crosses Debian's Bottom edge <-> Windows' Top edge)
    Bottom,
    /// Windows is ABOVE Debian (crosses Debian's Top edge <-> Windows' Bottom edge)
    Top,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorRect {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

fn default_true() -> bool {
    true
}

fn default_native_mode() -> String {
    "native".to_string()
}

fn default_win_w() -> i32 {
    1920
}

fn default_win_h() -> i32 {
    1080
}

fn default_mouse_speed() -> f32 {
    0.35
}

/// Auto-detects all connected Debian displays and their native hardware resolutions & positions via `xrandr --current`.
pub fn detect_debian_monitors_native() -> (Vec<MonitorRect>, i32, i32) {
    let mut monitors = Vec::new();
    if let Ok(output) = Command::new("xrandr").arg("--current").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        let mut current_mon: Option<MonitorRect> = None;

        for line in text.lines() {
            if !line.starts_with(' ') && !line.starts_with('\t') && line.contains(" connected") {
                if let Some(m) = current_mon.take() {
                    monitors.push(m);
                }
                let parts: Vec<&str> = line.split_whitespace().collect();
                let out_name = parts.first().copied().unwrap_or("Display").to_string();
                let is_primary = line.contains(" primary ");
                // Find token matching WxH+X+Y
                for tok in &parts {
                    if tok.contains('x') && tok.contains('+') {
                        let nums: Vec<i32> = tok
                            .split(|c: char| !c.is_ascii_digit() && c != '-')
                            .filter(|s| !s.is_empty())
                            .filter_map(|s| s.parse::<i32>().ok())
                            .collect();
                        if nums.len() >= 4 {
                            let label = if is_primary {
                                format!("{} (Primary Native)", out_name)
                            } else {
                                format!("{} (Native)", out_name)
                            };
                            current_mon = Some(MonitorRect {
                                name: label,
                                w: nums[0],
                                h: nums[1],
                                x: nums[2],
                                y: nums[3],
                            });
                            break;
                        }
                    }
                }
            } else if line.contains('*') {
                // Active refresh rate line, e.g. "   2560x1440    359.80*+"
                if let Some(ref mut m) = current_mon {
                    for tok in line.split_whitespace() {
                        if tok.contains('*') {
                            let clean: String = tok
                                .chars()
                                .take_while(|c| c.is_ascii_digit() || *c == '.')
                                .collect();
                            if let Ok(hz) = clean.parse::<f32>() {
                                let base_name = m
                                    .name
                                    .split(' ')
                                    .next()
                                    .unwrap_or(&m.name)
                                    .to_string();
                                m.name = format!("{} ({:.0}Hz Native)", base_name, hz.round());
                            }
                            break;
                        }
                    }
                }
            }
        }
        if let Some(m) = current_mon.take() {
            monitors.push(m);
        }
    }

    monitors.sort_by_key(|m| (m.x, m.y));

    if monitors.is_empty() {
        monitors = vec![
            MonitorRect {
                name: "DP-3 (240Hz Native)".to_string(),
                x: 0,
                y: 360,
                w: 1920,
                h: 1080,
            },
            MonitorRect {
                name: "DP-2 (360Hz Native)".to_string(),
                x: 1920,
                y: 0,
                w: 2560,
                h: 1440,
            },
        ];
    }

    let total_w = monitors.iter().map(|m| m.x + m.w).max().unwrap_or(4480);
    let total_h = monitors.iter().map(|m| m.y + m.h).max().unwrap_or(1440);
    (monitors, total_w, total_h)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialConfig {
    pub debian_monitors: Vec<MonitorRect>,
    pub deb_total_w: i32,
    pub deb_total_h: i32,
    /// Position of the Windows laptop in the same 2D pixel space as Debian
    pub win_x: i32,
    pub win_y: i32,
    pub win_w: i32,
    pub win_h: i32,
    /// Native physical resolution reported by Windows (or hardware default)
    #[serde(default = "default_win_w")]
    pub win_native_w: i32,
    #[serde(default = "default_win_h")]
    pub win_native_h: i32,
    /// "native" (default: uses native resolution of all displays) or "custom"
    #[serde(default = "default_native_mode")]
    pub resolution_mode: String,
    /// Which side of Debian the Windows screen is attached to
    pub attach_side: TouchSide,
    /// Mouse speed multiplier when controlling Windows from Debian's high-DPI mouse
    #[serde(default = "default_mouse_speed")]
    pub mouse_speed: f32,
    /// How many raw mouse counts of pushing against the boundary are needed to cross
    pub edge_resistance: i32,
    /// Automatically keep mouse captive when a fullscreen or pointer-locked game is running
    #[serde(default = "default_true")]
    pub auto_game_lock: bool,
    /// Manually lock mouse & keyboard captive to the currently focused device (Ctrl+Alt+G)
    #[serde(default)]
    pub manual_game_lock: bool,
}

impl Default for SpatialConfig {
    fn default() -> Self {
        let (debian_monitors, deb_total_w, deb_total_h) = detect_debian_monitors_native();
        Self {
            debian_monitors,
            deb_total_w,
            deb_total_h,
            win_x: 2240,
            win_y: deb_total_h,
            win_w: 1920,
            win_h: 1080,
            win_native_w: 1920,
            win_native_h: 1080,
            resolution_mode: "native".to_string(),
            attach_side: TouchSide::Bottom,
            mouse_speed: 0.35,
            edge_resistance: 55,
            auto_game_lock: true,
            manual_game_lock: false,
        }
    }
}

impl SpatialConfig {
    /// Re-snaps `win_x` and `win_y` cleanly onto the outside edge of Debian after `win_w`/`win_h` changes.
    pub fn resnap_after_resize(&mut self) {
        match self.attach_side {
            TouchSide::Right => {
                self.win_x = self.deb_total_w;
                self.win_y = self.win_y.clamp(-self.win_h + 240, self.deb_total_h - 240);
            }
            TouchSide::Left => {
                self.win_x = -self.win_w;
                self.win_y = self.win_y.clamp(-self.win_h + 240, self.deb_total_h - 240);
            }
            TouchSide::Bottom => {
                // Find which Debian monitor it overlaps horizontally with
                let center_x = self.win_x + self.win_w / 2;
                let mut target_y = self.deb_total_h;
                for m in &self.debian_monitors {
                    if center_x >= m.x && center_x <= m.x + m.w {
                        target_y = m.y + m.h;
                        break;
                    }
                }
                self.win_y = target_y;
                self.win_x = self.win_x.clamp(-self.win_w + 280, self.deb_total_w - 280);
            }
            TouchSide::Top => {
                let center_x = self.win_x + self.win_w / 2;
                let mut target_y = -self.win_h;
                for m in &self.debian_monitors {
                    if center_x >= m.x && center_x <= m.x + m.w {
                        target_y = m.y - self.win_h;
                        break;
                    }
                }
                self.win_y = target_y;
                self.win_x = self.win_x.clamp(-self.win_w + 280, self.deb_total_w - 280);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveTelemetry {
    pub active_node: String,
    pub panic_mode: bool,
    pub deb_cursor_x: i32,
    pub deb_cursor_y: i32,
    pub win_cursor_x: i32,
    pub win_cursor_y: i32,
    pub peer_addr: String,
    pub game_captive_active: bool,
    pub game_reason: String,
    pub audio_rx_packets: u64,
    pub audio_rx_peak: u32,
    pub audio_muted: bool,
    pub audio_volume: u32,
    pub audio_tx_enabled: bool,
    pub audio_tx_packets: u64,
    pub audio_tx_peak: u32,
    pub audio_tx_volume: u32,
    pub audio_tx_source_mode: u32,
    pub win_peer_reported_res: bool,
    #[serde(default)]
    pub peer_online: bool,
    pub main_conflict: bool,
}

impl LiveTelemetry {
    fn new(peer_addr: String) -> Self {
        Self {
            active_node: "Debian".to_string(),
            panic_mode: false,
            deb_cursor_x: 3200,
            deb_cursor_y: 720,
            win_cursor_x: 960,
            win_cursor_y: 540,
            peer_addr,
            game_captive_active: false,
            game_reason: String::new(),
            audio_rx_packets: 0,
            audio_rx_peak: 0,
            audio_muted: false,
            audio_volume: 100,
            audio_tx_enabled: false,
            audio_tx_packets: 0,
            audio_tx_peak: 0,
            audio_tx_volume: 100,
            audio_tx_source_mode: 0,
            win_peer_reported_res: false,
            peer_online: false,
            main_conflict: false,
        }
    }
}

pub struct SharedSpatialState {
    pub config: SpatialConfig,
    pub telemetry: LiveTelemetry,
    pub pending_manual_switch: Option<String>,
    pub pending_panic: bool,
}

/// What the input loop picks up from the Studio.
pub struct StudioRequests {
    pub manual_switch: bool,
    pub panic: bool,
    pub config: SpatialConfig,
}

/// Takes pending Studio requests with a config snapshot, or `None` if the lock is busy. The
/// input loop must never wait here: it runs the link timeout and the failsafe hotkeys.
pub fn take_studio_requests(state: &RwLock<SharedSpatialState>) -> Option<StudioRequests> {
    let mut guard = state.try_write().ok()?;
    Some(StudioRequests {
        manual_switch: guard.pending_manual_switch.take().is_some(),
        panic: std::mem::take(&mut guard.pending_panic),
        config: guard.config.clone(),
    })
}

impl SharedSpatialState {
    pub fn load_or_default(peer_addr: String) -> Self {
        let mut config = fs::read_to_string(LAYOUT_FILE)
            .ok()
            .and_then(|s| serde_json::from_str::<SpatialConfig>(&s).ok())
            .unwrap_or_default();

        // Always refresh Debian monitors from live hardware native resolutions
        let (deb_mons, total_w, total_h) = detect_debian_monitors_native();
        config.debian_monitors = deb_mons;
        config.deb_total_w = total_w;
        config.deb_total_h = total_h;
        if config.resolution_mode.is_empty() {
            config.resolution_mode = "native".to_string();
        }
        if config.resolution_mode == "native" && config.win_native_w > 0 && config.win_native_h > 0
        {
            config.win_w = config.win_native_w;
            config.win_h = config.win_native_h;
        }
        config.resnap_after_resize();

        let state = Self {
            config,
            telemetry: LiveTelemetry::new(peer_addr),
            pending_manual_switch: None,
            pending_panic: false,
        };
        state.save();
        state
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            let _ = fs::write(LAYOUT_FILE, json);
        }
    }
}

impl SpatialConfig {
    /// Checks if the Debian cursor `(cx, cy)` with movement `(dx, dy)` is touching the shared boundary
    /// with the Windows screen, and if so returns `(entry_win_x, entry_win_y)` on Windows.
    pub fn check_debian_to_windows(&self, cx: i32, cy: i32, dx: i32, dy: i32) -> Option<(i32, i32)> {
        match self.attach_side {
            TouchSide::Right => {
                let boundary_x = self.win_x;
                if dx > 0
                    && cx >= boundary_x - 4
                    && cx <= boundary_x + 4
                    && cy >= self.win_y
                    && cy <= self.win_y + self.win_h
                {
                    let wy = (cy - self.win_y).clamp(12, self.win_h - 12);
                    return Some((28, wy));
                }
            }
            TouchSide::Left => {
                let boundary_x = self.win_x + self.win_w;
                if dx < 0
                    && cx >= boundary_x - 4
                    && cx <= boundary_x + 4
                    && cy >= self.win_y
                    && cy <= self.win_y + self.win_h
                {
                    let wy = (cy - self.win_y).clamp(12, self.win_h - 12);
                    return Some(((self.win_w - 28).max(12), wy));
                }
            }
            TouchSide::Bottom => {
                let boundary_y = self.win_y;
                if dy > 0
                    && cy >= boundary_y - 4
                    && cy <= boundary_y + 4
                    && cx >= self.win_x
                    && cx <= self.win_x + self.win_w
                {
                    let wx = (cx - self.win_x).clamp(12, self.win_w - 12);
                    return Some((wx, 28));
                }
            }
            TouchSide::Top => {
                let boundary_y = self.win_y + self.win_h;
                if dy < 0
                    && cy >= boundary_y - 4
                    && cy <= boundary_y + 4
                    && cx >= self.win_x
                    && cx <= self.win_x + self.win_w
                {
                    let wx = (cx - self.win_x).clamp(12, self.win_w - 12);
                    return Some((wx, (self.win_h - 28).max(12)));
                }
            }
        }
        None
    }

    /// Adds this movement to the push against the shared boundary in `pushed`, and returns the
    /// laptop-local entry point once the push reaches `edge_resistance`. Movement that is not
    /// against the boundary clears the push.
    pub fn push_debian_edge(
        &self,
        cx: i32,
        cy: i32,
        dx: i32,
        dy: i32,
        pushed: &mut i32,
    ) -> Option<(i32, i32)> {
        let Some(entry) = self.check_debian_to_windows(cx, cy, dx, dy) else {
            *pushed = 0;
            return None;
        };
        *pushed = pushed
            .saturating_add(dx.saturating_abs())
            .saturating_add(dy.saturating_abs());
        if *pushed >= self.edge_resistance {
            *pushed = 0;
            return Some(entry);
        }
        None
    }

    /// The part of the laptop's edge that touches Debian, in laptop-local pixels along that edge.
    ///
    /// Only the monitors closest to the laptop's edge count, so a shorter monitor beside a taller
    /// one is not treated as touching. With no monitor alongside the laptop the span is empty
    /// (`start > end`), which no push can cross.
    pub fn laptop_edge_span(&self) -> EdgeSpan {
        let (edge, boundary, origin, length) = match self.attach_side {
            TouchSide::Right => (SwitchEdge::Left, self.win_x, self.win_y, self.win_h),
            TouchSide::Left => (SwitchEdge::Right, self.win_x + self.win_w, self.win_y, self.win_h),
            TouchSide::Bottom => (SwitchEdge::Top, self.win_y, self.win_x, self.win_w),
            TouchSide::Top => (SwitchEdge::Bottom, self.win_y + self.win_h, self.win_x, self.win_w),
        };
        // (distance from the laptop's edge, overlap start, overlap end), laptop-local and inclusive.
        let overlaps: Vec<(i32, i32, i32)> = self
            .debian_monitors
            .iter()
            .filter_map(|m| {
                let (facing, m_start, m_length) = match self.attach_side {
                    TouchSide::Right => (m.x + m.w, m.y, m.h),
                    TouchSide::Left => (m.x, m.y, m.h),
                    TouchSide::Bottom => (m.y + m.h, m.x, m.w),
                    TouchSide::Top => (m.y, m.x, m.w),
                };
                let start = origin.max(m_start);
                let end = (origin + length).min(m_start + m_length);
                (start < end).then_some(((facing - boundary).abs(), start - origin, end - 1 - origin))
            })
            .collect();
        let Some(nearest) = overlaps.iter().map(|o| o.0).min() else {
            return EdgeSpan { edge, start: 0, end: -1 };
        };
        let touching = || overlaps.iter().filter(|o| o.0 == nearest);
        EdgeSpan {
            edge,
            start: touching().map(|o| o.1).min().unwrap_or(0),
            end: touching().map(|o| o.2).max().unwrap_or(-1),
        }
    }

    /// Converts a laptop-local point to Debian desktop coordinates on the nearest monitor.
    /// `KEEP_CURSOR` in either coordinate means no warp, so it is returned for both.
    pub fn laptop_to_debian(&self, x: i32, y: i32) -> (i32, i32) {
        if x == KEEP_CURSOR || y == KEEP_CURSOR {
            return (KEEP_CURSOR, KEEP_CURSOR);
        }
        let p = self.clamp_to_debian(x.saturating_add(self.win_x), y.saturating_add(self.win_y));
        self.inset_on_monitor(p)
    }

    /// Pulls a point `ENTRY_INSET` pixels in from any monitor edge it sits on, so entry does not
    /// land on the boundary pixel. Stays inside the monitor.
    fn inset_on_monitor(&self, p: (i32, i32)) -> (i32, i32) {
        let inset = |v: i32, lo: i32, size: i32| {
            let hi = lo + (size - 1).max(0);
            let (lo_i, hi_i) = (lo + ENTRY_INSET, hi - ENTRY_INSET);
            if lo_i > hi_i {
                // Too small to inset on both sides: use the middle.
                return lo + (hi - lo) / 2;
            }
            if v == lo {
                lo_i
            } else if v == hi {
                hi_i
            } else {
                v
            }
        };
        let m = self.debian_monitors.iter().find(|m| {
            m.w > 0 && m.h > 0 && (m.x..m.x + m.w).contains(&p.0) && (m.y..m.y + m.h).contains(&p.1)
        });
        match m {
            Some(m) => (inset(p.0, m.x, m.w), inset(p.1, m.y, m.h)),
            None if self.debian_monitors.is_empty() => {
                (inset(p.0, 0, self.deb_total_w), inset(p.1, 0, self.deb_total_h))
            }
            None => p,
        }
    }

    /// The closest pixel to `(x, y)` on any Debian monitor.
    fn clamp_to_debian(&self, x: i32, y: i32) -> (i32, i32) {
        let on_rect = |left: i32, top: i32, w: i32, h: i32| {
            (x.clamp(left, left + (w - 1).max(0)), y.clamp(top, top + (h - 1).max(0)))
        };
        let distance = |&(px, py): &(i32, i32)| {
            let (dx, dy) = (i128::from(px) - i128::from(x), i128::from(py) - i128::from(y));
            dx * dx + dy * dy
        };
        self.debian_monitors
            .iter()
            .map(|m| on_rect(m.x, m.y, m.w, m.h))
            .min_by_key(distance)
            .unwrap_or_else(|| on_rect(0, 0, self.deb_total_w, self.deb_total_h))
    }
}


// ============================================================================================
// Studio API (Debian = spatial layout master)
// ============================================================================================

fn debian_autostart_enabled() -> bool {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/bread".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/default.target.wants/mesh-kvm.service")
        .exists()
}

fn set_debian_autostart(enabled: bool) {
    let verb = if enabled { "enable" } else { "disable" };
    let _ = Command::new("systemctl")
        .args(["--user", verb, "mesh-kvm.service"])
        .output();
}

/// Builds the Debian node's Studio API handler (plugged into the shared `webui` server).
pub fn studio_handler(state: Arc<RwLock<SharedSpatialState>>) -> crate::webui::Handler {
    Arc::new(move |req: &crate::webui::HttpRequest| -> Option<String> {
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method.as_str(), path) {
            ("GET", "/api/state") => {
                let g = state.read().ok()?;
                Some(serde_json::json!({ "config": g.config, "telemetry": g.telemetry }).to_string())
            }
            ("GET", "/api/local") => {
                let g = state.read().ok()?;
                let peer_ip = g
                    .telemetry
                    .peer_addr
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let mut status = serde_json::json!({
                    "node": "debian",
                    "label": "Debian Desktop",
                    "hostname": crate::webui::hostname(),
                    "ip": crate::webui::local_ip_toward(&peer_ip),
                    "peer_ip": peer_ip,
                    "version": env!("CARGO_PKG_VERSION"),
                    "os": "Debian 13 (KDE Plasma 6 Wayland)",
                    "peer_online": g.telemetry.peer_online,
                    "active_node": g.telemetry.active_node,
                    "panic_mode": g.telemetry.panic_mode,
                    "game_lock": g.telemetry.game_captive_active,
                    "game_reason": g.telemetry.game_reason,
                    "displays": g.config.debian_monitors,
                    "cursor": [g.telemetry.deb_cursor_x, g.telemetry.deb_cursor_y],
                    "autostart": debian_autostart_enabled(),
                    "input_devices": ["Physical mouse (evdev)", "Desk keyboard (evdev)"],
                    "audio": crate::settings::audio_status_json(true),
                });
                let conflict = g.telemetry.main_conflict;
                crate::settings::add_toggle_status(&mut status, g.config.mouse_speed, conflict);
                Some(status.to_string())
            }
            ("POST", "/api/local/action") => {
                let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
                let act = val.get("action").and_then(|v| v.as_str()).unwrap_or("");
                if act == "set_main_device" {
                    return Some(set_main_device(&state, &val));
                }
                if !handle_local_action(&state, act, &val) {
                    match act {
                        "set_autostart" => set_debian_autostart(
                            val.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                        ),
                        // Focus / panic / game lock are master actions; accept them here too
                        "toggle_focus" | "panic" | "toggle_game_lock" => {
                            apply_master_action(&state, act, &val);
                        }
                        _ => return Some(r#"{"ok":false,"error":"unknown action"}"#.to_string()),
                    }
                }
                Some(r#"{"ok":true}"#.to_string())
            }
            ("POST", "/api/layout") => {
                let mut new_cfg: SpatialConfig = match serde_json::from_str(&req.body) {
                    Ok(c) => c,
                    Err(e) => {
                        return Some(serde_json::json!({ "ok": false, "error": e.to_string() }).to_string())
                    }
                };
                // Debian monitors always come from live hardware. xrandr can hang, so it runs
                // before the lock.
                let (deb_mons, total_w, total_h) = detect_debian_monitors_native();
                let mut g = state.write().ok()?;
                new_cfg.debian_monitors = deb_mons;
                new_cfg.deb_total_w = total_w;
                new_cfg.deb_total_h = total_h;
                new_cfg.win_native_w = g.config.win_native_w;
                new_cfg.win_native_h = g.config.win_native_h;
                if new_cfg.resolution_mode == "native" {
                    new_cfg.win_w = new_cfg.win_native_w;
                    new_cfg.win_h = new_cfg.win_native_h;
                }
                new_cfg.mouse_speed = new_cfg.mouse_speed.clamp(0.05, 2.0);
                new_cfg.edge_resistance = new_cfg.edge_resistance.clamp(0, 400);
                println!(
                    "[Studio] Layout: ({}, {}) {}x{} [{}] side={:?} speed={:.2}x resist={}",
                    new_cfg.win_x,
                    new_cfg.win_y,
                    new_cfg.win_w,
                    new_cfg.win_h,
                    new_cfg.resolution_mode,
                    new_cfg.attach_side,
                    new_cfg.mouse_speed,
                    new_cfg.edge_resistance
                );
                g.config = new_cfg;
                g.save();
                Some(r#"{"ok":true}"#.to_string())
            }
            ("POST", "/api/action") => {
                let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
                let act = val.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if act == "set_main_device" {
                    return Some(set_main_device(&state, &val));
                }
                // Legacy audio action names from older UIs
                let handled_audio = match act.as_str() {
                    "toggle_audio_mute" => crate::settings::handle_common_action(
                        "set_audio_rx_muted",
                        &serde_json::json!({}),
                    ),
                    "toggle_audio_tx" => {
                        crate::settings::handle_common_action("set_audio_tx", &serde_json::json!({}))
                    }
                    "set_audio_volume" => {
                        crate::settings::handle_common_action("set_audio_rx_volume", &val)
                    }
                    other => handle_local_action(&state, other, &val),
                };
                if !handled_audio {
                    apply_master_action(&state, &act, &val);
                }
                Some(r#"{"ok":true}"#.to_string())
            }
            _ => None,
        }
    })
}

fn set_main_device(state: &Arc<RwLock<SharedSpatialState>>, val: &serde_json::Value) -> String {
    let link_up = state.read().is_ok_and(|g| g.telemetry.peer_online);
    crate::settings::set_main_device(val, link_up)
}

/// `settings::handle_common_action`, except that Debian's pointer speed is the layout's
/// `mouse_speed`, which the input loop picks up from its next config snapshot.
fn handle_local_action(
    state: &Arc<RwLock<SharedSpatialState>>,
    act: &str,
    val: &serde_json::Value,
) -> bool {
    if act != "set_pointer_speed" {
        return crate::settings::handle_common_action(act, val);
    }
    if let Some(speed) = crate::settings::requested_pointer_speed(val) {
        if let Ok(mut g) = state.write() {
            g.config.mouse_speed = speed;
            g.save();
        }
    }
    true
}

fn apply_master_action(state: &Arc<RwLock<SharedSpatialState>>, act: &str, _val: &serde_json::Value) {
    // xrandr can hang, so it runs before the lock.
    let detected = (act == "reset_native_resolutions").then(detect_debian_monitors_native);
    let Ok(mut g) = state.write() else { return };
    if let Some((deb_mons, total_w, total_h)) = detected {
        g.config.debian_monitors = deb_mons;
        g.config.deb_total_w = total_w;
        g.config.deb_total_h = total_h;
        g.config.resolution_mode = "native".to_string();
        g.config.win_w = g.config.win_native_w.max(640);
        g.config.win_h = g.config.win_native_h.max(480);
        g.config.resnap_after_resize();
        g.save();
        return;
    }
    match act {
        "toggle_focus" => g.pending_manual_switch = Some("toggle".to_string()),
        "panic" => g.pending_panic = true,
        "toggle_game_lock" => {
            g.config.manual_game_lock = !g.config.manual_game_lock;
            g.save();
        }
        "toggle_auto_game_lock" => {
            g.config.auto_game_lock = !g.config.auto_game_lock;
            g.save();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DP-3 (1920x1080 at 0,360) beside DP-2 (2560x1440 at 1920,0): both end at y = 1440.
    fn layout(attach_side: TouchSide, win_x: i32, win_y: i32) -> SpatialConfig {
        SpatialConfig {
            debian_monitors: vec![
                MonitorRect { name: "DP-3".into(), x: 0, y: 360, w: 1920, h: 1080 },
                MonitorRect { name: "DP-2".into(), x: 1920, y: 0, w: 2560, h: 1440 },
            ],
            deb_total_w: 4480,
            deb_total_h: 1440,
            win_x,
            win_y,
            win_w: 1920,
            win_h: 1080,
            win_native_w: 1920,
            win_native_h: 1080,
            resolution_mode: "native".to_string(),
            attach_side,
            mouse_speed: 0.35,
            edge_resistance: 55,
            auto_game_lock: true,
            manual_game_lock: false,
        }
    }

    fn span(edge: SwitchEdge, start: i32, end: i32) -> EdgeSpan {
        EdgeSpan { edge, start, end }
    }

    #[test]
    fn bottom_attachment_gives_top_span() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Top, 0, 1919));
    }

    #[test]
    fn right_attachment_gives_left_span() {
        let c = layout(TouchSide::Right, 4480, 200);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Left, 0, 1079));
    }

    #[test]
    fn left_attachment_gives_right_span() {
        // DP-3 starts at y = 360, so the laptop's first 360 rows have no neighbour.
        let c = layout(TouchSide::Left, -1920, 0);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Right, 360, 1079));
    }

    #[test]
    fn top_attachment_gives_bottom_span() {
        let c = layout(TouchSide::Top, 2240, -1080);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Bottom, 0, 1919));
    }

    #[test]
    fn partial_overlap_is_clamped() {
        // Hanging 640 px past DP-2's right end.
        let c = layout(TouchSide::Bottom, 3200, 1440);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Top, 0, 1279));
        // Hanging 200 px above DP-2's top.
        let c = layout(TouchSide::Right, 4480, -200);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Left, 200, 1079));
    }

    #[test]
    fn span_covers_every_monitor_on_the_same_line() {
        let c = layout(TouchSide::Bottom, 1500, 1440);
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Top, 0, 1919));
    }

    #[test]
    fn monitor_ending_short_of_the_laptop_does_not_count() {
        let mut c = layout(TouchSide::Bottom, 1500, 1440);
        c.debian_monitors[0].h = 720; // DP-3 now ends at y = 1080
        assert_eq!(c.laptop_edge_span(), span(SwitchEdge::Top, 420, 1919));
    }

    #[test]
    fn no_overlap_gives_an_empty_span() {
        let c = layout(TouchSide::Bottom, 5000, 1440);
        let s = c.laptop_edge_span();
        assert_eq!(s.edge, SwitchEdge::Top);
        assert!(s.start > s.end);
    }

    #[test]
    fn laptop_point_converts_to_debian_coordinates() {
        // Laptop right of DP-2: (-1, 300) is just outside, so it lands on DP-2's last column.
        let c = layout(TouchSide::Right, 4480, 200);
        assert_eq!(c.laptop_to_debian(-1, 300), (4451, 500));
    }

    #[test]
    fn laptop_point_clamps_to_nearest_monitor() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.laptop_to_debian(100, -1), (2340, 1411));
        assert_eq!(c.laptop_to_debian(100, 40), (2340, 1411));
        // Left of the laptop's origin and below DP-3.
        assert_eq!(c.laptop_to_debian(-1000, 0), (1240, 1411));
        // The empty corner above DP-3 goes to the closer of the two monitors.
        let c = layout(TouchSide::Top, 0, 0);
        assert_eq!(c.laptop_to_debian(1900, 100), (1948, 100));
        assert_eq!(c.laptop_to_debian(100, 340), (100, 388));
    }

    #[test]
    fn keep_cursor_is_not_converted() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.laptop_to_debian(KEEP_CURSOR, KEEP_CURSOR), (KEEP_CURSOR, KEEP_CURSOR));
        // A half-set sentinel must not become a warp to a clamped corner.
        assert_eq!(c.laptop_to_debian(KEEP_CURSOR, 5), (KEEP_CURSOR, KEEP_CURSOR));
        assert_eq!(c.laptop_to_debian(5, KEEP_CURSOR), (KEEP_CURSOR, KEEP_CURSOR));
    }

    #[test]
    fn hostile_laptop_coordinates_stay_on_a_monitor() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.laptop_to_debian(i32::MAX, i32::MAX), (4451, 1411));
        assert_eq!(c.laptop_to_debian(i32::MIN + 1, i32::MIN + 1), (28, 388));
    }

    #[test]
    fn laptop_point_without_monitors_uses_the_desktop_box() {
        let mut c = layout(TouchSide::Bottom, 2240, 1440);
        c.debian_monitors.clear();
        assert_eq!(c.laptop_to_debian(100, 40), (2340, 1411));
    }

    #[test]
    fn entry_is_inset_from_the_edge_it_lands_on_only() {
        let c = layout(TouchSide::Right, 4480, 200);
        // Interior points on the free axis keep their coordinate.
        assert_eq!(c.laptop_to_debian(-1, 700), (4451, 900));
        // An interior point is untouched.
        assert_eq!(c.laptop_to_debian(-500, 300), (3980, 500));
    }

    #[test]
    fn inset_stays_inside_a_tiny_monitor() {
        let mut c = layout(TouchSide::Right, 4480, 200);
        c.debian_monitors = vec![MonitorRect { name: "T".into(), x: 0, y: 0, w: 40, h: 40 }];
        let (x, y) = c.laptop_to_debian(-4480, -200);
        assert!((0..40).contains(&x) && (0..40).contains(&y));
    }

    fn shared_state() -> RwLock<SharedSpatialState> {
        RwLock::new(SharedSpatialState {
            config: layout(TouchSide::Bottom, 2240, 1440),
            telemetry: LiveTelemetry::new(String::new()),
            pending_manual_switch: Some("toggle".to_string()),
            pending_panic: true,
        })
    }

    #[test]
    fn studio_requests_are_taken_once() {
        let state = shared_state();
        let first = take_studio_requests(&state).unwrap();
        assert!(first.manual_switch && first.panic);
        assert_eq!(first.config.win_x, 2240);
        let second = take_studio_requests(&state).unwrap();
        assert!(!second.manual_switch && !second.panic);
    }

    #[test]
    fn busy_lock_skips_the_sync_and_keeps_requests() {
        let state = shared_state();
        {
            let _studio = state.read().unwrap();
            assert!(take_studio_requests(&state).is_none());
        }
        let taken = take_studio_requests(&state).unwrap();
        assert!(taken.manual_switch && taken.panic);
    }

    #[test]
    fn edge_push_crosses_after_resistance() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let mut pushed = 0;
        assert_eq!(c.push_debian_edge(2340, 1439, 0, 30, &mut pushed), None);
        assert_eq!(pushed, 30);
        assert_eq!(c.push_debian_edge(2340, 1439, 0, 30, &mut pushed), Some((100, 28)));
        assert_eq!(pushed, 0);
    }

    #[test]
    fn edge_push_resets_away_from_the_boundary() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let mut pushed = 0;
        c.push_debian_edge(2340, 1439, 0, 30, &mut pushed);
        assert_eq!(c.push_debian_edge(2340, 1000, 0, 30, &mut pushed), None);
        assert_eq!(pushed, 0);
    }
}
