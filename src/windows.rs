#![cfg(target_os = "windows")]

use crate::keymap::win_vk_scan_to_evdev;
use crate::protocol::{
    NodeId, Packet, Payload, SwitchEdge, MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT, MOUSE_BTN_MIDDLE,
    MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
};
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, DESKTOPVERTRES,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VK_CONTROL, VK_F11, VK_F12, VK_LCONTROL, VK_LMENU, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_SCROLL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorInfo, GetCursorPos, GetMessageW, GetSystemMetrics,
    SetCursorPos, SetProcessDPIAware, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    CURSORINFO, CURSOR_SHOWING, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG,
    SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    XBUTTON1, XBUTTON2,
};

struct GlobalNet {
    socket: UdpSocket,
    peer_addr: SocketAddr,
}

static NET_STATE: OnceLock<GlobalNet> = OnceLock::new();
/// Starts as Windows (standalone laptop mode) until a live heartbeat/FocusSwitch arrives from Debian!
static ACTIVE_NODE: AtomicU8 = AtomicU8::new(NodeId::Windows as u8);
static PANIC_MODE: AtomicBool = AtomicBool::new(false);
static MANUAL_GAME_LOCK: AtomicBool = AtomicBool::new(false);
static SEQ_COUNTER: AtomicU64 = AtomicU64::new(1);
static LAST_PEER_SEEN_MS: AtomicU64 = AtomicU64::new(0);
static SCREEN_WIDTH: AtomicI32 = AtomicI32::new(1920);
static SCREEN_HEIGHT: AtomicI32 = AtomicI32::new(1080);

#[inline]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Returns true only if the Debian desktop has sent a UDP packet within the last 1,350 ms.
/// When the laptop is taken "on the go" (away from Debian / Wi-Fi disconnected / Debian powered off),
/// this automatically returns `false` so the laptop keyboard & trackpad work 100% natively!
#[inline]
fn is_debian_peer_online() -> bool {
    let last = LAST_PEER_SEEN_MS.load(Ordering::Relaxed);
    last > 0 && now_ms().saturating_sub(last) <= 1350
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

static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static ALT_DOWN: AtomicBool = AtomicBool::new(false);
static SWALLOWED_SCANCODES: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

#[inline]
fn send_udp_payload(payload: Payload) {
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

    let startup_dir = PathBuf::from(&appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup");

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

unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code == HC_ACTION as i32 && l_param != 0 {
        let kb = &*(l_param as *const KBDLLHOOKSTRUCT);

        // Ignore software-injected events so we never create feedback loops
        if (kb.flags & LLKHF_INJECTED) != 0 {
            return CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param);
        }

        let msg = w_param as u32;
        let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let released = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        if pressed || released {
            let vk = kb.vkCode as u16;
            let extended = (kb.flags & LLKHF_EXTENDED) != 0;

            if vk == VK_CONTROL || vk == VK_LCONTROL || vk == VK_RCONTROL {
                CTRL_DOWN.store(pressed, Ordering::Relaxed);
            }
            if vk == VK_MENU || vk == VK_LMENU || vk == VK_RMENU {
                ALT_DOWN.store(pressed, Ordering::Relaxed);
            }

            // Fail-Safe / Panic Hotkey: Ctrl + Alt + ScrollLock OR Ctrl + Alt + F12
            if pressed
                && CTRL_DOWN.load(Ordering::Relaxed)
                && ALT_DOWN.load(Ordering::Relaxed)
                && (vk == VK_SCROLL || vk == VK_F12)
            {
                PANIC_MODE.store(true, Ordering::SeqCst);
                ACTIVE_NODE.store(NodeId::Windows as u8, Ordering::SeqCst);
                for _ in 0..3 {
                    send_udp_payload(Payload::PanicRelease);
                }
                println!(
                    "[PANIC] Fail-Safe Hotkey (Ctrl+Alt+ScrollLock/F12) triggered! Local Windows keyboard restored and Debian EVIOCGRAB released."
                );
                return CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param);
            }

            // Manual Game Captive Lock Hotkey: Ctrl + Alt + G (0x47)
            if pressed
                && CTRL_DOWN.load(Ordering::Relaxed)
                && ALT_DOWN.load(Ordering::Relaxed)
                && vk == 0x47
                && is_debian_peer_online()
            {
                let prev = MANUAL_GAME_LOCK.load(Ordering::Relaxed);
                MANUAL_GAME_LOCK.store(!prev, Ordering::SeqCst);
                println!(
                    "[Windows] Game Captive Lock (Ctrl+Alt+G): {}",
                    if !prev { "LOCKED" } else { "UNLOCKED" }
                );
                return 1;
            }

            // Manual Focus Toggle Hotkey: Ctrl + Alt + F11
            if pressed
                && CTRL_DOWN.load(Ordering::Relaxed)
                && ALT_DOWN.load(Ordering::Relaxed)
                && vk == VK_F11
                && is_debian_peer_online()
            {
                PANIC_MODE.store(false, Ordering::SeqCst);
                let cur = ACTIVE_NODE.load(Ordering::SeqCst);
                let next = if cur == NodeId::Debian as u8 {
                    NodeId::Windows
                } else {
                    NodeId::Debian
                };
                ACTIVE_NODE.store(next as u8, Ordering::SeqCst);
                for _ in 0..3 {
                    send_udp_payload(Payload::FocusSwitch {
                        target_node: next,
                        cursor_y_ratio: 0.5,
                    });
                }
                println!("[Windows] Manual focus toggle (Ctrl+Alt+F11) -> {}", next);
                return 1;
            }

            if let Some(scancode) = win_vk_scan_to_evdev(kb.vkCode, kb.scanCode, extended) {
                let idx = (scancode as usize).min(255);
                // On-The-Go Safety: Only forward & swallow keys if Debian peer is actively online!
                let is_debian_focused = !PANIC_MODE.load(Ordering::Relaxed)
                    && ACTIVE_NODE.load(Ordering::Relaxed) == NodeId::Debian as u8
                    && is_debian_peer_online();

                if is_debian_focused {
                    if pressed {
                        SWALLOWED_SCANCODES[idx].store(true, Ordering::Relaxed);
                        send_udp_payload(Payload::KeyEvent {
                            scancode,
                            pressed: true,
                        });
                        return 1; // Swallow keystroke locally on Windows!
                    } else {
                        let was_swallowed =
                            SWALLOWED_SCANCODES[idx].swap(false, Ordering::Relaxed);
                        send_udp_payload(Payload::KeyEvent {
                            scancode,
                            pressed: false,
                        });
                        if was_swallowed {
                            return 1;
                        }
                    }
                } else if released && SWALLOWED_SCANCODES[idx].swap(false, Ordering::Relaxed) {
                    // Key was pressed while Debian had focus and released after switching to Windows
                    send_udp_payload(Payload::KeyEvent {
                        scancode,
                        pressed: false,
                    });
                }
            }
        }
    }

    CallNextHookEx(ptr::null_mut(), n_code, w_param, l_param)
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
    let width = override_width.unwrap_or(det_w);
    let height = override_height.unwrap_or(det_h);

    SCREEN_WIDTH.store(width, Ordering::SeqCst);
    SCREEN_HEIGHT.store(height, Ordering::SeqCst);

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

    let _ = NET_STATE.set(GlobalNet {
        socket,
        peer_addr,
    });

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

    // Spawn periodic heartbeat & liveness watchdog thread (every 400ms)
    std::thread::Builder::new()
        .name("mesh-kvm-win-watchdog".to_string())
        .spawn(move || {
            let mut was_online = false;
            loop {
                std::thread::sleep(Duration::from_millis(400));
                let online = is_debian_peer_online();
                if !online && was_online {
                    was_online = false;
                    ACTIVE_NODE.store(NodeId::Windows as u8, Ordering::SeqCst);
                    println!(
                        "[Windows] Debian peer disconnected/out-of-range -> Switched to Standalone Laptop Mode."
                    );
                } else if online && !was_online {
                    was_online = true;
                    println!("[Windows] Debian peer online -> Mesh KVM link active.");
                }
                send_udp_payload(Payload::Heartbeat);
                send_udp_payload(Payload::PeerDisplayInfo {
                    width: SCREEN_WIDTH.load(Ordering::Relaxed),
                    height: SCREEN_HEIGHT.load(Ordering::Relaxed),
                });
            }
        })
        .ok();

    // Spawn high-priority UDP receiver and mouse injection / edge-transition worker thread
    std::thread::Builder::new()
        .name("mesh-kvm-win-udp".to_string())
        .spawn(move || {
            let mut buf = [0u8; 512];
            let mut last_switch_instant = Instant::now() - Duration::from_secs(1);
            let mut last_rx_focus_seq = 0u64;
            let mut buttons_held_mask: u8 = 0;

            loop {
                match rx_socket.recv_from(&mut buf) {
                    Ok((len, _src_addr)) => {
                        if let Ok(pkt) = Packet::decode(&buf[..len]) {
                            LAST_PEER_SEEN_MS.store(now_ms(), Ordering::Relaxed);
                            match pkt.payload {
                                Payload::MouseMoveDelta { dx, dy } => {
                                    if !PANIC_MODE.load(Ordering::Relaxed)
                                        && ACTIVE_NODE.load(Ordering::Relaxed)
                                            == NodeId::Windows as u8
                                    {
                                        inject_mouse_move(dx, dy);

                                        // Check cursor position for screen-edge transition back to Debian
                                        let mut pt = POINT { x: 0, y: 0 };
                                        if unsafe { GetCursorPos(&mut pt) } != 0 {
                                            let w = SCREEN_WIDTH.load(Ordering::Relaxed);
                                            let h = SCREEN_HEIGHT.load(Ordering::Relaxed);

                                            let hit_edge = match switch_edge {
                                                SwitchEdge::Left => dx < 0 && pt.x <= 0,
                                                SwitchEdge::Right => dx > 0 && pt.x >= w - 1,
                                                SwitchEdge::Top => dy < 0 && pt.y <= 0,
                                                SwitchEdge::Bottom => dy > 0 && pt.y >= h - 1,
                                            };

                                            if hit_edge
                                                && !is_windows_game_captive()
                                                && buttons_held_mask == 0
                                                && last_switch_instant.elapsed()
                                                    >= Duration::from_millis(150)
                                            {
                                                ACTIVE_NODE
                                                    .store(NodeId::Debian as u8, Ordering::SeqCst);
                                                last_switch_instant = Instant::now();
                                                let cursor_y_ratio =
                                                    (pt.y as f32 / h as f32).clamp(0.0, 1.0);

                                                // Step Windows cursor slightly away from edge
                                                let safe_x = match switch_edge {
                                                    SwitchEdge::Left => 24.min(w - 1),
                                                    SwitchEdge::Right => (w - 24).max(0),
                                                    _ => pt.x,
                                                };
                                                let safe_y = match switch_edge {
                                                    SwitchEdge::Top => 24.min(h - 1),
                                                    SwitchEdge::Bottom => (h - 24).max(0),
                                                    _ => pt.y,
                                                };
                                                unsafe {
                                                    SetCursorPos(safe_x, safe_y);
                                                }

                                                let switch_pkt = Packet::new(
                                                    SEQ_COUNTER.fetch_add(1, Ordering::Relaxed),
                                                    NodeId::Windows,
                                                    Payload::FocusSwitch {
                                                        target_node: NodeId::Debian,
                                                        cursor_y_ratio,
                                                    },
                                                );
                                                if let Ok(bytes) = switch_pkt.encode() {
                                                    for _ in 0..3 {
                                                        let _ =
                                                            rx_socket.send_to(&bytes, peer_addr);
                                                    }
                                                }
                                                println!(
                                                    "[Windows] <-- Edge transition ({})! Focus switched to DEBIAN (y_ratio: {:.3})",
                                                    switch_edge, cursor_y_ratio
                                                );
                                            }
                                        }
                                    }
                                }
                                Payload::MouseButton { button, pressed } => {
                                    let bit = button.saturating_sub(1).min(7);
                                    if pressed {
                                        buttons_held_mask |= 1 << bit;
                                    } else {
                                        buttons_held_mask &= !(1 << bit);
                                    }

                                    if !PANIC_MODE.load(Ordering::Relaxed)
                                        && ACTIVE_NODE.load(Ordering::Relaxed)
                                            == NodeId::Windows as u8
                                    {
                                        inject_mouse_button(button, pressed);
                                    }
                                }
                                Payload::MouseWheel { delta_x, delta_y } => {
                                    if !PANIC_MODE.load(Ordering::Relaxed)
                                        && ACTIVE_NODE.load(Ordering::Relaxed)
                                            == NodeId::Windows as u8
                                    {
                                        inject_mouse_wheel(delta_x, delta_y);
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
                                    PANIC_MODE.store(false, Ordering::SeqCst);

                                    let prev =
                                        ACTIVE_NODE.swap(target_node as u8, Ordering::SeqCst);
                                    last_switch_instant = Instant::now();

                                    if target_node == NodeId::Windows {
                                        let w = SCREEN_WIDTH.load(Ordering::Relaxed);
                                        let h = SCREEN_HEIGHT.load(Ordering::Relaxed);
                                        let target_y = ((cursor_y_ratio * h as f32) as i32)
                                            .clamp(0, h - 1);
                                        let (entry_x, entry_y) = match switch_edge {
                                            SwitchEdge::Left => (24.min(w - 1), target_y),
                                            SwitchEdge::Right => ((w - 24).max(0), target_y),
                                            SwitchEdge::Top => (w / 2, 24.min(h - 1)),
                                            SwitchEdge::Bottom => (w / 2, (h - 24).max(0)),
                                        };
                                        unsafe {
                                            SetCursorPos(entry_x, entry_y);
                                        }
                                        if prev != NodeId::Windows as u8 {
                                            println!(
                                                "[Windows] --> Focus switched to WINDOWS (entry cursor: {}, {})",
                                                entry_x, entry_y
                                            );
                                        }
                                    } else if prev != NodeId::Debian as u8 {
                                        println!("[Windows] <-- Focus switched to DEBIAN");
                                    }

                                    send_udp_payload(Payload::FocusAck {
                                        active_node: target_node,
                                        seq_ref: pkt.seq,
                                    });
                                    send_udp_payload(Payload::PeerDisplayInfo {
                                        width: SCREEN_WIDTH.load(Ordering::Relaxed),
                                        height: SCREEN_HEIGHT.load(Ordering::Relaxed),
                                    });
                                }
                                Payload::Heartbeat => {
                                    send_udp_payload(Payload::PeerDisplayInfo {
                                        width: SCREEN_WIDTH.load(Ordering::Relaxed),
                                        height: SCREEN_HEIGHT.load(Ordering::Relaxed),
                                    });
                                }
                                Payload::PanicRelease => {
                                    PANIC_MODE.store(true, Ordering::SeqCst);
                                    ACTIVE_NODE.store(NodeId::Windows as u8, Ordering::SeqCst);
                                    println!(
                                        "[PANIC] Received PanicRelease from Debian! Local keyboard unlocked."
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        })
        .map_err(|e| format!("Failed to spawn UDP thread: {}", e))?;

    // Install WH_KEYBOARD_LL hook on main thread and run Win32 message loop
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            ptr::null_mut(),
            0,
        )
    };
    if hook.is_null() {
        return Err("SetWindowsHookExW(WH_KEYBOARD_LL) failed".to_string());
    }
    println!("[Windows] Installed WH_KEYBOARD_LL low-level keyboard hook.");

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(hook);
    }

    Ok(())
}
