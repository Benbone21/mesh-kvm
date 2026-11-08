#![cfg(target_os = "windows")]

use super::win_logic::{self, HookGate, Hotkey, HotkeyFilter, KeyRoute, OsModifiers, KEY_SLOTS};
use super::link::{press_event, MotionBatch, PeerLink, PeerVersion, PressLog, REPORT_COPIES};
use super::{apply, InputBackend, PeerSender, PeerSocket};
use crate::keymap::{evdev_to_win_scan, win_vk_scan_to_evdev};
use crate::protocol::{
    NodeId, Packet, Payload, PressEvent, MAX_DATAGRAM, MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT,
    MOUSE_BTN_MIDDLE, MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
};
use crate::router::{Action, DeviceId, EdgeSpan, Event, Routed, Router, RouterConfig, KEEP_CURSOR};
use crate::secure::datagram::Channel;
use crate::spatial::{take_studio_requests, EdgeWatch, MonitorRect, SharedSpatialState};
use std::net::{SocketAddr, UdpSocket};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, DESKTOPVERTRES,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_CONTROL, VK_MENU,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorInfo, GetCursorPos,
    GetMessageW, GetSystemMetrics, PostQuitMessage, RegisterClassW, SetCursorPos,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, CURSORINFO,
    CURSOR_SHOWING, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, SM_CXSCREEN,
    SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_CLOSE, WM_ENDSESSION, WM_KEYDOWN, WM_KEYUP,
    WM_MOUSEMOVE, WM_POWERBROADCAST, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, XBUTTON1, XBUTTON2,
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

static NET_STATE: OnceLock<PeerSocket> = OnceLock::new();
static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
/// Filled by `EdgeMap`, and from this laptop's layout until one arrives.
static EDGE: Mutex<EdgeWatch> = Mutex::new(EdgeWatch::new());
/// This device's copy of the layout. The hooks use it only through `try_read`.
static SPATIAL: OnceLock<Arc<RwLock<SharedSpatialState>>> = OnceLock::new();
/// A copy of this laptop's monitors, for the hooks.
static MONITORS: Mutex<Vec<MonitorRect>> = Mutex::new(Vec::new());
static HOTKEYS: Mutex<HotkeyFilter> = Mutex::new(HotkeyFilter::new());
/// A copy of the layout's manual game lock, for the hooks.
static MANUAL_GAME_LOCK: AtomicBool = AtomicBool::new(false);
/// A copy of the layout's automatic game lock, for the hooks.
static AUTO_GAME_LOCK: AtomicBool = AtomicBool::new(true);
/// The hotkey was pressed. The watchdog changes the layout, as the hook must not wait for it.
static GAME_LOCK_HOTKEY: AtomicBool = AtomicBool::new(false);
static SEQ_COUNTER: AtomicU64 = AtomicU64::new(1);
/// Shared by the UDP and watchdog threads. The hooks never touch it.
static LINK: LazyLock<Mutex<PeerLink>> = LazyLock::new(Mutex::default);
/// The hooks number their presses here, so it is only ever held to do that.
static PRESSES: LazyLock<Mutex<PressLog>> = LazyLock::new(Mutex::default);
/// Movement from the mouse hook, gathered so that it goes at most once a `MOTION_GAP_MS`. Held while
/// sending, so that what it lets out keeps its place on the wire.
static MOTION: LazyLock<Mutex<MotionBatch>> = LazyLock::new(Mutex::default);
/// Buttons injected for the peer and not yet released.
static BUTTONS_HELD_MASK: AtomicU8 = AtomicU8::new(0);
/// Keys injected for the peer and not yet released, by evdev code.
static KEYS_HELD: [AtomicBool; KEY_SLOTS] = [const { AtomicBool::new(false) }; KEY_SLOTS];
/// Buttons physically down on this laptop, whatever the router did with them.
static LOCAL_BUTTONS_MASK: AtomicU8 = AtomicU8::new(0);
/// The main monitor, where the cursor is parked.
static SCREEN_WIDTH: AtomicI32 = AtomicI32::new(1920);
static SCREEN_HEIGHT: AtomicI32 = AtomicI32::new(1080);
/// The box around every monitor, which can start at negative coordinates.
static DESKTOP_X: AtomicI32 = AtomicI32::new(0);
static DESKTOP_Y: AtomicI32 = AtomicI32::new(0);
static DESKTOP_W: AtomicI32 = AtomicI32::new(1920);
static DESKTOP_H: AtomicI32 = AtomicI32::new(1080);
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
            main: crate::settings::main_device(),
            sharing: crate::settings::sharing(),
            pointer_speed: crate::settings::pointer_speed(),
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

fn link() -> MutexGuard<'static, PeerLink> {
    LINK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn presses() -> MutexGuard<'static, PressLog> {
    PRESSES.lock().unwrap_or_else(PoisonError::into_inner)
}

fn motion() -> MutexGuard<'static, MotionBatch> {
    MOTION.lock().unwrap_or_else(PoisonError::into_inner)
}

fn edge_state() -> MutexGuard<'static, EdgeWatch> {
    EDGE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn monitors() -> MutexGuard<'static, Vec<MonitorRect>> {
    MONITORS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Copies this laptop's monitors for the hooks, when they have changed.
fn sync_monitors(latest: &[MonitorRect]) {
    let mut current = monitors();
    if current.as_slice() == latest {
        return;
    }
    *current = latest.to_vec();
    let (Some(left), Some(top)) = (latest.iter().map(|m| m.x).min(), latest.iter().map(|m| m.y).min()) else {
        return;
    };
    let right = latest.iter().map(|m| m.x + m.w).max().unwrap_or(left);
    let bottom = latest.iter().map(|m| m.y + m.h).max().unwrap_or(top);
    DESKTOP_X.store(left, Ordering::SeqCst);
    DESKTOP_Y.store(top, Ordering::SeqCst);
    DESKTOP_W.store(right - left, Ordering::SeqCst);
    DESKTOP_H.store(bottom - top, Ordering::SeqCst);
}

/// The closest point to `pt` in the box around every monitor.
fn clamp_to_desktop(pt: (i32, i32)) -> (i32, i32) {
    let (left, top) = (DESKTOP_X.load(Ordering::Relaxed), DESKTOP_Y.load(Ordering::Relaxed));
    let (w, h) = (DESKTOP_W.load(Ordering::Relaxed), DESKTOP_H.load(Ordering::Relaxed));
    let (x, y) = win_logic::clamp_to_screen((pt.0 - left, pt.1 - top), w, h);
    (x + left, y + top)
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

fn route_presses(presses: &[PressEvent]) {
    let events: Vec<Event> = presses.iter().map(|event| press_event(*event)).collect();
    route_events(&events);
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
    let settled = win_logic::lock_live(router_lock())
        .map(|router| win_logic::settled_input(&router, ME))
        .unwrap_or_default();
    WindowsBackend.set_blocking(settled.block_mouse, settled.block_keyboard);
    if !settled.injecting {
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
    if !AUTO_GAME_LOCK.load(Ordering::Relaxed) {
        return false;
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
/// Every packet of input leaves through here. A poisoned router sends none, heartbeats included, so
/// that Debian's link timeout takes its input back rather than forwarding to a frozen laptop. What
/// the channel sends by itself doesn't hold the link up.
#[inline]
fn send_udp_payload(payload: Payload) {
    let mut motion = motion();
    for payload in motion.send(payload, now_ms()) {
        send_numbered(SEQ_COUNTER.fetch_add(1, Ordering::Relaxed), payload);
    }
}

/// Movement that waited out the gap. The hooks only add to it, so the UDP thread lets it out.
fn send_due_motion() {
    let mut motion = motion();
    if let Some(payload) = motion.due(now_ms()) {
        send_numbered(SEQ_COUNTER.fetch_add(1, Ordering::Relaxed), payload);
    }
}

fn send_numbered(seq: u64, payload: Payload) {
    if router_lock().is_poisoned() {
        return;
    }
    if let Some(net) = NET_STATE.get() {
        let pkt = Packet::new(seq, NodeId::Windows, payload);
        if let Ok(bytes) = pkt.encode() {
            net.send(&bytes);
        }
    }
}

/// The hooks forward input while this runs. The numbers are taken before the router is read, so a
/// press that this report can't know of is always newer than the report, and Debian won't take the
/// report's word over it. Reading first would let a press slip in between and be let go again.
fn send_link_state() {
    let tail = presses().tail(now_ms());
    if let Some(tail) = tail {
        send_udp_payload(tail);
    }
    let order = presses().newest();
    let seq = SEQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let (held, panicked, main) = {
        let router = router();
        (router.held_forwarded(), router.panicked(), router.main() == ME)
    };
    let state = link().next_state(held, panicked, main, order, now_ms());
    send_numbered(seq, Payload::LinkState(state));
    for _ in 1..REPORT_COPIES {
        send_udp_payload(Payload::LinkState(state));
    }
}

/// Lets go of any modifier waiting to go back that Windows says is up. A release can go unseen by
/// the hook, on the secure desktop say, and restoring that would hold Ctrl down on the peer. A key
/// the hook swallowed never reached Windows, so it counts as up unless auto-repeat got through while
/// the link was down. That just means it stays here, which is the safe way round.
fn drop_unheld_modifiers() {
    let carried = router().carried_modifiers();
    for scancode in carried {
        if win_logic::modifier_vk(scancode).is_some_and(|vk| !os_key_down(vk)) {
            route(Event::LocalKey { scancode, pressed: false });
        }
    }
}

struct UdpPeer;

impl PeerSender for UdpPeer {
    fn send(&mut self, payload: Payload) {
        send_udp_payload(payload);
    }

    fn number_press(&mut self, event: PressEvent) -> Option<Payload> {
        presses().record(event, now_ms())
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
    let (x, y) = clamp_to_desktop(pt);
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
            GAME_LOCK_HOTKEY.store(true, Ordering::SeqCst);
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
/// and crosses when the push completes. Returns true if focus left this laptop. `from_peer` says
/// Debian's input moved the cursor.
fn push_edge(x: i32, y: i32, dx: i32, dy: i32, from_peer: bool) -> bool {
    let may_cross = {
        let router = router();
        router.online() && !router.panicked() && !router.captive() && router.active() == ME
    };
    let settled =
        now_ms().saturating_sub(FOCUS_CHANGED_MS.load(Ordering::SeqCst)) >= EDGE_SETTLE_MS;
    if !may_cross || !settled {
        return false;
    }
    let buttons_held = BUTTONS_HELD_MASK.load(Ordering::SeqCst) != 0
        || LOCAL_BUTTONS_MASK.load(Ordering::SeqCst) != 0;
    let pushed_through = {
        let monitors = monitors();
        edge_state().push(&monitors, (x, y), (dx, dy), buttons_held)
    };
    if !pushed_through {
        return false;
    }
    // Where the cursor lands. If the layout is busy, the next push crosses instead.
    let entry = SPATIAL.get().and_then(|state| {
        let state = state.try_read().ok()?;
        Some(state.config.entry_on_peer(NodeId::Windows, x, y))
    });
    let Some((x, y)) = entry else {
        return false;
    };
    route(Event::EdgeCrossed { x, y, from_peer })
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
    let (x, y) = clamp_to_desktop(pt);
    // On a crossing the event must not reach the OS, or it would drag the cursor off the pin.
    push_edge(x, y, dx, dy, false)
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

/// A logoff or shutdown is the laptop's clean exit, and sleep is the link going away unseen. They
/// are only announced to top-level windows, so the service keeps a hidden one. Ending the process
/// from Task Manager announces nothing; the tray cleans up after that.
unsafe extern "system" fn session_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Zero means the session isn't ending after all.
        WM_ENDSESSION if wparam != 0 => {
            crate::mute::shutdown();
            0
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            0
        }
        WM_POWERBROADCAST => {
            match wparam as u32 {
                PBT_APMSUSPEND => crate::mute::before_sleep(),
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => crate::mute::after_sleep(),
                _ => {}
            }
            1
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Lets go of every key and button this device is holding down or keeping from Windows, before
/// the service ends.
pub fn let_go() {
    WindowsBackend.set_blocking(false, false);
    WindowsBackend.release_held();
}

/// Lets go of everything, then replaces this process with a fresh service, which reads the saved
/// address. Ending the process also removes the hooks.
fn restart_service() -> ! {
    WindowsBackend.set_blocking(false, false);
    WindowsBackend.release_held();
    crate::mute::shutdown();
    let problem = crate::app::launch::restart_service();
    crate::settings::log_fatal(&format!("mesh-kvm couldn't start itself again: {problem}"));
    std::process::exit(1);
}

/// Runs on the thread with the message loop. Failing only loses the unmute at logoff and sleep.
fn create_session_window() {
    let name: Vec<u16> = "mesh-kvm-session\0".encode_utf16().collect();
    let created = unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class = WNDCLASSW {
            lpfnWndProc: Some(session_window_proc),
            hInstance: instance,
            lpszClassName: name.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            0,
            name.as_ptr(),
            name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if created.is_null() {
        crate::settings::log_note("Couldn't create the session window, so logging off or sleeping won't unmute this device.");
    }
}

pub fn run_windows_node(
    spatial_state: Arc<RwLock<SharedSpatialState>>,
    peer_addr: SocketAddr,
    channel: Arc<Channel>,
    listen_port: u16,
    override_width: Option<i32>,
    override_height: Option<i32>,
) -> Result<(), String> {
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
    if let Ok(state) = spatial_state.read() {
        sync_monitors(state.config.monitors(NodeId::Windows));
    }
    let _ = SPATIAL.set(Arc::clone(&spatial_state));
    if let Some(pt) = cursor_pos() {
        remember_cursor(pt);
    }

    let init_seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    SEQ_COUNTER.store(init_seq, Ordering::SeqCst);

    let bind_addr: SocketAddr = format!("0.0.0.0:{}", listen_port).parse().unwrap();
    // A service that replaces another may find the old one still letting go of this port.
    let socket = crate::app::launch::bind_patiently(|| UdpSocket::bind(bind_addr))
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

    let _ = NET_STATE.set(PeerSocket::new(socket, peer_addr, Arc::clone(&channel)));
    let rx_peer = PeerSocket::new(rx_socket, peer_addr, channel);

    crate::mute::start();

    // Ensure auto-boot on Windows login is installed. By now this process holds the API port, so
    // a process that loses the race to another service doesn't rewrite the launchers.
    crate::platform::windows::ensure_autorun_installed();
    crate::update::note_service_up();

    println!("[Windows] Native display resolution: {}x{}, peer: {}", width, height, peer_addr);
    println!(
        "[Windows] On-The-Go Watchdog enabled: If Debian peer is unreachable (>1.3s), laptop operates 100% standalone."
    );

    let watchdog_state = Arc::clone(&spatial_state);
    // Spawn the link and game-captive watchdog (every 100ms), which also sends heartbeats.
    // State changes are logged here because the hooks must not print.
    std::thread::Builder::new()
        .name("mesh-kvm-win-watchdog".to_string())
        .spawn(move || {
            let mut captive = false;
            let mut logged = None;
            let mut ticks = 0u32;
            let mut config = None;
            loop {
                std::thread::sleep(TICK_INTERVAL);
                if crate::settings::restart_requested() {
                    restart_service();
                }
                if let Some(mut router) = win_logic::lock_live(router_lock()) {
                    router.set_pointer_speed(crate::settings::pointer_speed());
                }
                for event in crate::settings::take_router_events() {
                    route(event);
                }
                route(Event::Tick { now_ms: now_ms() });
                settle_races();

                if GAME_LOCK_HOTKEY.swap(false, Ordering::SeqCst) {
                    if let Ok(mut state) = watchdog_state.write() {
                        state.config.manual_game_lock = !state.config.manual_game_lock;
                        state.save();
                    }
                }
                // If the lock is busy, these wait for the next tick.
                if let Some(requests) = take_studio_requests(&watchdog_state) {
                    if requests.panic {
                        route(Event::Panic);
                    }
                    if requests.manual_switch {
                        route(Event::ManualSwitch);
                    }
                    MANUAL_GAME_LOCK.store(requests.config.manual_game_lock, Ordering::SeqCst);
                    AUTO_GAME_LOCK.store(requests.config.auto_game_lock, Ordering::SeqCst);
                    sync_monitors(requests.config.monitors(NodeId::Windows));
                    config = Some(requests.config);
                }

                let now_captive = is_windows_game_captive();
                if now_captive != captive {
                    captive = now_captive;
                    route(Event::SetCaptive(captive));
                }

                let status = {
                    let router = router();
                    let main = router.main();
                    (router.online(), router.active(), router.panicked(), router.captive(), main)
                };
                crate::audio::AUDIO_PEER_ONLINE.store(status.0, Ordering::Relaxed);
                link().set_online(status.0);
                let version = link().peer_version(now_ms());
                let mut log = presses();
                log.set_reliable(version == PeerVersion::Same);
                if !status.0 {
                    log.forget();
                }
                drop(log);
                if logged != Some(status) {
                    logged = Some(status);
                    let (online, active, panicked, captive, main) = status;
                    let name = |device| if device == PEER { "DEBIAN" } else { "WINDOWS" };
                    println!(
                        "[Windows] Link {}, active {}, main {}, panic {}, game lock {}",
                        if online { "up" } else { "down" },
                        name(active),
                        name(main),
                        panicked,
                        captive
                    );
                }

                if let Ok(mut state) = watchdog_state.try_write() {
                    let (online, active, panicked, _, _) = status;
                    let manual = MANUAL_GAME_LOCK.load(Ordering::Relaxed);
                    let telemetry = &mut state.telemetry;
                    telemetry.active_node = if active == PEER { "Debian" } else { "Windows" }.to_string();
                    telemetry.panic_mode = panicked;
                    telemetry.peer_online = online;
                    {
                        let router = router();
                        telemetry.main_conflict = router.main_conflict();
                        telemetry.link = link().quality(&router, now_ms());
                    }
                    telemetry.game_captive_active = captive;
                    telemetry.game_reason = match (manual, captive) {
                        (true, _) => "Manual lock",
                        (false, true) => "Game is holding the cursor",
                        (false, false) => "",
                    }
                    .to_string();
                    if let Some((x, y)) = cursor_pos() {
                        telemetry.cursor_x = x;
                        telemetry.cursor_y = y;
                    }
                }

                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(TICKS_PER_HEARTBEAT) {
                    match &config {
                        Some(config) => {
                            edge_state().use_own_layout(config, NodeId::Windows);
                            for payload in crate::spatial::heartbeat_payloads(config, NodeId::Windows) {
                                send_udp_payload(payload);
                            }
                        }
                        None => send_udp_payload(Payload::Heartbeat),
                    }
                    send_link_state();
                }
            }
        })
        .map_err(|e| format!("Failed to spawn watchdog thread: {}", e))?;

    // Spawn the UDP receiver, which feeds peer packets to the router
    std::thread::Builder::new()
        .name("mesh-kvm-win-udp".to_string())
        .spawn(move || {
            let mut buf = [0u8; MAX_DATAGRAM];
            let mut opened = Vec::new();
            let mut handshake_known = None;

            // Handshakes happen on this thread and no other. It comes round every poll gap at the
            // longest, and every packet when they're coming fast.
            loop {
                rx_peer.tick(now_ms());
                if let Some(at) = rx_peer.handshake_news(&mut handshake_known) {
                    link().handshake_heard(at);
                }
                // The hooks send presses and movement, and this loop sends what had to wait.
                send_due_motion();
                let repeat = presses().repeat(now_ms());
                if let Some(repeat) = repeat {
                    send_udp_payload(repeat);
                }
                match rx_peer.socket.recv_from(&mut buf) {
                    Ok((len, src_addr)) => {
                        let note = &mut |what| link().note(what, now_ms());
                        let Some(pkt) = rx_peer.open(&buf[..len], src_addr, now_ms(), &mut opened, note) else {
                            continue;
                        };
                        route(Event::PeerAlive { now_ms: now_ms() });
                        if !link().arrived(&pkt, now_ms()) {
                            continue;
                        }
                        match pkt.payload {
                            Payload::MouseMoveDelta { dx, dy } => {
                                let routed = route(Event::PeerMove { dx, dy });
                                let injected = routed
                                    .actions
                                    .iter()
                                    .any(|a| matches!(a, Action::InjectMove { .. }));
                                if injected {
                                    if let Some((x, y)) = cursor_pos() {
                                        push_edge(x, y, dx, dy, true);
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
                                let (x, y) = spatial_state
                                    .read()
                                    .map_or((x, y), |s| s.config.place_on_own(NodeId::Windows, x, y));
                                route(Event::PeerFocus {
                                    target: target_node as DeviceId,
                                    x,
                                    y,
                                });
                            }
                            Payload::ActiveView { active_node } => {
                                drop_unheld_modifiers();
                                route(Event::PeerActiveView { active: active_node as DeviceId });
                            }
                            Payload::GameLock { locked } => {
                                route(Event::PeerGameLock(locked));
                            }
                            Payload::EdgeMap { edge, start, end, resistance } => {
                                edge_state().set_span(EdgeSpan { edge, start, end }, resistance);
                            }
                            Payload::PeerDisplayInfo { name, monitors } => {
                                crate::spatial::note_peer_displays(&spatial_state, &name, monitors);
                            }
                            Payload::LayoutRevision { revision, editor } => {
                                crate::spatial::note_peer_revision(&spatial_state, revision, editor);
                            }
                            Payload::LinkState(state) => {
                                drop_unheld_modifiers();
                                let events = link().heard_state(&state, now_ms());
                                route_events(&events);
                            }
                            Payload::Presses { newest, sent_ms, events } => {
                                let ready = link().take_presses(newest, sent_ms, &events, now_ms());
                                route_presses(&ready);
                            }
                            Payload::PanicRelease => {
                                println!(
                                    "[PANIC] Received PanicRelease from the peer! Local keyboard unlocked."
                                );
                                route(Event::PeerPanic);
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {
                        let ready = link().due_presses(now_ms());
                        route_presses(&ready);
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

    create_session_window();

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
