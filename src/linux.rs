#![cfg(target_os = "linux")]

use crate::keymap::{
    KEY_F11, KEY_F12, KEY_G, KEY_LEFTALT, KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_SCROLLLOCK,
};
use crate::protocol::{
    NodeId, Packet, Payload, SwitchEdge, MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT, MOUSE_BTN_MIDDLE,
    MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
};
use crate::spatial::{studio_handler, SharedSpatialState, TouchSide};
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::io::RawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// Linux input event constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_REP: u16 = 0x14;

const SYN_REPORT: u16 = 0x00;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

// uinput ioctl constants (x86_64 Linux)
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c5504;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

// evdev ioctl constants (x86_64 Linux)
const EVIOCGRAB: libc::c_ulong = 0x40044590;

#[inline]
const fn eviocgname(len: usize) -> libc::c_ulong {
    (2 << 30) | ((len as libc::c_ulong) << 16) | ((b'E' as libc::c_ulong) << 8) | 0x06
}

#[inline]
const fn eviocgbit(ev: u16, len: usize) -> libc::c_ulong {
    (2 << 30)
        | ((len as libc::c_ulong) << 16)
        | ((b'E' as libc::c_ulong) << 8)
        | (0x20 + ev as libc::c_ulong)
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct InputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UinputUserDev {
    name: [u8; 80],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; 64],
    absmin: [i32; 64],
    absfuzz: [i32; 64],
    absflat: [i32; 64],
}

impl Default for UinputUserDev {
    fn default() -> Self {
        Self {
            name: [0u8; 80],
            id: InputId {
                bustype: 0x03,
                vendor: 0x1d6b,
                product: 0x0104,
                version: 1,
            },
            ff_effects_max: 0,
            absmax: [0; 64],
            absmin: [0; 64],
            absfuzz: [0; 64],
            absflat: [0; 64],
        }
    }
}

pub struct VirtualKeyboard {
    fd: RawFd,
    held_keys: [bool; 256],
}

impl VirtualKeyboard {
    pub fn new() -> Result<Self, String> {
        let path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(format!(
                "Failed to open /dev/uinput (errno {}).",
                std::io::Error::last_os_error()
            ));
        }

        unsafe {
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_REP as libc::c_int);

            for key in 1..248 {
                libc::ioctl(fd, UI_SET_KEYBIT, key as libc::c_int);
            }

            let mut udev = UinputUserDev::default();
            let dev_name = b"mesh-kvm Virtual Keyboard";
            udev.name[..dev_name.len()].copy_from_slice(dev_name);

            let written = libc::write(
                fd,
                &udev as *const UinputUserDev as *const libc::c_void,
                std::mem::size_of::<UinputUserDev>(),
            );
            if written < 0 || libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                libc::close(fd);
                return Err("Failed UI_DEV_CREATE for Virtual Keyboard".to_string());
            }
        }

        Ok(Self {
            fd,
            held_keys: [false; 256],
        })
    }

    pub fn inject_key(&mut self, scancode: u16, pressed: bool) {
        if scancode == 0 || scancode as usize >= self.held_keys.len() {
            return;
        }
        let idx = scancode as usize;
        let value = if pressed {
            if self.held_keys[idx] {
                2
            } else {
                self.held_keys[idx] = true;
                1
            }
        } else {
            self.held_keys[idx] = false;
            0
        };

        self.write_event(EV_KEY, scancode, value);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn release_all_keys(&mut self) {
        let mut any = false;
        for code in 1..self.held_keys.len() {
            if self.held_keys[code] {
                self.held_keys[code] = false;
                self.write_event(EV_KEY, code as u16, 0);
                any = true;
            }
        }
        if any {
            self.write_event(EV_SYN, SYN_REPORT, 0);
        }
    }

    #[inline]
    fn write_event(&self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        unsafe {
            let _ = libc::write(
                self.fd,
                &ev as *const InputEvent as *const libc::c_void,
                std::mem::size_of::<InputEvent>(),
            );
        }
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        self.release_all_keys();
        unsafe {
            libc::ioctl(self.fd, UI_DEV_DESTROY);
            libc::close(self.fd);
        }
    }
}

/// Dedicated EV_ABS-only pointer device using UI_ABS_SETUP + UI_DEV_SETUP so KDE Plasma 6 Wayland
/// maps (ABS_X, ABS_Y) 1:1 to exact desktop coordinates (0..width-1, 0..height-1).
pub struct VirtualMouse {
    fd: RawFd,
}

impl VirtualMouse {
    pub fn new(width: i32, height: i32) -> Result<Self, String> {
        let path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err("Failed to open /dev/uinput for Virtual Mouse".to_string());
        }

        unsafe {
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);
            libc::ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_int);
            libc::ioctl(fd, UI_SET_ABSBIT, ABS_X as libc::c_int);
            libc::ioctl(fd, UI_SET_ABSBIT, ABS_Y as libc::c_int);

            let abs_x = UinputAbsSetup {
                code: ABS_X,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: width.max(1) - 1,
                    fuzz: 0,
                    flat: 0,
                    resolution: 1,
                },
            };
            let abs_y = UinputAbsSetup {
                code: ABS_Y,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: height.max(1) - 1,
                    fuzz: 0,
                    flat: 0,
                    resolution: 1,
                },
            };
            libc::ioctl(fd, UI_ABS_SETUP, &abs_x as *const UinputAbsSetup);
            libc::ioctl(fd, UI_ABS_SETUP, &abs_y as *const UinputAbsSetup);

            let mut setup = UinputSetup {
                id: InputId {
                    bustype: 0x03,
                    vendor: 0x1d6b,
                    product: 0x0105,
                    version: 1,
                },
                name: [0u8; 80],
                ff_effects_max: 0,
            };
            let dev_name = b"mesh-kvm Absolute Warp Pointer";
            setup.name[..dev_name.len()].copy_from_slice(dev_name);

            if libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) < 0
                || libc::ioctl(fd, UI_DEV_CREATE) < 0
            {
                libc::close(fd);
                return Err("Failed to create mesh-kvm Absolute Warp Pointer".to_string());
            }
        }

        Ok(Self { fd })
    }

    pub fn warp_abs(&self, x: i32, y: i32) {
        self.write_event(EV_ABS, ABS_X, x);
        self.write_event(EV_ABS, ABS_Y, y);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    #[inline]
    fn write_event(&self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        unsafe {
            let _ = libc::write(
                self.fd,
                &ev as *const InputEvent as *const libc::c_void,
                std::mem::size_of::<InputEvent>(),
            );
        }
    }
}

impl Drop for VirtualMouse {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, UI_DEV_DESTROY);
            libc::close(self.fd);
        }
    }
}

#[inline]
fn test_bit(bits: &[u8], bit: usize) -> bool {
    let byte_idx = bit / 8;
    let bit_idx = bit % 8;
    byte_idx < bits.len() && (bits[byte_idx] & (1 << bit_idx)) != 0
}

pub struct EvdevDevice {
    pub path: String,
    pub name: String,
    pub fd: RawFd,
    pub grabbed: bool,
}

impl EvdevDevice {
    pub fn set_grab(&mut self, enable: bool) {
        if self.grabbed == enable {
            return;
        }
        let arg: libc::c_int = if enable { 1 } else { 0 };
        let res = unsafe { libc::ioctl(self.fd, EVIOCGRAB, arg) };
        if res == 0 {
            self.grabbed = enable;
            println!(
                "[Debian] EVIOCGRAB {} on '{}' ({})",
                if enable { "ENABLED" } else { "RELEASED" },
                self.name,
                self.path
            );
        }
    }
}

impl Drop for EvdevDevice {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = unsafe { libc::ioctl(self.fd, EVIOCGRAB, 0 as libc::c_int) };
        }
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn discover_devices(explicit_mouse: Option<&str>) -> (Vec<EvdevDevice>, Vec<EvdevDevice>) {
    let mut mice = Vec::new();
    let mut keyboards = Vec::new();

    if let Some(mouse_path) = explicit_mouse {
        if let Ok(cpath) = CString::new(mouse_path) {
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                let name = get_device_name(fd);
                mice.push(EvdevDevice {
                    path: mouse_path.to_string(),
                    name,
                    fd,
                    grabbed: false,
                });
            }
        }
    }

    let entries = match fs::read_dir("/dev/input") {
        Ok(e) => e,
        Err(_) => return (mice, keyboards),
    };

    let mut paths: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().to_string())
        .filter(|p| p.contains("/event"))
        .collect();
    paths.sort();

    for path in paths {
        if explicit_mouse == Some(path.as_str()) {
            continue;
        }
        let cpath = match CString::new(path.as_str()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            continue;
        }

        let name = get_device_name(fd);
        let lower_name = name.to_ascii_lowercase();
        if lower_name.contains("mesh-kvm") || lower_name.contains("ydotool") {
            unsafe { libc::close(fd) };
            continue;
        }

        let mut ev_bits = [0u8; 4];
        let mut rel_bits = [0u8; 4];
        let mut key_bits = [0u8; 96];

        unsafe {
            libc::ioctl(fd, eviocgbit(0, ev_bits.len()), ev_bits.as_mut_ptr());
            libc::ioctl(fd, eviocgbit(EV_REL, rel_bits.len()), rel_bits.as_mut_ptr());
            libc::ioctl(fd, eviocgbit(EV_KEY, key_bits.len()), key_bits.as_mut_ptr());
        }

        let has_rel = test_bit(&ev_bits, EV_REL as usize)
            && test_bit(&rel_bits, REL_X as usize)
            && test_bit(&rel_bits, REL_Y as usize);
        let has_btn_left = test_bit(&key_bits, BTN_LEFT as usize);
        let has_kbd_keys = test_bit(&key_bits, KEY_LEFTCTRL as usize)
            && test_bit(&key_bits, KEY_F12 as usize);

        if explicit_mouse.is_none() && has_rel && has_btn_left {
            println!("[Debian] Auto-discovered physical mouse: '{}' ({})", name, path);
            mice.push(EvdevDevice {
                path,
                name,
                fd,
                grabbed: false,
            });
        } else if has_kbd_keys {
            keyboards.push(EvdevDevice {
                path,
                name,
                fd,
                grabbed: false,
            });
        } else {
            unsafe { libc::close(fd) };
        }
    }

    (mice, keyboards)
}

fn get_device_name(fd: RawFd) -> String {
    let mut buf = [0u8; 256];
    let res = unsafe { libc::ioctl(fd, eviocgname(buf.len()), buf.as_mut_ptr()) };
    if res > 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).trim().to_string()
    } else {
        "Unknown Input Device".to_string()
    }
}

pub fn detect_screen_dimensions() -> (i32, i32) {
    if let Ok(output) = Command::new("xrandr").arg("--current").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(idx) = line.find("current ") {
                let after = &line[idx + "current ".len()..];
                let parts: Vec<&str> = after
                    .split(|c: char| !c.is_ascii_digit())
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.len() >= 2 {
                    if let (Ok(w), Ok(h)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>()) {
                        if w > 0 && h > 0 {
                            return (w, h);
                        }
                    }
                }
            }
        }
    }
    (4480, 1440)
}

/// Spawns a KWin Scripting bridge that tracks the TRUE KDE Plasma 6 Wayland cursor (x, y)
/// AND active fullscreen/game windows in real time via `workspace.cursorPosChanged` over session D-Bus.
fn spawn_kwin_cursor_tracker(
    real_x: Arc<AtomicI32>,
    real_y: Arc<AtomicI32>,
    kwin_game_active: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("mesh-kvm-kwin-cursor".to_string())
        .spawn(move || {
            let script_path = "/tmp/mesh_kvm_kwin_cursor.js";
            let js = r#"
var lastEmit = 0;
function isGameWindow() {
    var w = workspace.activeWindow;
    if (!w) return 0;
    if (w.fullScreen) return 1;
    var rc = (w.resourceClass || "").toString().toLowerCase();
    var rn = (w.resourceName || "").toString().toLowerCase();
    var cap = (w.caption || "").toString().toLowerCase();
    if (rc.indexOf("steam_app") !== -1 || rc.indexOf("gamescope") !== -1 ||
        rc.indexOf("wine") !== -1 || rc.indexOf("proton") !== -1 ||
        rc.indexOf("minecraft") !== -1 || rc.indexOf("prism") !== -1 ||
        rc.indexOf("lutris") !== -1 || rc.indexOf("heroic") !== -1 ||
        rc.indexOf("cs2") !== -1 || rc.indexOf("dota2") !== -1 ||
        rc.indexOf("retroarch") !== -1 || rn.indexOf("steam_app") !== -1 ||
        cap.indexOf("counter-strike") !== -1) {
        return 1;
    }
    return 0;
}
function emitPos(force) {
    var p = workspace.cursorPos;
    var x = Math.round(p.x);
    var y = Math.round(p.y);
    var now = Date.now();
    var nearEdge = (x <= 16 || x >= 4464 || (x >= 1904 && x <= 1936) ||
                    y <= 16 || y >= 1424 || (y >= 344 && y <= 376));
    if (force || nearEdge || (now - lastEmit) >= 12) {
        lastEmit = now;
        callDBus("org.meshkvm.Cursor", "/Cursor", "org.meshkvm.Cursor", "Update", x, y, isGameWindow());
    }
}
workspace.cursorPosChanged.connect(function() { emitPos(false); });
if (workspace.windowActivated) {
    workspace.windowActivated.connect(function() { emitPos(true); });
}
emitPos(true);
"#;
            let _ = fs::write(script_path, js);

            // Start dbus-monitor BEFORE running the KWin script so we catch the initial emitPos(true)
            let child_opt = Command::new("dbus-monitor")
                .args(["--session", "interface='org.meshkvm.Cursor'"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok();

            std::thread::sleep(Duration::from_millis(80));

            let _ = Command::new("qdbus6")
                .args([
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.unloadScript",
                    "mesh_kvm_cursor_watch",
                ])
                .output();

            if let Ok(out) = Command::new("qdbus6")
                .args([
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.loadScript",
                    script_path,
                    "mesh_kvm_cursor_watch",
                ])
                .output()
            {
                let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let _ = Command::new("qdbus6")
                    .args([
                        "org.kde.KWin",
                        &format!("/Scripting/Script{}", id),
                        "org.kde.kwin.Script.run",
                    ])
                    .output();
            }

            if let Some(mut child) = child_opt {
                if let Some(stdout) = child.stdout.take() {
                    let reader = BufReader::new(stdout);
                    let mut ints: Vec<i32> = Vec::with_capacity(3);
                    for line in reader.lines().flatten() {
                        let trimmed = line.trim();
                        if let Some(rest) = trimmed.strip_prefix("int32 ") {
                            if let Ok(val) = rest.trim().parse::<i32>() {
                                ints.push(val);
                                if ints.len() == 2 {
                                    real_x.store(ints[0], Ordering::Relaxed);
                                    real_y.store(ints[1], Ordering::Relaxed);
                                } else if ints.len() >= 3 {
                                    kwin_game_active.store(ints[2] != 0, Ordering::Relaxed);
                                }
                            }
                        } else if trimmed.starts_with("method call") {
                            ints.clear();
                        }
                    }
                }
            }
        })
        .ok();
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn run_debian_node(
    peer_addr: SocketAddr,
    listen_port: u16,
    _switch_edge: SwitchEdge,
    override_width: Option<i32>,
    override_height: Option<i32>,
    explicit_mouse: Option<String>,
) -> Result<(), String> {
    let (det_w, det_h) = detect_screen_dimensions();
    let width = override_width.unwrap_or(det_w).max(100);
    let height = override_height.unwrap_or(det_h).max(100);

    let spatial_state = Arc::new(RwLock::new(SharedSpatialState::load_or_default(
        peer_addr.to_string(),
    )));
    crate::webui::start(
        peer_addr.ip().to_string(),
        studio_handler(Arc::clone(&spatial_state)),
    )?;

    let mut vkbd = VirtualKeyboard::new()?;
    let vmouse = VirtualMouse::new(width, height)?;
    println!("[Debian] Created /dev/uinput Virtual Keyboard & Absolute Warp Pointer.");

    let kwin_x = Arc::new(AtomicI32::new(3200));
    let kwin_y = Arc::new(AtomicI32::new(720));
    let kwin_game_active = Arc::new(AtomicBool::new(false));
    spawn_kwin_cursor_tracker(
        Arc::clone(&kwin_x),
        Arc::clone(&kwin_y),
        Arc::clone(&kwin_game_active),
    );

    let (mut mice, keyboards) = discover_devices(explicit_mouse.as_deref());
    if mice.is_empty() {
        return Err("No physical mouse devices found in /dev/input/event*".to_string());
    }

    let bind_addr: SocketAddr = format!("0.0.0.0:{}", listen_port).parse().unwrap();
    let socket = UdpSocket::bind(bind_addr)
        .map_err(|e| format!("Failed to bind UDP {}: {}", bind_addr, e))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set UDP socket nonblocking: {}", e))?;

    let active_node = Arc::new(AtomicU8::new(NodeId::Debian as u8));
    let panic_mode = Arc::new(AtomicBool::new(false));
    // Initialize sequence counter with current Unix timestamp in microseconds so that
    // across daemon restarts `pkt.seq` is always strictly greater than `last_rx_focus_seq` on the peer.
    let init_seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let seq_counter = Arc::new(AtomicU64::new(init_seq));

    let mut was_peer_online = false;
    let mut win_cursor_x: i32 = 960;
    let mut win_cursor_y: i32 = 540;
    let mut frac_dx: f32 = 0.0;
    let mut frac_dy: f32 = 0.0;
    let mut edge_push_accum: i32 = 0;

    let mut last_switch_instant = Instant::now() - Duration::from_secs(1);
    let mut last_heartbeat = Instant::now();
    let mut last_telemetry_sync = Instant::now();
    let mut last_rx_focus_seq = 0u64;
    let mut last_peer_seen: Option<Instant> = None;
    let mut peer_reports_heartbeats = false;

    let mut kbd_ctrl_l = false;
    let mut kbd_ctrl_r = false;
    let mut kbd_alt_l = false;
    let mut kbd_alt_r = false;
    let mut mouse_buttons_mask: u8 = 0;

    let mut udp_buf = [0u8; 512];
    let mut ev_buf = [InputEvent::default(); 64];

    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, handle_sigint as *const () as usize);
    }

    println!(
        "[Debian] Spatial Studio UI ready at: http://localhost:24880 (or http://192.168.8.241:24880)"
    );

    // Immediately sync initial Debian focus state with Windows peer on startup
    send_burst(
        &socket,
        peer_addr,
        &seq_counter,
        Payload::FocusSwitch {
            target_node: NodeId::Debian,
            cursor_y_ratio: 0.5,
        },
    );

    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            let _ = Command::new("qdbus6")
                .args([
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.unloadScript",
                    "mesh_kvm_cursor_watch",
                ])
                .output();
            println!("\n[Debian] Shutting down cleanly; releasing all EVIOCGRAB locks.");
            break Ok(());
        }

        // Check WebUI actions (manual toggle / panic)
        let (manual_toggle, web_panic, cfg_snapshot) = if let Ok(mut guard) = spatial_state.write()
        {
            let mt = guard.pending_manual_switch.take().is_some();
            let wp = guard.pending_panic;
            guard.pending_panic = false;
            (mt, wp, guard.config.clone())
        } else {
            (false, false, Default::default())
        };

        if web_panic {
            panic_mode.store(true, Ordering::SeqCst);
            active_node.store(NodeId::Debian as u8, Ordering::SeqCst);
            vkbd.release_all_keys();
            for m in &mut mice {
                m.set_grab(false);
            }
            send_burst(&socket, peer_addr, &seq_counter, Payload::PanicRelease);
        }

        if manual_toggle {
            panic_mode.store(false, Ordering::SeqCst);
            let cur = active_node.load(Ordering::SeqCst);
            let next = if cur == NodeId::Debian as u8 {
                NodeId::Windows
            } else {
                NodeId::Debian
            };
            active_node.store(next as u8, Ordering::SeqCst);
            let grab = next == NodeId::Windows;
            if grab {
                vkbd.release_all_keys();
            }
            for m in &mut mice {
                m.set_grab(grab);
            }
            last_switch_instant = Instant::now();
            edge_push_accum = 0;
            send_burst(
                &socket,
                peer_addr,
                &seq_counter,
                Payload::FocusSwitch {
                    target_node: next,
                    cursor_y_ratio: 0.5,
                },
            );
        }

        // 1. Drain incoming UDP packets on port 24850
        while let Ok((len, src_addr)) = socket.recv_from(&mut udp_buf) {
            if let Ok(pkt) = Packet::decode(&udp_buf[..len]) {
                last_peer_seen = Some(Instant::now());
                match pkt.payload {
                    Payload::KeyEvent { scancode, pressed } => {
                        if !panic_mode.load(Ordering::Relaxed)
                            && active_node.load(Ordering::Relaxed) == NodeId::Debian as u8
                        {
                            vkbd.inject_key(scancode, pressed);
                        }
                    }
                    Payload::FocusSwitch {
                        target_node,
                        cursor_y_ratio,
                    } => {
                        if pkt.seq <= last_rx_focus_seq && pkt.seq != 0 {
                            continue;
                        }
                        last_rx_focus_seq = pkt.seq;

                        if target_node == NodeId::Debian {
                            // Only accept peer's Left-edge FocusSwitch if attach_side is Right (or manual toggle)
                            let prev = active_node.swap(NodeId::Debian as u8, Ordering::SeqCst);
                            for m in &mut mice {
                                m.set_grab(false);
                            }
                            last_switch_instant = Instant::now();
                            edge_push_accum = 0;

                            let wy = ((cursor_y_ratio * cfg_snapshot.win_h as f32) as i32)
                                .clamp(0, cfg_snapshot.win_h - 1);
                            let (target_deb_x, target_deb_y) = match cfg_snapshot.attach_side {
                                TouchSide::Right => (
                                    (cfg_snapshot.win_x - 100).clamp(0, width - 1),
                                    (cfg_snapshot.win_y + wy).clamp(0, height - 1),
                                ),
                                TouchSide::Left => (
                                    (cfg_snapshot.win_x + cfg_snapshot.win_w + 100)
                                        .clamp(0, width - 1),
                                    (cfg_snapshot.win_y + wy).clamp(0, height - 1),
                                ),
                                TouchSide::Bottom => (
                                    (cfg_snapshot.win_x + win_cursor_x).clamp(0, width - 1),
                                    (cfg_snapshot.win_y - 100).clamp(0, height - 1),
                                ),
                                TouchSide::Top => (
                                    (cfg_snapshot.win_x + win_cursor_x).clamp(0, width - 1),
                                    (cfg_snapshot.win_y + cfg_snapshot.win_h + 100)
                                        .clamp(0, height - 1),
                                ),
                            };

                            vmouse.warp_abs(target_deb_x, target_deb_y);
                            kwin_x.store(target_deb_x, Ordering::Relaxed);
                            kwin_y.store(target_deb_y, Ordering::Relaxed);

                            if prev != NodeId::Debian as u8 {
                                println!(
                                    "[Debian] <-- Focus switched to DEBIAN at ({}, {})",
                                    target_deb_x, target_deb_y
                                );
                            }

                            let ack = Packet::new(
                                seq_counter.fetch_add(1, Ordering::Relaxed),
                                NodeId::Debian,
                                Payload::FocusAck {
                                    active_node: NodeId::Debian,
                                    seq_ref: pkt.seq,
                                },
                            );
                            if let Ok(bytes) = ack.encode() {
                                let _ = socket.send_to(&bytes, src_addr);
                            }
                        } else if target_node == NodeId::Windows
                            && !panic_mode.load(Ordering::Relaxed)
                        {
                            active_node.store(NodeId::Windows as u8, Ordering::SeqCst);
                            vkbd.release_all_keys();
                            for m in &mut mice {
                                m.set_grab(true);
                            }
                            last_switch_instant = Instant::now();
                            edge_push_accum = 0;
                        }
                    }
                    Payload::PanicRelease => {
                        panic_mode.store(true, Ordering::SeqCst);
                        active_node.store(NodeId::Debian as u8, Ordering::SeqCst);
                        vkbd.release_all_keys();
                        for m in &mut mice {
                            m.set_grab(false);
                        }
                        println!("[PANIC] Received PanicRelease from Windows!");
                    }
                    Payload::PeerDisplayInfo {
                        width: peer_w,
                        height: peer_h,
                    } => {
                        peer_reports_heartbeats = true;
                        if peer_w >= 640 && peer_h >= 480 {
                            if let Ok(mut guard) = spatial_state.write() {
                                let changed = guard.config.win_native_w != peer_w
                                    || guard.config.win_native_h != peer_h;
                                guard.config.win_native_w = peer_w;
                                guard.config.win_native_h = peer_h;
                                guard.telemetry.win_peer_reported_res = true;
                                if guard.config.resolution_mode == "native"
                                    && (guard.config.win_w != peer_w
                                        || guard.config.win_h != peer_h)
                                {
                                    guard.config.win_w = peer_w;
                                    guard.config.win_h = peer_h;
                                    guard.config.resnap_after_resize();
                                    guard.save();
                                    println!(
                                        "[Debian] Auto-synced Windows 11 native display resolution: {}x{}",
                                        peer_w, peer_h
                                    );
                                } else if changed {
                                    guard.save();
                                }
                            }
                        }
                    }
                    Payload::Heartbeat => {
                        peer_reports_heartbeats = true;
                    }
                    _ => {}
                }
            }
        }

        // 2. Check local keyboards on Debian for Panic Hotkey (Ctrl+Alt+ScrollLock / F12) & Toggle (Ctrl+Alt+F11)
        for kbd in &keyboards {
            loop {
                let n = unsafe {
                    libc::read(
                        kbd.fd,
                        ev_buf.as_mut_ptr() as *mut libc::c_void,
                        std::mem::size_of_val(&ev_buf),
                    )
                };
                if n <= 0 {
                    break;
                }
                let count = (n as usize) / std::mem::size_of::<InputEvent>();
                for ev in &ev_buf[..count] {
                    if ev.type_ == EV_KEY {
                        let pressed = ev.value != 0;
                        match ev.code {
                            KEY_LEFTCTRL => kbd_ctrl_l = pressed,
                            KEY_RIGHTCTRL => kbd_ctrl_r = pressed,
                            KEY_LEFTALT => kbd_alt_l = pressed,
                            KEY_RIGHTALT => kbd_alt_r = pressed,
                            KEY_SCROLLLOCK | KEY_F12 if ev.value == 1 => {
                                if (kbd_ctrl_l || kbd_ctrl_r) && (kbd_alt_l || kbd_alt_r) {
                                    panic_mode.store(true, Ordering::SeqCst);
                                    active_node.store(NodeId::Debian as u8, Ordering::SeqCst);
                                    vkbd.release_all_keys();
                                    for m in &mut mice {
                                        m.set_grab(false);
                                    }
                                    send_burst(&socket, peer_addr, &seq_counter, Payload::PanicRelease);
                                    println!("[PANIC] Local Fail-Safe Hotkey triggered!");
                                }
                            }
                            KEY_F11 if ev.value == 1 => {
                                if (kbd_ctrl_l || kbd_ctrl_r) && (kbd_alt_l || kbd_alt_r) {
                                    panic_mode.store(false, Ordering::SeqCst);
                                    let cur = active_node.load(Ordering::SeqCst);
                                    let next = if cur == NodeId::Debian as u8 {
                                        NodeId::Windows
                                    } else {
                                        NodeId::Debian
                                    };
                                    active_node.store(next as u8, Ordering::SeqCst);
                                    let grab = next == NodeId::Windows;
                                    if grab {
                                        vkbd.release_all_keys();
                                    }
                                    for m in &mut mice {
                                        m.set_grab(grab);
                                    }
                                    last_switch_instant = Instant::now();
                                    edge_push_accum = 0;
                                    send_burst(
                                        &socket,
                                        peer_addr,
                                        &seq_counter,
                                        Payload::FocusSwitch {
                                            target_node: next,
                                            cursor_y_ratio: 0.5,
                                        },
                                    );
                                    println!("[Debian] Manual focus toggle (Ctrl+Alt+F11) -> {}", next);
                                }
                            }
                            KEY_G if ev.value == 1 => {
                                if (kbd_ctrl_l || kbd_ctrl_r) && (kbd_alt_l || kbd_alt_r) {
                                    if let Ok(mut guard) = spatial_state.write() {
                                        guard.config.manual_game_lock =
                                            !guard.config.manual_game_lock;
                                        let locked = guard.config.manual_game_lock;
                                        guard.save();
                                        println!(
                                            "[Debian] Manual Game Captive Lock (Ctrl+Alt+G): {}",
                                            if locked { "LOCKED" } else { "UNLOCKED" }
                                        );
                                    }
                                }
                            }
                            _ => {
                                // If user types on physical Debian keyboard while Windows had focus,
                                // automatically bring focus back to Debian!
                                if ev.value == 1
                                    && active_node.load(Ordering::Relaxed) == NodeId::Windows as u8
                                {
                                    active_node.store(NodeId::Debian as u8, Ordering::SeqCst);
                                    for m in &mut mice {
                                        m.set_grab(false);
                                    }
                                    last_switch_instant = Instant::now();
                                    edge_push_accum = 0;
                                    send_burst(
                                        &socket,
                                        peer_addr,
                                        &seq_counter,
                                        Payload::FocusSwitch {
                                            target_node: NodeId::Debian,
                                            cursor_y_ratio: 0.5,
                                        },
                                    );
                                    println!(
                                        "[Debian] <-- Local Debian keyboard activity detected; focus switched to DEBIAN"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. Read physical mouse events and apply 2D Spatial Transition Engine
        let peer_online = last_peer_seen
            .map(|t| t.elapsed() <= Duration::from_millis(1600))
            .unwrap_or(false);

        // The laptop keyboard can vanish mid-keypress; don't leave those keys held on Debian
        if was_peer_online && !peer_online {
            vkbd.release_all_keys();
        }
        was_peer_online = peer_online;

        // On-The-Go Watchdog: If Windows was active and its heartbeat dropped for >1.6s, auto-restore Debian!
        if active_node.load(Ordering::Relaxed) == NodeId::Windows as u8
            && peer_reports_heartbeats
            && !peer_online
        {
            active_node.store(NodeId::Debian as u8, Ordering::SeqCst);
            for m in &mut mice {
                m.set_grab(false);
            }
            edge_push_accum = 0;
            println!(
                "[Debian] Windows laptop went offline/out-of-range -> Auto-released EVIOCGRAB and restored standalone Debian control."
            );
        }

        let is_game_captive = cfg_snapshot.manual_game_lock
            || (cfg_snapshot.auto_game_lock && kwin_game_active.load(Ordering::Relaxed));
        let mut switch_to_windows_entry: Option<(i32, i32)> = None;
        let mut switch_to_debian_entry: Option<(i32, i32)> = None;
        let is_windows_active = !panic_mode.load(Ordering::Relaxed)
            && active_node.load(Ordering::Relaxed) == NodeId::Windows as u8;

        for mouse in &mut mice {
            let mut acc_dx: i32 = 0;
            let mut acc_dy: i32 = 0;
            let mut acc_wheel_x: i32 = 0;
            let mut acc_wheel_y: i32 = 0;

            loop {
                let n = unsafe {
                    libc::read(
                        mouse.fd,
                        ev_buf.as_mut_ptr() as *mut libc::c_void,
                        std::mem::size_of_val(&ev_buf),
                    )
                };
                if n <= 0 {
                    break;
                }
                let count = (n as usize) / std::mem::size_of::<InputEvent>();
                for ev in &ev_buf[..count] {
                    match ev.type_ {
                        EV_REL => match ev.code {
                            REL_X => acc_dx += ev.value,
                            REL_Y => acc_dy += ev.value,
                            REL_WHEEL => acc_wheel_y += ev.value * 120,
                            REL_HWHEEL => acc_wheel_x += ev.value * 120,
                            _ => {}
                        },
                        EV_KEY => {
                            let pressed = ev.value != 0;
                            let btn_opt = match ev.code {
                                BTN_LEFT => Some((MOUSE_BTN_LEFT, 0)),
                                BTN_RIGHT => Some((MOUSE_BTN_RIGHT, 1)),
                                BTN_MIDDLE => Some((MOUSE_BTN_MIDDLE, 2)),
                                BTN_SIDE => Some((MOUSE_BTN_SIDE, 3)),
                                BTN_EXTRA => Some((MOUSE_BTN_EXTRA, 4)),
                                _ => None,
                            };
                            if let Some((btn_id, bit)) = btn_opt {
                                if pressed {
                                    mouse_buttons_mask |= 1 << bit;
                                    if panic_mode.load(Ordering::Relaxed) {
                                        panic_mode.store(false, Ordering::SeqCst);
                                    }
                                } else {
                                    mouse_buttons_mask &= !(1 << bit);
                                }

                                if is_windows_active {
                                    send_payload(
                                        &socket,
                                        peer_addr,
                                        &seq_counter,
                                        Payload::MouseButton {
                                            button: btn_id,
                                            pressed,
                                        },
                                    );
                                }
                            }
                        }
                        EV_SYN if ev.code == SYN_REPORT => {
                            if is_windows_active {
                                if acc_dx != 0 || acc_dy != 0 {
                                    frac_dx += acc_dx as f32 * cfg_snapshot.mouse_speed;
                                    frac_dy += acc_dy as f32 * cfg_snapshot.mouse_speed;
                                    let mut send_dx = frac_dx.trunc() as i32;
                                    let send_dy = frac_dy.trunc() as i32;
                                    frac_dx -= send_dx as f32;
                                    frac_dy -= send_dy as f32;

                                    win_cursor_x = (win_cursor_x + send_dx)
                                        .clamp(0, cfg_snapshot.win_w - 1);
                                    win_cursor_y = (win_cursor_y + send_dy)
                                        .clamp(0, cfg_snapshot.win_h - 1);

                                    // If Windows is NOT attached on Debian's Right edge, prevent the legacy
                                    // left-edge check on Windows (pt.x <= 0) from falsely firing when touching x=0
                                    if cfg_snapshot.attach_side != TouchSide::Right
                                        && win_cursor_x <= 6
                                        && send_dx < 0
                                    {
                                        send_dx = 0;
                                    }

                                    if send_dx != 0 || send_dy != 0 {
                                        send_payload(
                                            &socket,
                                            peer_addr,
                                            &seq_counter,
                                            Payload::MouseMoveDelta {
                                                dx: send_dx,
                                                dy: send_dy,
                                            },
                                        );
                                    }

                                    // Check 2D spatial boundary from Windows back to Debian (disabled when Game Captive Lock is active)
                                    if !is_game_captive
                                        && mouse_buttons_mask == 0
                                        && last_switch_instant.elapsed()
                                            >= Duration::from_millis(180)
                                    {
                                        if let Some(deb_target) = cfg_snapshot
                                            .check_windows_to_debian(
                                                win_cursor_x,
                                                win_cursor_y,
                                                acc_dx,
                                                acc_dy,
                                            )
                                        {
                                            edge_push_accum += acc_dx.abs() + acc_dy.abs();
                                            if edge_push_accum >= cfg_snapshot.edge_resistance {
                                                switch_to_debian_entry = Some(deb_target);
                                            }
                                        } else {
                                            edge_push_accum = 0;
                                        }
                                    }

                                    acc_dx = 0;
                                    acc_dy = 0;
                                }
                                if acc_wheel_x != 0 || acc_wheel_y != 0 {
                                    send_payload(
                                        &socket,
                                        peer_addr,
                                        &seq_counter,
                                        Payload::MouseWheel {
                                            delta_x: acc_wheel_x,
                                            delta_y: acc_wheel_y,
                                        },
                                    );
                                    acc_wheel_x = 0;
                                    acc_wheel_y = 0;
                                }
                            } else if !panic_mode.load(Ordering::Relaxed) {
                                // Active node is Debian: use ONLY true KWin Wayland cursor coordinates (zero raw-delta drift!)
                                if acc_dx != 0 || acc_dy != 0 {
                                    let real_cx = kwin_x.load(Ordering::Relaxed);
                                    let real_cy = kwin_y.load(Ordering::Relaxed);

                                    if peer_online
                                        && !is_game_captive
                                        && mouse_buttons_mask == 0
                                        && last_switch_instant.elapsed()
                                            >= Duration::from_millis(250)
                                    {
                                        if let Some(win_entry) = cfg_snapshot
                                            .check_debian_to_windows(
                                                real_cx, real_cy, acc_dx, acc_dy,
                                            )
                                        {
                                            edge_push_accum += acc_dx.abs() + acc_dy.abs();
                                            if edge_push_accum >= cfg_snapshot.edge_resistance {
                                                switch_to_windows_entry = Some(win_entry);
                                            }
                                        } else {
                                            edge_push_accum = 0;
                                        }
                                    }
                                    acc_dx = 0;
                                    acc_dy = 0;
                                }
                                acc_wheel_x = 0;
                                acc_wheel_y = 0;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Execute transition: Debian -> Windows
        if let Some((entry_wx, entry_wy)) = switch_to_windows_entry {
            active_node.store(NodeId::Windows as u8, Ordering::SeqCst);
            vkbd.release_all_keys();
            for m in &mut mice {
                m.set_grab(true);
            }
            last_switch_instant = Instant::now();
            edge_push_accum = 0;
            win_cursor_x = entry_wx;
            win_cursor_y = entry_wy;

            let cursor_y_ratio =
                (entry_wy as f32 / cfg_snapshot.win_h.max(1) as f32).clamp(0.01, 0.99);
            send_burst(
                &socket,
                peer_addr,
                &seq_counter,
                Payload::FocusSwitch {
                    target_node: NodeId::Windows,
                    cursor_y_ratio,
                },
            );

            // If entering at X != 24 (e.g., Top/Bottom/Left attachment), nudge Windows cursor horizontally to entry_wx
            if (entry_wx - 24).abs() > 4 {
                send_payload(
                    &socket,
                    peer_addr,
                    &seq_counter,
                    Payload::MouseMoveDelta {
                        dx: entry_wx - 24,
                        dy: 0,
                    },
                );
            }

            println!(
                "[Debian] --> Spatial transition ({:?}) to WINDOWS at ({}, {})",
                cfg_snapshot.attach_side, entry_wx, entry_wy
            );
        }

        // Execute transition: Windows -> Debian
        if let Some((target_deb_x, target_deb_y)) = switch_to_debian_entry {
            active_node.store(NodeId::Debian as u8, Ordering::SeqCst);
            for m in &mut mice {
                m.set_grab(false);
            }
            last_switch_instant = Instant::now();
            edge_push_accum = 0;

            vmouse.warp_abs(target_deb_x, target_deb_y);
            kwin_x.store(target_deb_x, Ordering::Relaxed);
            kwin_y.store(target_deb_y, Ordering::Relaxed);

            send_burst(
                &socket,
                peer_addr,
                &seq_counter,
                Payload::FocusSwitch {
                    target_node: NodeId::Debian,
                    cursor_y_ratio: 0.5,
                },
            );

            println!(
                "[Debian] <-- Spatial transition ({:?}) to DEBIAN at ({}, {})",
                cfg_snapshot.attach_side, target_deb_x, target_deb_y
            );
        }

        // Update live WebUI telemetry every 40ms
        if last_telemetry_sync.elapsed() >= Duration::from_millis(40) {
            last_telemetry_sync = Instant::now();
            if let Ok(mut guard) = spatial_state.write() {
                guard.telemetry.active_node =
                    if active_node.load(Ordering::Relaxed) == NodeId::Windows as u8 {
                        "Windows".to_string()
                    } else {
                        "Debian".to_string()
                    };
                guard.telemetry.panic_mode = panic_mode.load(Ordering::Relaxed);
                guard.telemetry.deb_cursor_x = kwin_x.load(Ordering::Relaxed);
                guard.telemetry.deb_cursor_y = kwin_y.load(Ordering::Relaxed);
                guard.telemetry.win_cursor_x = win_cursor_x;
                guard.telemetry.win_cursor_y = win_cursor_y;
                guard.telemetry.game_captive_active = is_game_captive;
                guard.telemetry.game_reason = if cfg_snapshot.manual_game_lock {
                    "Manual Lock (Ctrl+Alt+G)".to_string()
                } else if cfg_snapshot.auto_game_lock && kwin_game_active.load(Ordering::Relaxed) {
                    "Auto: Fullscreen / Game Active".to_string()
                } else {
                    String::new()
                };
                guard.telemetry.audio_rx_packets =
                    crate::audio::AUDIO_RX_PACKETS.load(Ordering::Relaxed);
                guard.telemetry.audio_rx_peak =
                    crate::audio::AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed);
                guard.telemetry.audio_muted =
                    crate::audio::AUDIO_RX_MUTED.load(Ordering::Relaxed);
                guard.telemetry.audio_volume =
                    crate::audio::AUDIO_RX_VOLUME_PCT.load(Ordering::Relaxed);
                guard.telemetry.audio_tx_enabled =
                    crate::audio::AUDIO_TX_ENABLED.load(Ordering::Relaxed);
                guard.telemetry.audio_tx_packets =
                    crate::audio::AUDIO_TX_PACKETS.load(Ordering::Relaxed);
                guard.telemetry.audio_tx_peak =
                    crate::audio::AUDIO_TX_PEAK_PCT.load(Ordering::Relaxed);
                guard.telemetry.audio_tx_volume =
                    crate::audio::AUDIO_TX_VOLUME_PCT.load(Ordering::Relaxed);
                guard.telemetry.audio_tx_source_mode =
                    crate::audio::AUDIO_TX_SOURCE_MODE.load(Ordering::Relaxed);
                guard.telemetry.peer_online = peer_online;
            }
        }

        // Periodic Heartbeat & State Sync (every 400ms)
        if last_heartbeat.elapsed() >= Duration::from_millis(400) {
            last_heartbeat = Instant::now();
            if !panic_mode.load(Ordering::Relaxed)
                && active_node.load(Ordering::Relaxed) == NodeId::Debian as u8
            {
                // Continuously ensure Windows peer knows Debian is the active node so keyboard hooks stay active
                send_payload(
                    &socket,
                    peer_addr,
                    &seq_counter,
                    Payload::FocusSwitch {
                        target_node: NodeId::Debian,
                        cursor_y_ratio: 0.5,
                    },
                );
            } else {
                send_payload(&socket, peer_addr, &seq_counter, Payload::Heartbeat);
            }
        }

        std::thread::sleep(Duration::from_micros(500));
    }
}

#[inline]
fn send_payload(
    socket: &UdpSocket,
    peer_addr: SocketAddr,
    seq_counter: &AtomicU64,
    payload: Payload,
) {
    let pkt = Packet::new(
        seq_counter.fetch_add(1, Ordering::Relaxed),
        NodeId::Debian,
        payload,
    );
    if let Ok(bytes) = pkt.encode() {
        let _ = socket.send_to(&bytes, peer_addr);
    }
}

#[inline]
fn send_burst(
    socket: &UdpSocket,
    peer_addr: SocketAddr,
    seq_counter: &AtomicU64,
    payload: Payload,
) {
    for _ in 0..3 {
        let pkt = Packet::new(
            seq_counter.fetch_add(1, Ordering::Relaxed),
            NodeId::Debian,
            payload.clone(),
        );
        if let Ok(bytes) = pkt.encode() {
            let _ = socket.send_to(&bytes, peer_addr);
        }
    }
}
