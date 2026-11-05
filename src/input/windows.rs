#![cfg(target_os = "windows")]

use super::win_logic::{self, HookGate, Hotkey, HotkeyFilter, KeyRoute, OsModifiers, KEY_SLOTS};
use super::{apply, fresh_focus_seq, from_peer, InputBackend, PeerSender};
use crate::keymap::{evdev_to_win_scan, win_vk_scan_to_evdev};
use crate::protocol::{
    NodeId, Packet, Payload, SwitchEdge, MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT, MOUSE_BTN_MIDDLE,
    MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
};
use crate::router::{
    Action, DeviceId, EdgeSpan, EdgeTracker, Event, Routed, Router, RouterConfig, Sharing,
    KEEP_CURSOR,
};
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, DESKTOPVERTRES,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_CONTROL, VK_MENU,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorInfo, GetCursorPos, GetMessageW, GetSystemMetrics,
    SetCursorPos, SetProcessDPIAware, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    CURSORINFO, CURSOR_SHOWING, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE, WM_SYSKEYDOWN, WM_SYSKEYUP, XBUTTON1, XBUTTON2,
};

// `win_logic` restates these so that it builds on Linux.
const _: () = {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as kbm;
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    assert!(win_logic::WM_MOUSEMOVE == wm::WM_MOUSEMOVE);
    assert!(win_logic::WM_LBUTTONDOWN == wm::WM_LBUTTONDOWN);
    assert!(win_logic::WM_LBUTTONUP == wm::WM_LBUTTONUP);
    assert!(win_logic::WM_RBUTTONDOWN == wm::WM_RBUTTONDOWN);
    assert!(win_logic::WM_RBUTTONUP == wm::WM_RBUTTONUP);
    assert!(win_logic::WM_MBUTTONDOWN == wm::WM_MBUTTONDOWN);
    assert!(win_logic::WM_MBUTTONUP == wm::WM_MBUTTONUP);
    assert!(win_logic::WM_MOUSEWHEEL == wm::WM_MOUSEWHEEL);
    assert!(win_logic::WM_XBUTTONDOWN == wm::WM_XBUTTONDOWN);
    assert!(win_logic::WM_XBUTTONUP == wm::WM_XBUTTONUP);
    assert!(win_logic::WM_MOUSEHWHEEL == wm::WM_MOUSEHWHEEL);
    assert!(win_logic::XBUTTON1 == wm::XBUTTON1 as u32);
    assert!(win_logic::XBUTTON2 == wm::XBUTTON2 as u32);
    assert!(win_logic::KEYEVENTF_EXTENDEDKEY == kbm::KEYEVENTF_EXTENDEDKEY);
    assert!(win_logic::KEYEVENTF_KEYUP == kbm::KEYEVENTF_KEYUP);
    assert!(win_logic::KEYEVENTF_SCANCODE == kbm::KEYEVENTF_SCANCODE);
};

const ME: DeviceId = NodeId::Windows as DeviceId;
const PEER: DeviceId = NodeId::Debian as DeviceId;
const TICK_INTERVAL: Duration = Duration::from_millis(100);
/// Heartbeats go out every this many ticks, which is 400 ms.
const TICKS_PER_HEARTBEAT: u32 = 4;
/// How long after a focus change the cursor must wait before it can cross back.
const EDGE_SETTLE_MS: u64 = 250;
/// Receive poll gaps. The socket is non-blocking, and a laptop on battery has no peer to hurry for.
const RX_IDLE_LINKED: Duration = Duration::from_millis(1);
const RX_IDLE_ALONE: Duration = Duration::from_millis(10);

struct GlobalNet {
    socket: UdpSocket,
    peer_addr: SocketAddr,
}

struct EdgeState {
    span: Option<EdgeSpan>,
    tracker: EdgeTracker,
}

static NET_STATE: OnceLock<GlobalNet> = OnceLock::new();
static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
/// Filled by `EdgeMap`. Until one arrives no edge touches Debian.
static EDGE: Mutex<EdgeState> =
    Mutex::new(EdgeState { span: None, tracker: EdgeTracker::new(0) });
static HOTKEYS: Mutex<HotkeyFilter> = Mutex::new(HotkeyFilter::new());
static MANUAL_GAME_LOCK: AtomicBool = AtomicBool::new(false);
static SEQ_COUNTER: AtomicU64 = AtomicU64::new(1);
/// Buttons injected for the peer and not yet released.
static BUTTONS_HELD_MASK: AtomicU8 = AtomicU8::new(0);
/// Keys injected for the peer and not yet released, by evdev code.
static KEYS_HELD: [AtomicBool; KEY_SLOTS] = [const { AtomicBool::new(false) }; KEY_SLOTS];
/// Buttons physically down on this laptop, whatever the router did with them.
static LOCAL_BUTTONS_MASK: AtomicU8 = AtomicU8::new(0);
static SCREEN_WIDTH: AtomicI32 = AtomicI32::new(1920);
static SCREEN_HEIGHT: AtomicI32 = AtomicI32::new(1080);
/// The cursor is parked at `pin_position` while the trackpad drives the peer.
static MOUSE_PINNED: AtomicBool = AtomicBool::new(false);
/// Last known cursor position, clamped to the screen.
static LAST_X: AtomicI32 = AtomicI32::new(0);
static LAST_Y: AtomicI32 = AtomicI32::new(0);
static FOCUS_CHANGED_MS: AtomicU64 = AtomicU64::new(0);

/// Monotonic on purpose: with the wall clock, a backwards jump (NTP resync after waking away
/// from home) makes the peer look online until the clock catches up, swallowing the keyboard.
#[inline]
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn router_lock() -> &'static Mutex<Router> {
    ROUTER.get_or_init(|| {
        Mutex::new(Router::new(RouterConfig {
            me: ME,
            peer: PEER,
            main: PEER,
            sharing: Sharing { mouse: true, keyboard: true },
            pointer_speed: 1.0,
        }))
    })
}

/// The shared router, for reading its state. Anything that decides what happens to input uses
/// `win_logic::lock_live` instead, so that a poisoned router fails open.
///
/// Hold either guard only to call the router: never across `SendInput`, `SetCursorPos`, a socket
/// send or `println!`. `SendInput` waits for the hook thread, and the hook thread may be waiting
/// for this lock.
fn router() -> MutexGuard<'static, Router> {
    router_lock().lock().unwrap_or_else(PoisonError::into_inner)
}

fn edge_state() -> MutexGuard<'static, EdgeState> {
    EDGE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Feeds events to the router, then carries out their actions outside the lock. Returns the
/// actions, and the last event's `swallow`. A poisoned router swallows nothing and does nothing.
fn route_events(events: &[Event]) -> Routed {
    let mut routed = Routed::default();
    {
        let Some(mut router) = win_logic::lock_live(router_lock()) else {
            return Routed::default();
        };
        let before = router.active();
        for &event in events {
            let step = router.handle(event);
            routed.swallow = step.swallow;
            routed.actions.extend(step.actions);
        }
        if router.active() != before {
            FOCUS_CHANGED_MS.store(now_ms(), Ordering::SeqCst);
        }
    }
    apply(&routed.actions, &mut WindowsBackend, &mut UdpPeer);
    let may_have_raced = routed.actions.iter().any(|a| {
        matches!(
            a,
            Action::SetBlocking { .. }
                | Action::InjectKey { pressed: true, .. }
                | Action::InjectButton { pressed: true, .. }
        )
    });
    if may_have_raced {
        settle_races();
    }
    routed
}

fn route(event: Event) -> Routed {
    route_events(&[event])
}

/// For the hooks. The extra tick means a press finds out about a dead link by itself, so the
/// laptop's own keyboard does not depend on the watchdog thread still running.
fn route_local(event: Event) -> Routed {
    route_events(&[Event::Tick { now_ms: now_ms() }, event])
}

/// The hook, UDP, watchdog and webui threads all apply actions outside the lock, so actions can
/// land out of order: a key press injected just after another thread's `ReleaseHeld` would stick,
/// and a stale `SetBlocking` would leave the pin wrong. Whoever applies last corrects it here
/// against the router, and the watchdog does so every tick. A poisoned router counts as neither.
fn settle_races() {
    let (remote, pin_mouse, injecting) = match win_logic::lock_live(router_lock()) {
        Some(router) => {
            let live = router.online() && !router.panicked();
            let remote = live && router.active() != ME;
            (remote, remote && router.shares_mouse(), live && router.active() == ME)
        }
        None => (false, false, false),
    };
    WindowsBackend.set_blocking(pin_mouse, remote);
    if !injecting {
        WindowsBackend.release_held();
    }
}

/// Returns true if a game on Windows is holding the mouse captive (either manual lock Ctrl+Alt+G
/// or a 3D game hiding/locking the hardware cursor via CURSOR_SHOWING == 0).
#[inline]
fn is_windows_game_captive() -> bool {
    if MANUAL_GAME_LOCK.load(Ordering::Relaxed) {
        return true;
    }
    unsafe {
        let mut ci: CURSORINFO = std::mem::zeroed();
        ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if GetCursorInfo(&mut ci) != 0 && (ci.flags & CURSOR_SHOWING) == 0 {
            return true;
        }
    }
    false
}

/// Safe from a hook: the socket is non-blocking, and a failed send is dropped.
///
/// Every packet leaves through here. A poisoned router sends nothing, heartbeats included, so
/// that Debian's link timeout takes its input back rather than forwarding to a frozen laptop.
#[inline]
fn send_udp_payload(payload: Payload) {
    if router_lock().is_poisoned() {
        return;
    }
    if let Some(net) = NET_STATE.get() {
        let pkt = Packet::new(
            SEQ_COUNTER.fetch_add(1, Ordering::Relaxed),
            NodeId::Windows,
            payload,
        );
        if let Ok(bytes) = pkt.encode() {
            let _ = net.socket.send_to(&bytes, net.peer_addr);
        }
    }
}

struct UdpPeer;

impl PeerSender for UdpPeer {
    fn send(&mut self, payload: Payload) {
        send_udp_payload(payload);
    }
}

fn screen_size() -> (i32, i32) {
    (SCREEN_WIDTH.load(Ordering::Relaxed), SCREEN_HEIGHT.load(Ordering::Relaxed))
}

/// The screen centre: a cursor pinned on an edge would report no outward movement.
fn pin_position() -> (i32, i32) {
    let (w, h) = screen_size();
    (w / 2, h / 2)
}

fn cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut pt) } != 0).then_some((pt.x, pt.y))
}

fn remember_cursor(pt: (i32, i32)) {
    let (w, h) = screen_size();
    let (x, y) = win_logic::clamp_to_screen(pt, w, h);
    LAST_X.store(x, Ordering::Relaxed);
    LAST_Y.store(y, Ordering::Relaxed);
}

/// A handle only: the hook, UDP, watchdog and webui threads all use it, so its state is in statics.
struct WindowsBackend;

impl InputBackend for WindowsBackend {
    /// Only the mouse needs anything here. Each hook call asks the router whether to swallow.
    fn set_blocking(&mut self, mouse: bool, _keyboard: bool) {
        let was_pinned = MOUSE_PINNED.swap(mouse, Ordering::SeqCst);
        if mouse && !was_pinned {
            let (x, y) = pin_position();
            unsafe {
                SetCursorPos(x, y);
            }
        } else if !mouse && was_pinned {
            if let Some(pt) = cursor_pos() {
                remember_cursor(pt);
            }
        }
    }

    fn inject_move(&mut self, dx: i32, dy: i32) {
        inject_mouse_move(dx, dy);
    }

    fn inject_button(&mut self, button: u8, pressed: bool) {
        // Recorded first, so a release racing this call can always see it.
        let update = |mask| Some(win_logic::button_mask(mask, button, pressed));
        if let Ok(prev) = BUTTONS_HELD_MASK.try_update(Ordering::SeqCst, Ordering::SeqCst, update) {
            // A repeated release changes nothing, so it sends nothing.
            if !pressed && win_logic::button_mask(prev, button, false) == prev {
                return;
            }
        }
        inject_mouse_button(button, pressed);
    }

    fn inject_wheel(&mut self, dx: i32, dy: i32) {
        inject_mouse_wheel(dx, dy);
    }

    fn inject_key(&mut self, scancode: u16, pressed: bool) {
        let Some((scan, extended)) = evdev_to_win_scan(scancode) else {
            return;
        };
        // Recorded first, so a release racing this call can always see it.
        if let Some(held) = KEYS_HELD.get(scancode as usize) {
            // A repeated release changes nothing, so it sends nothing.
            if !held.swap(pressed, Ordering::SeqCst) && !pressed {
                return;
            }
        }
        send_key(scan, extended, pressed);
    }

    fn warp_cursor(&mut self, x: i32, y: i32) {
        if x == KEEP_CURSOR || y == KEEP_CURSOR {
            return;
        }
        unsafe {
            SetCursorPos(x, y);
        }
        remember_cursor((x, y));
    }

    fn release_held(&mut self) {
        for (code, held) in KEYS_HELD.iter().enumerate() {
            if held.swap(false, Ordering::SeqCst) {
                if let Some((scan, extended)) = evdev_to_win_scan(code as u16) {
                    send_key(scan, extended, false);
                }
            }
        }
        let held = BUTTONS_HELD_MASK.swap(0, Ordering::SeqCst);
        for bit in (0..8u8).filter(|b| held & (1 << b) != 0) {
            inject_mouse_button(bit + 1, false);
        }
    }
}

fn startup_vbs_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("mesh-kvm-autostart.vbs"),
    )
}

/// Present when the user turned auto-boot off in the Studio, so startup doesn't reinstall it.
fn autostart_disabled_marker() -> PathBuf {
    crate::settings::config_dir().join("autostart-disabled")
}

fn set_windows_autostart(enabled: bool, peer_addr: SocketAddr, switch_edge: SwitchEdge) {
    let marker = autostart_disabled_marker();
    if enabled {
        let _ = fs::remove_file(&marker);
        ensure_windows_autorun_installed(peer_addr, switch_edge);
    } else {
        let _ = fs::create_dir_all(crate::settings::config_dir());
        let _ = fs::write(&marker, "");
        if let Some(vbs) = startup_vbs_path() {
            let _ = fs::remove_file(vbs);
        }
    }
}

/// Builds the Windows node's Studio API handler (plugged into the shared `webui` server).
/// Layout and focus live on the Debian master; the UI reaches them through the peer proxy.
fn studio_handler(peer_addr: SocketAddr, switch_edge: SwitchEdge) -> crate::webui::Handler {
    let peer_ip = peer_addr.ip().to_string();
    Arc::new(move |req: &crate::webui::HttpRequest| -> Option<String> {
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method.as_str(), path) {
            ("GET", "/api/local") => {
                let mut pt = POINT { x: 0, y: 0 };
                unsafe {
                    GetCursorPos(&mut pt);
                }
                let (peer_online, active_node, panic_mode) = {
                    let router = router();
                    (router.online(), router.active(), router.panicked())
                };
                let active = if active_node == PEER { "Debian" } else { "Windows" };
                let manual = MANUAL_GAME_LOCK.load(Ordering::Relaxed);
                let captive = is_windows_game_captive();
                let reason = if manual {
                    "Manual lock"
                } else if captive {
                    "Game is holding the cursor"
                } else {
                    ""
                };
                let (w, h) = screen_size();
                Some(
                    serde_json::json!({
                        "node": "windows",
                        "label": "Windows Laptop",
                        "hostname": crate::webui::hostname(),
                        "ip": crate::webui::local_ip_toward(&peer_ip),
                        "peer_ip": peer_ip,
                        "version": env!("CARGO_PKG_VERSION"),
                        "os": "Windows 11",
                        "peer_online": peer_online,
                        "active_node": active,
                        "panic_mode": panic_mode,
                        "game_lock": captive,
                        "game_reason": reason,
                        "displays": [{ "name": "Laptop display", "x": 0, "y": 0, "w": w, "h": h }],
                        "cursor": [pt.x, pt.y],
                        "autostart": startup_vbs_path().is_some_and(|p| p.exists()),
                        "input_devices": ["Laptop keyboard", "Laptop trackpad"],
                        "audio": crate::settings::audio_status_json(false),
                    })
                    .to_string(),
                )
            }
            ("POST", "/api/local/action") => {
                let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
                let act = val.get("action").and_then(|v| v.as_str()).unwrap_or("");
                if !crate::settings::handle_common_action(act, &val) {
                    match act {
                        "set_autostart" => set_windows_autostart(
                            val.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                            peer_addr,
                            switch_edge,
                        ),
                        "panic" => {
                            route(Event::Panic);
                        }
                        "toggle_game_lock" => {
                            MANUAL_GAME_LOCK.fetch_xor(true, Ordering::SeqCst);
                        }
                        _ => return Some(r#"{"ok":false,"error":"unknown action"}"#.to_string()),
                    }
                }
                Some(r#"{"ok":true}"#.to_string())
            }
            _ => None,
        }
    })
}

/// Installs `mesh-kvm.exe` into `%LOCALAPPDATA%\mesh-kvm\mesh-kvm.exe` and creates a silent
/// windowless Startup launcher in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\mesh-kvm-autostart.vbs`
/// so the Windows 11 laptop automatically starts `mesh-kvm` in the background on every boot/login.
fn ensure_windows_autorun_installed(peer_addr: SocketAddr, switch_edge: SwitchEdge) {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return;
    };
    let local_appdata = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| appdata.clone());
    let install_dir = PathBuf::from(&local_appdata).join("mesh-kvm");
    let _ = fs::create_dir_all(&install_dir);
    let installed_exe = install_dir.join("mesh-kvm.exe");

    if let Ok(cur_exe) = std::env::current_exe() {
        if cur_exe != installed_exe {
            let _ = fs::copy(&cur_exe, &installed_exe);
        }
    }

    let exe_to_run = if installed_exe.exists() {
        installed_exe
    } else if let Ok(cur_exe) = std::env::current_exe() {
        cur_exe
    } else {
        return;
    };

    let programs_dir = PathBuf::from(&appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs");

    // Start Menu entry for the Studio UI (the daemon itself runs windowless)
    let _ = fs::write(
        programs_dir.join("mesh-kvm Studio.vbs"),
        format!(
            "Set WshShell = CreateObject(\"WScript.Shell\")\r\n\
             WshShell.Run \"\"\"{}\"\" --studio\", 0, False\r\n",
            exe_to_run.to_string_lossy()
        ),
    );

    if autostart_disabled_marker().exists() {
        return;
    }

    let startup_dir = programs_dir.join("Startup");
    if startup_dir.exists() {
        let vbs_path = startup_dir.join("mesh-kvm-autostart.vbs");
        let peer_ip = peer_addr.ip();
        let vbs_content = format!(
            "Set WshShell = CreateObject(\"WScript.Shell\")\r\n\
             WshShell.Run \"\"\"{}\"\" --role windows --peer {} --switch-edge {}\", 0, False\r\n",
            exe_to_run.to_string_lossy(),
            peer_ip,
            switch_edge
        );
        if fs::write(&vbs_path, vbs_content).is_ok() {
            println!(
                "[Windows] Auto-Boot installed: '{}' -> launches '{}' silently at login.",
                vbs_path.to_string_lossy(),
                exe_to_run.to_string_lossy()
            );
        }
    }
}

fn hotkeys() -> MutexGuard<'static, HotkeyFilter> {
    HOTKEYS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn os_key_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// True if the hook must hand this event straight to the OS, because the router is poisoned.
fn hook_fails_open() -> bool {
    let open = win_logic::hook_gate(router_lock().is_poisoned()) == HookGate::PassToOs;
    if open {
        MOUSE_PINNED.store(false, Ordering::SeqCst);
    }
    open
}

/// Handles one physical key. Returns true to keep it from the OS.
fn local_key(code: u16, pressed: bool) -> bool {
    let link_up = router().online();
    let os = OsModifiers { ctrl: os_key_down(VK_CONTROL), alt: os_key_down(VK_MENU) };
    let key_route = hotkeys().feed(code, pressed, link_up, os);
    match key_route {
        // Panic stays with the OS: restoring the keyboard must not depend on anything else.
        KeyRoute::Hotkey(Hotkey::Panic) => {
            route(Event::Panic);
            false
        }
        KeyRoute::Hotkey(Hotkey::ManualSwitch) => {
            route_local(Event::ManualSwitch);
            true
        }
        KeyRoute::Hotkey(Hotkey::GameLock) => {
            MANUAL_GAME_LOCK.fetch_xor(true, Ordering::SeqCst);
            true
        }
        KeyRoute::ConsumedSwallowed => true,
        KeyRoute::ConsumedToOs => false,
        KeyRoute::Route => {
            let swallow = route_local(Event::LocalKey { scancode: code, pressed }).swallow;
            if swallow {
                hotkeys().note_swallowed(code);
            }
            swallow
        }
    }
}

unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if hook_fails_open() {
        return CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param);
    }

    if n_code == HC_ACTION as i32 && l_param != 0 {
        let kb = &*(l_param as *const KBDLLHOOKSTRUCT);
        let msg = w_param as u32;
        let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let released = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        // Ignore software-injected events so we never create feedback loops
        if (kb.flags & LLKHF_INJECTED) == 0 && (pressed || released) {
            let extended = (kb.flags & LLKHF_EXTENDED) != 0;
            if let Some(code) = win_vk_scan_to_evdev(kb.vkCode, kb.scanCode, extended) {
                if local_key(code, pressed) {
                    return 1;
                }
            }
        }
    }

    CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param)
}

/// Counts a cursor at `(x, y)` that just moved by `(dx, dy)` against the edge touching Debian,
/// and crosses when the push completes. Returns true if focus left this laptop.
fn push_edge(x: i32, y: i32, dx: i32, dy: i32) -> bool {
    let may_cross = {
        let router = router();
        router.online() && !router.panicked() && !router.captive() && router.active() == ME
    };
    let settled =
        now_ms().saturating_sub(FOCUS_CHANGED_MS.load(Ordering::SeqCst)) >= EDGE_SETTLE_MS;
    if !may_cross || !settled {
        return false;
    }
    let (w, h) = screen_size();
    let buttons_held = BUTTONS_HELD_MASK.load(Ordering::SeqCst) != 0
        || LOCAL_BUTTONS_MASK.load(Ordering::SeqCst) != 0;
    let pushed_through = {
        let mut edge = edge_state();
        match edge.span {
            Some(span) => edge.tracker.push(span, x, y, w, h, dx, dy, buttons_held),
            None => false,
        }
    };
    pushed_through
        && route(Event::EdgeCrossed { x, y })
            .actions
            .iter()
            .any(|a| matches!(a, Action::SendFocus { .. }))
}

/// Handles physical pointer movement to `pt`. Returns true to keep it from the OS.
fn local_move(pt: (i32, i32)) -> bool {
    let pinned = MOUSE_PINNED.load(Ordering::SeqCst);
    let pin = pinned.then(pin_position);
    let cursor = if pinned { cursor_pos() } else { None };
    let last = (LAST_X.load(Ordering::Relaxed), LAST_Y.load(Ordering::Relaxed));
    let Some((dx, dy)) = win_logic::move_offset(pt, last, pin, cursor) else {
        return false;
    };

    if route_local(Event::LocalMove { dx, dy }).swallow {
        // Something moved the cursor off the pin. Put it back: blocked moves leave it there.
        if let (Some((x, y)), true) = (pin, cursor != pin) {
            unsafe {
                SetCursorPos(x, y);
            }
        }
        return true;
    }

    remember_cursor(pt);
    if pinned {
        // Blocking ended just now, so the offset was measured from the pin, not a real move.
        return false;
    }
    let (w, h) = screen_size();
    let (x, y) = win_logic::clamp_to_screen(pt, w, h);
    // On a crossing the event must not reach the OS, or it would drag the cursor off the pin.
    push_edge(x, y, dx, dy)
}

/// Handles one physical mouse event. Returns true to keep it from the OS.
fn local_mouse(msg: u32, pt: (i32, i32), mouse_data: u32) -> bool {
    if msg == WM_MOUSEMOVE {
        return local_move(pt);
    }
    if let Some((dx, dy)) = win_logic::wheel_delta(msg, mouse_data) {
        return route_local(Event::LocalWheel { dx, dy }).swallow;
    }
    if let Some((button, pressed)) = win_logic::mouse_button_event(msg, mouse_data) {
        let update = |mask| Some(win_logic::button_mask(mask, button, pressed));
        let _ = LOCAL_BUTTONS_MASK.try_update(Ordering::SeqCst, Ordering::SeqCst, update);
        return route_local(Event::LocalButton { button, pressed }).swallow;
    }
    false
}

unsafe extern "system" fn mouse_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if hook_fails_open() {
        return CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param);
    }

    if n_code == HC_ACTION as i32 && l_param != 0 {
        let ms = &*(l_param as *const MSLLHOOKSTRUCT);
        let pt = (ms.pt.x, ms.pt.y);
        if (ms.flags & LLMHF_INJECTED) != 0 {
            // Not routed, but it does move the cursor, and local offsets are measured from there.
            if w_param as u32 == WM_MOUSEMOVE {
                remember_cursor(pt);
            }
        } else if local_mouse(w_param as u32, pt, ms.mouseData) {
            return 1;
        }
    }

    CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param)
}

#[inline]
fn send_key(scan: u16, extended: bool, pressed: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan,
                dwFlags: win_logic::key_flags(extended, pressed),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

#[inline]
fn inject_mouse_move(dx: i32, dy: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

#[inline]
fn inject_mouse_button(button: u8, pressed: bool) {
    let (flags, mouse_data) = match (button, pressed) {
        (MOUSE_BTN_LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MOUSE_BTN_LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MOUSE_BTN_RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MOUSE_BTN_RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MOUSE_BTN_MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MOUSE_BTN_MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MOUSE_BTN_SIDE, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
        (MOUSE_BTN_SIDE, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
        (MOUSE_BTN_EXTRA, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
        (MOUSE_BTN_EXTRA, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        _ => return,
    };

    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

#[inline]
fn inject_mouse_wheel(delta_x: i32, delta_y: i32) {
    if delta_y != 0 {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta_y as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
        }
    }
    if delta_x != 0 {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta_x as u32,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

pub fn run_windows_node(
    peer_addr: SocketAddr,
    listen_port: u16,
    switch_edge: SwitchEdge,
    override_width: Option<i32>,
    override_height: Option<i32>,
) -> Result<(), String> {
    unsafe {
        let _ = SetProcessDPIAware();
    }
    let (phys_w, phys_h) = unsafe {
        let hdc = GetDC(ptr::null_mut());
        if !hdc.is_null() {
            let pw = GetDeviceCaps(hdc, DESKTOPHORZRES as i32);
            let ph = GetDeviceCaps(hdc, DESKTOPVERTRES as i32);
            ReleaseDC(ptr::null_mut(), hdc);
            (pw, ph)
        } else {
            (0, 0)
        }
    };
    let sm_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(100);
    let sm_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(100);
    let det_w = phys_w.max(sm_w).max(100);
    let det_h = phys_h.max(sm_h).max(100);
    let width = override_width.unwrap_or(det_w).max(100);
    let height = override_height.unwrap_or(det_h).max(100);

    SCREEN_WIDTH.store(width, Ordering::SeqCst);
    SCREEN_HEIGHT.store(height, Ordering::SeqCst);
    if let Some(pt) = cursor_pos() {
        remember_cursor(pt);
    }

    // Ensure auto-boot on Windows login is installed
    ensure_windows_autorun_installed(peer_addr, switch_edge);

    let init_seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    SEQ_COUNTER.store(init_seq, Ordering::SeqCst);

    let bind_addr: SocketAddr = format!("0.0.0.0:{}", listen_port).parse().unwrap();
    let socket = UdpSocket::bind(bind_addr)
        .map_err(|e| format!("Failed to bind UDP {}: {}", bind_addr, e))?;
    let rx_socket = socket
        .try_clone()
        .map_err(|e| format!("Failed to clone UDP socket: {}", e))?;
    // The hooks send on this socket, and must never block. Both handles are set, as the mode may
    // be shared between them. Sends stay on the listening port: a stateful firewall may be
    // letting Debian's packets in only as replies to that port.
    for handle in [&socket, &rx_socket] {
        handle
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to make UDP socket non-blocking: {}", e))?;
    }

    let _ = NET_STATE.set(GlobalNet {
        socket,
        peer_addr,
    });

    crate::webui::start(
        peer_addr.ip().to_string(),
        studio_handler(peer_addr, switch_edge),
    )?;

    // Report true native Windows display resolution to Debian peer immediately
    for _ in 0..3 {
        send_udp_payload(Payload::PeerDisplayInfo { width, height });
    }

    println!(
        "[Windows] Native display resolution: {}x{}, switch_edge: {}, peer: {}",
        width, height, switch_edge, peer_addr
    );
    println!(
        "[Windows] On-The-Go Watchdog enabled: If Debian peer is unreachable (>1.3s), laptop operates 100% standalone."
    );

    // Spawn the link and game-captive watchdog (every 100ms), which also sends heartbeats.
    // State changes are logged here because the hooks must not print.
    std::thread::Builder::new()
        .name("mesh-kvm-win-watchdog".to_string())
        .spawn(move || {
            let mut captive = false;
            let mut logged = None;
            let mut ticks = 0u32;
            loop {
                std::thread::sleep(TICK_INTERVAL);
                route(Event::Tick { now_ms: now_ms() });
                settle_races();

                let now_captive = is_windows_game_captive();
                if now_captive != captive {
                    captive = now_captive;
                    route(Event::SetCaptive(captive));
                }

                let status = {
                    let router = router();
                    (router.online(), router.active(), router.panicked(), router.captive())
                };
                if logged != Some(status) {
                    logged = Some(status);
                    let (online, active, panicked, captive) = status;
                    let active = if active == PEER { "DEBIAN" } else { "WINDOWS" };
                    println!(
                        "[Windows] Link {}, active {}, panic {}, game lock {}",
                        if online { "up" } else { "down" },
                        active,
                        panicked,
                        captive
                    );
                }

                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(TICKS_PER_HEARTBEAT) {
                    send_udp_payload(Payload::Heartbeat);
                    send_udp_payload(Payload::PeerDisplayInfo { width, height });
                }
            }
        })
        .map_err(|e| format!("Failed to spawn watchdog thread: {}", e))?;

    // Spawn the UDP receiver, which feeds peer packets to the router
    std::thread::Builder::new()
        .name("mesh-kvm-win-udp".to_string())
        .spawn(move || {
            let mut buf = [0u8; 512];
            let mut last_rx_focus_seq = 0u64;

            loop {
                match rx_socket.recv_from(&mut buf) {
                    Ok((len, src_addr)) => {
                        if !from_peer(src_addr, peer_addr) {
                            continue;
                        }
                        let Ok(pkt) = Packet::decode(&buf[..len]) else {
                            continue;
                        };
                        route(Event::PeerAlive { now_ms: now_ms() });
                        match pkt.payload {
                            Payload::MouseMoveDelta { dx, dy } => {
                                let routed = route(Event::PeerMove { dx, dy });
                                let injected = routed
                                    .actions
                                    .iter()
                                    .any(|a| matches!(a, Action::InjectMove { .. }));
                                if injected {
                                    if let Some((x, y)) = cursor_pos() {
                                        push_edge(x, y, dx, dy);
                                    }
                                }
                            }
                            Payload::MouseButton { button, pressed } => {
                                route(Event::PeerButton { button, pressed });
                            }
                            Payload::MouseWheel { delta_x, delta_y } => {
                                route(Event::PeerWheel { dx: delta_x, dy: delta_y });
                            }
                            Payload::KeyEvent { scancode, pressed } => {
                                route(Event::PeerKey { scancode, pressed });
                            }
                            Payload::FocusSwitchAt { target_node, x, y } => {
                                if fresh_focus_seq(&mut last_rx_focus_seq, pkt.seq) {
                                    route(Event::PeerFocus {
                                        target: target_node as DeviceId,
                                        x,
                                        y,
                                    });
                                }
                            }
                            Payload::ActiveView { active_node } => {
                                route(Event::PeerActiveView { active: active_node as DeviceId });
                            }
                            Payload::GameLock { locked } => {
                                route(Event::PeerGameLock(locked));
                            }
                            Payload::EdgeMap { edge, start, end, resistance } => {
                                let mut state = edge_state();
                                state.span = Some(EdgeSpan { edge, start, end });
                                state.tracker.set_resistance(resistance);
                            }
                            Payload::Heartbeat => {
                                send_udp_payload(Payload::PeerDisplayInfo { width, height });
                            }
                            Payload::PanicRelease => {
                                println!(
                                    "[PANIC] Received PanicRelease from Debian! Local keyboard unlocked."
                                );
                                route(Event::PeerPanic);
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {
                        let linked = router().online();
                        std::thread::sleep(if linked { RX_IDLE_LINKED } else { RX_IDLE_ALONE });
                    }
                }
            }
        })
        .map_err(|e| format!("Failed to spawn UDP thread: {}", e))?;

    // Install both low-level hooks on this thread and run the Win32 message loop
    let keyboard_hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            ptr::null_mut(),
            0,
        )
    };
    if keyboard_hook.is_null() {
        return Err("SetWindowsHookExW(WH_KEYBOARD_LL) failed".to_string());
    }
    println!("[Windows] Installed WH_KEYBOARD_LL low-level keyboard hook.");

    let mouse_hook =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), ptr::null_mut(), 0) };
    if mouse_hook.is_null() {
        unsafe {
            UnhookWindowsHookEx(keyboard_hook);
        }
        return Err("SetWindowsHookExW(WH_MOUSE_LL) failed".to_string());
    }
    println!("[Windows] Installed WH_MOUSE_LL low-level mouse hook.");

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(mouse_hook);
        UnhookWindowsHookEx(keyboard_hook);
    }

    Ok(())
}
