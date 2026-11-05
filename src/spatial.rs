use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;
use std::sync::atomic::Ordering;
#[allow(unused_imports)]
use std::sync::atomic::AtomicBool as _AtomicBoolUnused;
use std::sync::{Arc, RwLock};

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
}

pub struct SharedSpatialState {
    pub config: SpatialConfig,
    pub telemetry: LiveTelemetry,
    pub pending_manual_switch: Option<String>,
    pub pending_panic: bool,
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
            telemetry: LiveTelemetry {
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
            },
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

    /// Checks if the Windows cursor `(wx, wy)` with movement `(dx, dy)` crossed back into Debian,
    /// returning the target `(deb_x, deb_y)` on Debian.
    pub fn check_windows_to_debian(&self, wx: i32, wy: i32, dx: i32, dy: i32) -> Option<(i32, i32)> {
        let raw_deb_x = self.win_x + wx;
        let raw_deb_y = self.win_y + wy;

        match self.attach_side {
            TouchSide::Right => {
                if dx < 0 && wx <= 4 {
                    let target_x = (self.win_x - 90).clamp(0, self.deb_total_w - 1);
                    let target_y = self.clamp_to_debian_y(target_x, raw_deb_y);
                    return Some((target_x, target_y));
                }
            }
            TouchSide::Left => {
                if dx > 0 && wx >= self.win_w - 4 {
                    let target_x = (self.win_x + self.win_w + 90).clamp(0, self.deb_total_w - 1);
                    let target_y = self.clamp_to_debian_y(target_x, raw_deb_y);
                    return Some((target_x, target_y));
                }
            }
            TouchSide::Bottom => {
                if dy < 0 && wy <= 4 {
                    let target_x = raw_deb_x.clamp(0, self.deb_total_w - 1);
                    let target_y = (self.win_y - 90).clamp(0, self.deb_total_h - 1);
                    return Some((target_x, self.clamp_to_debian_y(target_x, target_y)));
                }
            }
            TouchSide::Top => {
                if dy > 0 && wy >= self.win_h - 4 {
                    let target_x = raw_deb_x.clamp(0, self.deb_total_w - 1);
                    let target_y = (self.win_y + self.win_h + 90).clamp(0, self.deb_total_h - 1);
                    return Some((target_x, self.clamp_to_debian_y(target_x, target_y)));
                }
            }
        }
        None
    }

    fn clamp_to_debian_y(&self, x: i32, y: i32) -> i32 {
        for m in &self.debian_monitors {
            if x >= m.x && x < m.x + m.w {
                return y.clamp(m.y + 4, m.y + m.h - 4);
            }
        }
        y.clamp(0, self.deb_total_h - 1)
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
                Some(
                    serde_json::json!({
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
                    })
                    .to_string(),
                )
            }
            ("POST", "/api/local/action") => {
                let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
                let act = val.get("action").and_then(|v| v.as_str()).unwrap_or("");
                if !crate::settings::handle_common_action(act, &val) {
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
                let mut g = state.write().ok()?;
                // Debian monitors always come from live hardware
                let (deb_mons, total_w, total_h) = detect_debian_monitors_native();
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
                    other => crate::settings::handle_common_action(other, &val),
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

fn apply_master_action(state: &Arc<RwLock<SharedSpatialState>>, act: &str, _val: &serde_json::Value) {
    let Ok(mut g) = state.write() else { return };
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
        "reset_native_resolutions" => {
            let (deb_mons, total_w, total_h) = detect_debian_monitors_native();
            g.config.debian_monitors = deb_mons;
            g.config.deb_total_w = total_w;
            g.config.deb_total_h = total_h;
            g.config.resolution_mode = "native".to_string();
            g.config.win_w = g.config.win_native_w.max(640);
            g.config.win_h = g.config.win_native_h.max(480);
            g.config.resnap_after_resize();
            g.save();
        }
        _ => {}
    }
    let _ = Ordering::Relaxed;
}
