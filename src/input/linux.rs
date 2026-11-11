#![cfg(target_os = "linux")]

use crate::keymap::{
    KEY_F11, KEY_F12, KEY_G, KEY_LEFTALT, KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_SCROLLLOCK,
};
use super::link::{came_up, press_event, HeartbeatPace, MotionBatch, PeerLink, PeerVersion, PressLog, REPORT_COPIES};
use super::grab::{hand_back_press, wait_for_releases, GrabNews, GrabOps, GrabSlot};
use super::{apply, InputBackend, PeerSender, PeerSocket};
use crate::protocol::{
    HeldInput, NodeId, Packet, Payload, PressEvent, MAX_DATAGRAM, MOUSE_BTN_EXTRA, MOUSE_BTN_LEFT,
    MOUSE_BTN_MIDDLE, MOUSE_BTN_RIGHT, MOUSE_BTN_SIDE,
};
use crate::router::{Action, DeviceId, EdgeSpan, Event, Router, RouterConfig, KEEP_CURSOR};
use crate::secure::datagram::{Channel, LinkSecurity};
use crate::crossing::{
    judge, record_refusal, taken_at, CrossLine, CursorTrack, Gate, Refusal, RefusalLog, Sample, SampleSlot, Spot,
    TrackerNews,
};
use crate::spatial::{take_studio_requests, EdgeWatch, SharedSpatialState, SpatialConfig};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// Linux input event constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_REP: u16 = 0x14;

const SYN_REPORT: u16 = 0x00;
const SYN_DROPPED: u16 = 0x03;

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
const UI_SET_RELBIT: libc::c_ulong = 0x40045566;
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c5504;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

// evdev ioctl constants (x86_64 Linux)
const EVIOCGRAB: libc::c_ulong = 0x40044590;
const EVIOCSCLOCKID: libc::c_ulong = 0x400445a0;

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
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
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
            if !self.held_keys[idx] {
                // A repeated release: nothing to do.
                return;
            }
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
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
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

/// Wheel units per notch on the wire (the Windows `WHEEL_DELTA`).
const WHEEL_DELTA: i32 = 120;

/// Adds `delta` wire units to `carry` and returns the whole notches now due.
fn take_notches(carry: &mut i32, delta: i32) -> i32 {
    *carry = carry.saturating_add(delta);
    let notches = *carry / WHEEL_DELTA;
    *carry -= notches * WHEEL_DELTA;
    notches
}

/// The `MOUSE_BTN_*` id of an evdev button code.
fn button_id(code: u16) -> Option<u8> {
    match code {
        BTN_LEFT => Some(MOUSE_BTN_LEFT),
        BTN_RIGHT => Some(MOUSE_BTN_RIGHT),
        BTN_MIDDLE => Some(MOUSE_BTN_MIDDLE),
        BTN_SIDE => Some(MOUSE_BTN_SIDE),
        BTN_EXTRA => Some(MOUSE_BTN_EXTRA),
        _ => None,
    }
}

/// The evdev button code of a `MOUSE_BTN_*` id.
fn button_code(button: u8) -> Option<u16> {
    match button {
        MOUSE_BTN_LEFT => Some(BTN_LEFT),
        MOUSE_BTN_RIGHT => Some(BTN_RIGHT),
        MOUSE_BTN_MIDDLE => Some(BTN_MIDDLE),
        MOUSE_BTN_SIDE => Some(BTN_SIDE),
        MOUSE_BTN_EXTRA => Some(BTN_EXTRA),
        _ => None,
    }
}

/// Relative pointer that replays the laptop's mouse on Debian. Its name must keep the
/// `mesh-kvm` prefix: `discover_devices` would otherwise read it back as a physical mouse.
pub struct VirtualRelMouse {
    fd: RawFd,
    held_buttons: HashSet<u16>,
    wheel_carry_x: i32,
    wheel_carry_y: i32,
}

impl VirtualRelMouse {
    pub fn new() -> Result<Self, String> {
        let path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err("Failed to open /dev/uinput for Virtual Relative Mouse".to_string());
        }

        unsafe {
            libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int);
            libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int);
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                libc::ioctl(fd, UI_SET_KEYBIT, button as libc::c_int);
            }
            libc::ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int);
            for axis in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                libc::ioctl(fd, UI_SET_RELBIT, axis as libc::c_int);
            }

            let mut setup = UinputSetup {
                id: InputId {
                    bustype: 0x03,
                    vendor: 0x1d6b,
                    product: 0x0106,
                    version: 1,
                },
                name: [0u8; 80],
                ff_effects_max: 0,
            };
            let dev_name = b"mesh-kvm Relative Pointer";
            setup.name[..dev_name.len()].copy_from_slice(dev_name);

            if libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) < 0
                || libc::ioctl(fd, UI_DEV_CREATE) < 0
            {
                libc::close(fd);
                return Err("Failed to create mesh-kvm Relative Pointer".to_string());
            }
        }

        Ok(Self {
            fd,
            held_buttons: HashSet::new(),
            wheel_carry_x: 0,
            wheel_carry_y: 0,
        })
    }

    pub fn move_rel(&self, dx: i32, dy: i32) {
        if dx != 0 {
            self.write_event(EV_REL, REL_X, dx);
        }
        if dy != 0 {
            self.write_event(EV_REL, REL_Y, dy);
        }
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn button(&mut self, button: u8, pressed: bool) {
        let Some(code) = button_code(button) else {
            return;
        };
        if pressed {
            self.held_buttons.insert(code);
        } else if !self.held_buttons.remove(&code) {
            // A repeated release: nothing to do.
            return;
        }
        self.write_event(EV_KEY, code, pressed as i32);
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    /// `dx`, `dy` are in wire units, `WHEEL_DELTA` per notch.
    pub fn wheel(&mut self, dx: i32, dy: i32) {
        let notches_x = take_notches(&mut self.wheel_carry_x, dx);
        let notches_y = take_notches(&mut self.wheel_carry_y, dy);
        if notches_x != 0 {
            self.write_event(EV_REL, REL_HWHEEL, notches_x);
        }
        if notches_y != 0 {
            self.write_event(EV_REL, REL_WHEEL, notches_y);
        }
        if notches_x != 0 || notches_y != 0 {
            self.write_event(EV_SYN, SYN_REPORT, 0);
        }
    }

    pub fn release_held(&mut self) {
        if self.held_buttons.is_empty() {
            return;
        }
        for code in std::mem::take(&mut self.held_buttons) {
            self.write_event(EV_KEY, code, 0);
        }
        self.write_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn any_held(&self) -> bool {
        !self.held_buttons.is_empty()
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

impl Drop for VirtualRelMouse {
    fn drop(&mut self) {
        self.release_held();
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
    /// Shared with a release under way on its own thread, so it stays open until that's done.
    fd: Arc<OwnedFd>,
    /// The clock the kernel stamps this device's events with.
    clock: libc::clockid_t,
    /// Keys and buttons physically down on this device.
    pub held: HashSet<u16>,
    grab: GrabSlot,
    /// Keys and buttons put on the desktop through the virtual devices while focus came back.
    handed_back: HashSet<u16>,
}

/// An evdev device as a release thread needs it.
#[derive(Clone)]
struct EvdevHandle {
    fd: Arc<OwnedFd>,
    clock: libc::clockid_t,
}

/// The real `EVIOCGRAB`.
struct EvdevGrab;

fn clock_now_us(clock: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as i64 * 1_000_000 + ts.tv_nsec as i64 / 1_000
}

fn event_time_us(ev: &InputEvent) -> i64 {
    ev.tv_sec as i64 * 1_000_000 + ev.tv_usec as i64
}

fn evdev_grab(fd: RawFd, enable: bool) -> std::io::Result<()> {
    let arg: libc::c_int = enable as libc::c_int;
    if unsafe { libc::ioctl(fd, EVIOCGRAB, arg) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl GrabOps<EvdevHandle> for EvdevGrab {
    fn grab(&self, device: &EvdevHandle) -> std::io::Result<()> {
        evdev_grab(device.fd.as_raw_fd(), true)
    }
    fn release(&self, device: &EvdevHandle, started: &dyn Fn(i64)) -> std::io::Result<()> {
        started(clock_now_us(device.clock));
        evdev_grab(device.fd.as_raw_fd(), false)
    }
}

/// Updates `held` from one event read off a device.
fn track_held(held: &mut HashSet<u16>, ev: &InputEvent) {
    match ev.type_ {
        EV_KEY if ev.value == 0 => {
            held.remove(&ev.code);
        }
        EV_KEY => {
            held.insert(ev.code);
        }
        // The kernel lost events, possibly a release. Forget rather than never grab again.
        EV_SYN if ev.code == SYN_DROPPED => held.clear(),
        _ => {}
    }
}

/// A device can be grabbed with nothing down, or with only keys the peer is holding. The desktop
/// never saw those go down, as they went down under a grab, and only came back to the peer with
/// control after a dropout. So it can't be left with a key stuck, and without the grab everything
/// typed under a held Ctrl would land here.
fn free_to_grab(held: &HashSet<u16>, forwarded: &HeldInput) -> bool {
    held.iter().all(|key| forwarded.keys().any(|forwarded| forwarded == *key))
}

/// Whether a device should be grabbed now. A grab hides the release of any key already down from
/// the desktop, leaving that key stuck on Debian, so a new grab waits until the device is idle.
/// Letting go never waits.
fn should_grab(wanted: bool, grabbed: bool, idle: bool) -> bool {
    wanted && (grabbed || idle)
}

/// Whether a local event belongs to the router. While a wanted grab is still waiting the desktop
/// receives everything, so new input stays there alone. Releases always go through: only they
/// clear a key or button forwarded earlier.
fn reaches_router(blocking: bool, grabbed: bool, release: bool) -> bool {
    release || !blocking || grabbed
}

/// Where one local event goes: to the router, and whether to put it on the desktop if the router
/// doesn't send it to the peer. `held` is a grab that stays; `only_here` that the grab kept the event
/// from the desktop. While input goes to the peer, an event the desktop never saw goes to the
/// router as a grabbed one would, and nothing is put on Debian; only with Debian in charge is a
/// lost event handed back.
fn local_route(blocking: bool, held: bool, only_here: bool, release: bool) -> (bool, bool) {
    (reaches_router(blocking, held || only_here, release), only_here && !blocking)
}

impl EvdevDevice {
    /// Reads pending events into `buf` and returns how many; 0 when there are none.
    /// An unplugged device says so here, and is marked gone.
    fn read_events(&mut self, buf: &mut [InputEvent]) -> usize {
        if self.grab.gone() {
            return 0;
        }
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                std::mem::size_of_val(buf),
            )
        };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENODEV) {
            self.grab.mark_gone();
            self.log(GrabNews::Gone);
        }
        n.max(0) as usize / std::mem::size_of::<InputEvent>()
    }

    /// Takes over a freshly opened evdev fd. Its events are stamped on the monotonic clock if the
    /// kernel allows, so a clock change can't confuse which came before a release.
    fn new(path: String, name: String, fd: RawFd) -> EvdevDevice {
        let monotonic: libc::c_int = libc::CLOCK_MONOTONIC;
        let clock = if unsafe { libc::ioctl(fd, EVIOCSCLOCKID, &monotonic) } == 0 {
            libc::CLOCK_MONOTONIC
        } else {
            libc::CLOCK_REALTIME
        };
        EvdevDevice {
            path,
            name,
            fd: Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }),
            clock,
            held: HashSet::new(),
            grab: GrabSlot::default(),
            handed_back: HashSet::new(),
        }
    }

    fn handle(&self) -> EvdevHandle {
        EvdevHandle { fd: Arc::clone(&self.fd), clock: self.clock }
    }

    /// Grabbed and staying so: what it reads goes to this process alone.
    pub fn grabbed(&self) -> bool {
        self.grab.held()
    }

    /// Whether an event read off the device reached this process alone, as the desktop won't see
    /// it, and should get it if it was meant for here.
    fn only_here(&self, ev: &InputEvent) -> bool {
        self.grab.delivered_only_here(event_time_us(ev))
    }

    fn log(&self, news: GrabNews) {
        match news {
            GrabNews::Enabled | GrabNews::Released => println!(
                "[Debian] EVIOCGRAB {} on '{}' ({})",
                if news == GrabNews::Enabled { "ENABLED" } else { "RELEASED" },
                self.name,
                self.path
            ),
            GrabNews::Gone => eprintln!("[Debian] '{}' ({}) has gone; it's left alone until a restart.", self.name, self.path),
            GrabNews::Failed { grab, error } => eprintln!(
                "[Debian] EVIOCGRAB {} failed on '{}' ({}): {}",
                if grab { "grab" } else { "release" },
                self.name,
                self.path,
                error
            ),
        }
    }

    fn drive_grab(&mut self, want: bool, ops: &Arc<dyn GrabOps<EvdevHandle>>) {
        let handle = self.handle();
        for news in self.grab.drive(want, &handle, ops, Instant::now()) {
            self.log(news);
        }
    }
}

impl Drop for EvdevDevice {
    fn drop(&mut self) {
        // A release still under way holds the fd and finishes by itself; closing lets go too.
        if self.grab.held() {
            let _ = evdev_grab(self.fd.as_raw_fd(), false);
        }
    }
}

/// `O_CLOEXEC` because a grab lives as long as its file description: a long-running child such
/// as dbus-monitor would otherwise keep the keyboard grabbed after this process died.
const EVDEV_OPEN_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC;

fn discover_devices(explicit_mouse: Option<&str>) -> (Vec<EvdevDevice>, Vec<EvdevDevice>) {
    let mut mice = Vec::new();
    let mut keyboards = Vec::new();

    if let Some(mouse_path) = explicit_mouse {
        if let Ok(cpath) = CString::new(mouse_path) {
            let fd = unsafe { libc::open(cpath.as_ptr(), EVDEV_OPEN_FLAGS) };
            if fd >= 0 {
                let name = get_device_name(fd);
                mice.push(EvdevDevice::new(mouse_path.to_string(), name, fd));
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
        let fd = unsafe { libc::open(cpath.as_ptr(), EVDEV_OPEN_FLAGS) };
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
            mice.push(EvdevDevice::new(path, name, fd));
        } else if has_kbd_keys {
            keyboards.push(EvdevDevice::new(path, name, fd));
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

/// The KWin script that reports the cursor. Each position goes out with a number and the time KWin
/// took it, so the receiver can tell how old it is. They go out at most once every `SEND_GAP_MS`,
/// the latest one always going out at the end of the gap, so that nothing queues up behind a fast
/// mouse. A KWin without `QTimer` gets the older way: every move near an edge, and every 12 ms
/// elsewhere.
const KWIN_SCRIPT: &str = r#"
var SEND_GAP_MS = 2;
var lastEmit = 0;
var seq = 0;
var waiting = false;
var timer = null;
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
function send() {
    var p = workspace.cursorPos;
    var now = Date.now();
    lastEmit = now;
    seq = (seq % 2000000000) + 1;
    callDBus("org.meshkvm.Cursor", "/Cursor", "org.meshkvm.Cursor", "Update",
             Math.round(p.x) | 0, Math.round(p.y) | 0, isGameWindow() | 0, seq | 0, (now % 1000000000) | 0);
}
try {
    timer = new QTimer();
    timer.singleShot = true;
    timer.interval = SEND_GAP_MS;
    timer.timeout.connect(function() { waiting = false; send(); });
} catch (e) {
    timer = null;
}
function emitPos(force) {
    var now = Date.now();
    if (timer === null) {
        var p = workspace.cursorPos;
        var x = Math.round(p.x);
        var y = Math.round(p.y);
        var nearEdge = (x <= 16 || x >= 4464 || (x >= 1904 && x <= 1936) ||
                        y <= 16 || y >= 1424 || (y >= 344 && y <= 376));
        if (force || nearEdge || (now - lastEmit) >= 12) {
            send();
        }
        return;
    }
    if (force || (!waiting && now - lastEmit >= SEND_GAP_MS)) {
        if (waiting) {
            timer.stop();
            waiting = false;
        }
        send();
    } else if (!waiting) {
        waiting = true;
        timer.start();
    }
}
workspace.cursorPosChanged.connect(function() { emitPos(false); });
if (workspace.windowActivated) {
    workspace.windowActivated.connect(function() { emitPos(true); });
}
emitPos(true);
"#;

/// One `Update` call from the KWin script, as dbus-monitor prints it.
#[derive(Debug, PartialEq)]
struct Reading {
    x: i32,
    y: i32,
    game: bool,
    /// The script's number for it, and when KWin took it by `Date.now()` modulo 10^9. An older
    /// script sends neither.
    seq: Option<u32>,
    kwin_ms: Option<i64>,
    /// The stamp came as a double, which has lost its last digits, so it is no stamp.
    bad_stamp: bool,
}

/// Puts dbus-monitor's lines back together into calls: a header line, then one line per argument.
#[derive(Default)]
struct MonitorReader {
    args: Vec<i64>,
    /// Which of `args` came as a double.
    doubles: Vec<bool>,
    in_update: bool,
}

impl MonitorReader {
    fn feed(&mut self, line: &str) -> Option<Reading> {
        let trimmed = line.trim();
        let header = ["method call", "method return", "error", "signal"].iter().any(|kind| trimmed.starts_with(kind));
        if header {
            // An older script's call has three arguments, and ends only when the next one starts.
            let legacy = (self.in_update && self.args.len() == 3).then(|| self.reading());
            self.args.clear();
            self.doubles.clear();
            self.in_update = trimmed.starts_with("method call") && trimmed.ends_with("member=Update");
            return legacy;
        }
        if !self.in_update {
            return None;
        }
        let number = ["int32 ", "uint32 ", "int64 ", "uint64 ", "double "].iter().find_map(|kind| {
            let value = trimmed.strip_prefix(kind)?.trim();
            value.parse::<i64>().ok().or_else(|| value.parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v.round() as i64))
        });
        self.args.push(number?);
        self.doubles.push(trimmed.starts_with("double "));
        if self.args.len() < 5 {
            return None;
        }
        let reading = self.reading();
        self.args.clear();
        self.doubles.clear();
        self.in_update = false;
        Some(reading)
    }

    fn reading(&self) -> Reading {
        let arg = |i: usize| self.args.get(i).copied();
        let int = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let bad_stamp = self.doubles.get(4).copied().unwrap_or(false);
        Reading {
            x: int(arg(0).unwrap_or(0)),
            y: int(arg(1).unwrap_or(0)),
            game: arg(2).unwrap_or(0) != 0,
            seq: arg(3).map(|v| v as u32),
            kwin_ms: arg(4).filter(|_| !bad_stamp),
            bad_stamp,
        }
    }
}

/// Microseconds on the monotonic clock, which positions and movement are both timed by.
fn mono_us() -> i64 {
    clock_now_us(libc::CLOCK_MONOTONIC)
}

/// When the kernel says a movement happened, on the clock positions are timed by, so that a late
/// read of the mouse doesn't make movement KWin already showed look newer than its position. A
/// device that isn't on the monotonic clock, or a time from the future, goes by `now_us`.
fn motion_time_us(clock: libc::clockid_t, event_us: i64, now_us: i64) -> i64 {
    if clock == libc::CLOCK_MONOTONIC && event_us > 0 {
        event_us.min(now_us)
    } else {
        now_us
    }
}

fn wall_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Spawns a KWin Scripting bridge that tracks the TRUE KDE Plasma 6 Wayland cursor (x, y)
/// AND active fullscreen/game windows in real time via `workspace.cursorPosChanged` over session D-Bus.
/// Only the newest position is kept, with when KWin took it.
fn spawn_kwin_cursor_tracker(positions: Arc<SampleSlot>, kwin_game_active: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("mesh-kvm-kwin-cursor".to_string())
        .spawn(move || {
            let script_path = "/tmp/mesh_kvm_kwin_cursor.js";
            let _ = fs::write(script_path, KWIN_SCRIPT);

            // Start dbus-monitor BEFORE running the KWin script so we catch the initial emitPos(true).
            // Line by line, so a position is never left sitting in its buffer.
            let monitor_args = ["--session", "interface='org.meshkvm.Cursor'"];
            let line_buffered = Command::new("stdbuf")
                .arg("-oL")
                .arg("dbus-monitor")
                .args(monitor_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let child_opt = line_buffered
                .or_else(|_| {
                    Command::new("dbus-monitor").args(monitor_args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
                })
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
                    let mut reader = MonitorReader::default();
                    let mut unnumbered = 0u32;
                    let mut said_bad_stamp = false;
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        let Some(reading) = reader.feed(&line) else { continue };
                        if reading.bad_stamp && !said_bad_stamp {
                            said_bad_stamp = true;
                            crate::linklog::record("cursor_tracker", crate::linklog::Details::new().text("state", "bad_stamp"));
                        }
                        kwin_game_active.store(reading.game, Ordering::Relaxed);
                        let seq = reading.seq.unwrap_or_else(|| {
                            unnumbered = unnumbered.wrapping_add(1);
                            unnumbered
                        });
                        let taken_us = taken_at(reading.kwin_ms, wall_ms(), mono_us());
                        positions.offer(Sample { x: reading.x, y: reading.y, seq, taken_us });
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

const DEBIAN: DeviceId = NodeId::Debian as DeviceId;
const WINDOWS: DeviceId = NodeId::Windows as DeviceId;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hotkey {
    ManualSwitch,
    Panic,
    GameLock,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum KeyRoute {
    Hotkey(Hotkey),
    /// Repeat or release of a key that fired a hotkey.
    Consumed,
    Forward { scancode: u16, pressed: bool },
}

/// Finds hotkeys in the physical key stream, so a hotkey's own key never reaches the router.
/// Ctrl and Alt are ordinary keys here: they are tracked and passed on.
#[derive(Default)]
struct HotkeyFilter {
    ctrl_l: bool,
    ctrl_r: bool,
    alt_l: bool,
    alt_r: bool,
    /// Hotkey keys still physically down.
    consumed: HashSet<u16>,
}

impl HotkeyFilter {
    /// Classifies one evdev key event: `value` is 1 press, 2 auto-repeat, 0 release.
    fn feed(&mut self, code: u16, value: i32) -> KeyRoute {
        let pressed = value != 0;
        match code {
            KEY_LEFTCTRL => self.ctrl_l = pressed,
            KEY_RIGHTCTRL => self.ctrl_r = pressed,
            KEY_LEFTALT => self.alt_l = pressed,
            KEY_RIGHTALT => self.alt_r = pressed,
            _ => {}
        }
        if self.consumed.contains(&code) {
            if !pressed {
                self.consumed.remove(&code);
            }
            return KeyRoute::Consumed;
        }
        if value == 1 && (self.ctrl_l || self.ctrl_r) && (self.alt_l || self.alt_r) {
            let hotkey = match code {
                KEY_F11 => Some(Hotkey::ManualSwitch),
                KEY_F12 | KEY_SCROLLLOCK => Some(Hotkey::Panic),
                KEY_G => Some(Hotkey::GameLock),
                _ => None,
            };
            if let Some(hotkey) = hotkey {
                self.consumed.insert(code);
                return KeyRoute::Hotkey(hotkey);
            }
        }
        KeyRoute::Forward { scancode: code, pressed }
    }
}

struct LinuxBackend {
    vkbd: VirtualKeyboard,
    vmouse: VirtualMouse,
    vrel: VirtualRelMouse,
    mice: Vec<EvdevDevice>,
    keyboards: Vec<EvdevDevice>,
    block_mice: bool,
    block_keyboards: bool,
    /// Where the Debian cursor is, from KWin's positions and the movement since.
    track: CursorTrack,
    /// What the router has forwarded and not released, as of the last read of the devices.
    forwarded: HeldInput,
    grab_ops: Arc<dyn GrabOps<EvdevHandle>>,
}

impl LinuxBackend {
    /// Moves every device toward the wanted grab state, and retries any failed `EVIOCGRAB`. Must
    /// run straight after the devices are read: a new grab trusts `held`, and a key pressed since
    /// the last read would end up stuck.
    fn sync_grabs(&mut self, forwarded: HeldInput) {
        self.forwarded = forwarded;
        self.update_grabs(true);
    }

    /// Grabs happen here and now; releases start here and land on their own threads, keyboards
    /// first, and a device on its way out isn't grabbed again until its release has landed.
    fn update_grabs(&mut self, may_start: bool) {
        for (devices, wanted) in [
            (&mut self.keyboards, self.block_keyboards),
            (&mut self.mice, self.block_mice),
        ] {
            for device in devices {
                let idle = may_start && free_to_grab(&device.held, &self.forwarded);
                let want = should_grab(wanted, device.grabbed(), idle);
                device.drive_grab(want, &self.grab_ops);
            }
        }
        self.let_go_of_gone();
    }

    /// What was put on the desktop from a device that has gone is let go, as its releases never
    /// will come.
    fn let_go_of_gone(&mut self) {
        for keyboard in self.keyboards.iter_mut().filter(|k| k.grab.gone()) {
            for code in keyboard.handed_back.drain() {
                self.vkbd.inject_key(code, false);
            }
        }
        for mouse in self.mice.iter_mut().filter(|m| m.grab.gone()) {
            for button in mouse.handed_back.drain().filter_map(button_id) {
                self.vrel.button(button, false);
            }
        }
    }

    /// Waits a little for releases under way, so a clean stop leaves nothing grabbed and says so.
    fn finish_releases(&mut self, limit: Duration) {
        let devices: Vec<&mut EvdevDevice> = self.keyboards.iter_mut().chain(self.mice.iter_mut()).collect();
        let mut news = Vec::new();
        let (slots, labels): (Vec<&mut GrabSlot>, Vec<(String, String)>) =
            devices.into_iter().map(|d| (&mut d.grab, (d.name.clone(), d.path.clone()))).unzip();
        let done = wait_for_releases(slots.into_iter(), limit, |i, n| news.push((i, n)));
        for (i, n) in news {
            let (name, path) = &labels[i];
            if n == GrabNews::Released {
                println!("[Debian] EVIOCGRAB RELEASED on '{name}' ({path})");
            } else if let GrabNews::Failed { error, .. } = n {
                eprintln!("[Debian] EVIOCGRAB release failed on '{name}' ({path}): {error}");
            }
        }
        if !done {
            eprintln!("[Debian] Some EVIOCGRAB releases were still going at exit; closing the devices lets go.");
        }
    }

    /// Where an event read off keyboard `i` goes: see `local_route`.
    fn key_route(&self, i: usize, ev: &InputEvent, release: bool) -> (bool, bool) {
        let device = &self.keyboards[i];
        local_route(self.block_keyboards, device.grabbed(), device.only_here(ev), release)
    }

    /// Where an event read off mouse `i` goes: see `local_route`. Moving the pointer while focus
    /// comes back to Debian then isn't lost.
    fn mouse_route(&self, i: usize, ev: &InputEvent, release: bool) -> (bool, bool) {
        let device = &self.mice[i];
        local_route(self.block_mice, device.grabbed(), device.only_here(ev), release)
    }

    /// Puts a key read off keyboard `i` on the desktop if the grab kept it from there, once, and
    /// lets go of one it put there.
    fn hand_back_key(&mut self, i: usize, ev: &InputEvent, hand_back: bool, forwarded: bool) {
        let device = &mut self.keyboards[i];
        if let Some(pressed) = hand_back_press(&mut device.handed_back, ev.code, ev.value, hand_back, forwarded) {
            self.vkbd.inject_key(ev.code, pressed);
        }
    }

    fn hand_back_button(&mut self, i: usize, ev: &InputEvent, button: u8, hand_back: bool, forwarded: bool) {
        let device = &mut self.mice[i];
        if let Some(pressed) = hand_back_press(&mut device.handed_back, ev.code, ev.value, hand_back, forwarded) {
            self.vrel.button(button, pressed);
        }
    }

    fn any_button_held(&self) -> bool {
        self.vrel.any_held() || self.mice.iter().any(|m| !m.held.is_empty())
    }
}

impl InputBackend for LinuxBackend {
    fn set_blocking(&mut self, mouse: bool, keyboard: bool) {
        self.block_mice = mouse;
        self.block_keyboards = keyboard;
        // Letting go is immediate. New grabs wait for `sync_grabs`, as `held` may be stale here.
        self.update_grabs(false);
    }

    fn inject_move(&mut self, dx: i32, dy: i32) {
        self.vrel.move_rel(dx, dy);
    }

    fn inject_button(&mut self, button: u8, pressed: bool) {
        self.vrel.button(button, pressed);
    }

    fn inject_wheel(&mut self, dx: i32, dy: i32) {
        self.vrel.wheel(dx, dy);
    }

    fn inject_key(&mut self, scancode: u16, pressed: bool) {
        self.vkbd.inject_key(scancode, pressed);
    }

    fn warp_cursor(&mut self, x: i32, y: i32) {
        self.vmouse.warp_abs(x, y);
        // Where KWin really put it comes with its next position.
        self.track.warped(mono_us(), x, y);
    }

    fn release_held(&mut self) {
        self.vkbd.release_all_keys();
        self.vrel.release_held();
    }
}

struct UdpPeer<'a> {
    socket: &'a PeerSocket,
    seq: u64,
    presses: PressLog,
    motion: MotionBatch,
    clock: Instant,
}

impl UdpPeer<'_> {
    fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    fn send_now(&mut self, payload: Payload) {
        let pkt = Packet::new(self.seq, NodeId::Debian, payload);
        self.seq = self.seq.wrapping_add(1);
        if let Ok(bytes) = pkt.encode() {
            self.socket.send(&bytes);
        }
    }

    /// Movement that waited out the gap, and presses due to go again.
    fn send_due(&mut self) {
        let now_ms = self.now_ms();
        if let Some(motion) = self.motion.due(now_ms) {
            self.send_now(motion);
        }
        if let Some(repeat) = self.presses.repeat(now_ms) {
            self.send(repeat);
        }
    }
}

impl PeerSender for UdpPeer<'_> {
    fn send(&mut self, payload: Payload) {
        for payload in self.motion.send(payload, self.now_ms()) {
            self.send_now(payload);
        }
    }

    fn number_press(&mut self, event: PressEvent) -> Option<Payload> {
        self.presses.record(event, self.clock.elapsed().as_millis() as u64)
    }
}

/// The router with everything its actions touch, so each event is handled in one call.
struct Core<'a> {
    router: Router,
    backend: LinuxBackend,
    peer: UdpPeer<'a>,
    edge: EdgeWatch,
    /// A push that completed and waits for a position to confirm it.
    holding: Option<Held>,
    refusals: RefusalLog,
}

impl Core<'_> {
    /// Feeds one event to the router, carries out its actions, and returns them.
    fn route(&mut self, event: Event) -> Vec<Action> {
        let before = self.router.active();
        let actions = self.router.handle(event).actions;
        apply(&actions, &mut self.backend, &mut self.peer);
        let after = self.router.active();
        if after != before {
            self.edge.arrived(self.peer.now_ms());
            self.holding = None;
            let name = if after == DEBIAN { "DEBIAN" } else { "WINDOWS" };
            println!("[Debian] Focus switched to {name}");
        }
        actions
    }

    /// Counts movement of the Debian cursor by `(dx, dy)` against the shared edge, from either
    /// device's mouse, and crosses to the laptop when the push completes on a position that can be
    /// trusted. Otherwise the push waits, complete, for the next position or movement.
    fn push_edge(&mut self, cfg: &SpatialConfig, dx: i32, dy: i32, from_peer: bool) {
        let can_cross = self.router.online()
            && !self.router.panicked()
            && !self.router.captive()
            && self.router.active() == DEBIAN;
        if !can_cross {
            self.holding = None;
            return;
        }
        let monitors = cfg.monitors(NodeId::Debian);
        let (here, news) = self.backend.track.locate(mono_us(), monitors);
        if let Some(news) = news {
            note_tracker(news, here.age_ms);
        }
        let held = self.backend.any_button_held();
        let now_ms = self.peer.now_ms();
        if !self.edge.press(monitors, here.sample, (dx, dy), held) {
            self.holding = None;
            self.refusals.cleared();
            return;
        }
        if self.edge.too_soon(now_ms) {
            self.holding = None;
            if let Some(line) = self.refusals.refused(Refusal::Settle, now_ms) {
                record_refusal(line);
            }
            self.refusals.cleared();
            return;
        }
        let edge = &self.edge;
        let spot = |p| Spot { on_span: edge.on_span(monitors, p), guarded: edge.guarded(monitors, p) };
        match judge(&here, edge.along_x(), spot) {
            Gate::Cross { at } => {
                self.edge.clear();
                self.refusals.cleared();
                let held_ms = self.holding.take().map_or(0, |h| now_ms.saturating_sub(h.since_ms));
                let settle_left_ms = self.edge.settle_full_left_ms(now_ms);
                let entry = cfg.entry_on_peer(NodeId::Debian, at.0, at.1);
                let actions = self.route(Event::EdgeCrossed { x: entry.0, y: entry.1, from_peer });
                if actions.iter().any(|a| matches!(a, Action::SendFocus { .. })) {
                    let est = here.moved;
                    CrossLine { to_laptop: true, at, entry, sample_age_ms: here.age_ms, est, held_ms, settle_left_ms }.record();
                }
            }
            Gate::Hold(reason) => {
                if self.holding.is_none() {
                    self.holding = Some(Held { from_peer, since_ms: now_ms });
                }
                if let Some(line) = self.refusals.refused(reason, now_ms) {
                    record_refusal(line);
                }
            }
        }
    }

    /// Takes KWin's newest position, and decides a push that was waiting on it.
    fn hear_position(&mut self, cfg: &SpatialConfig, sample: Sample) {
        if !self.backend.track.is_new(&sample) {
            return;
        }
        if let Some(news) = self.backend.track.heard(sample) {
            note_tracker(news, 0);
        }
        if let Some(held) = self.holding {
            self.push_edge(cfg, 0, 0, held.from_peer);
        }
    }
}

/// A completed push waiting on a position it can trust.
#[derive(Clone, Copy)]
struct Held {
    from_peer: bool,
    since_ms: u64,
}

/// Says in the link log that KWin's positions stopped coming, or came back.
fn note_tracker(news: TrackerNews, age_ms: u64) {
    let details = crate::linklog::Details::new();
    let details = match news {
        TrackerNews::Silent => details.text("state", "silent").num("last_position_ms", age_ms),
        TrackerNews::Back => details.text("state", "back"),
    };
    crate::linklog::record("cursor_tracker", details);
}

pub fn run_debian_node(
    spatial_state: Arc<RwLock<SharedSpatialState>>,
    peer_addr: SocketAddr,
    channel: Arc<Channel>,
    listen_port: u16,
    override_width: Option<i32>,
    override_height: Option<i32>,
    explicit_mouse: Option<String>,
) -> Result<(), String> {
    let (det_w, det_h) = detect_screen_dimensions();
    let width = override_width.unwrap_or(det_w).max(100);
    let height = override_height.unwrap_or(det_h).max(100);

    crate::mute::start();

    let vkbd = VirtualKeyboard::new()?;
    let vmouse = VirtualMouse::new(width, height)?;
    let vrel = VirtualRelMouse::new()?;
    println!("[Debian] Created /dev/uinput Virtual Keyboard, Absolute Warp Pointer & Relative Pointer.");

    let positions = Arc::new(SampleSlot::default());
    let kwin_game_active = Arc::new(AtomicBool::new(false));
    spawn_kwin_cursor_tracker(Arc::clone(&positions), Arc::clone(&kwin_game_active));

    let (mice, keyboards) = discover_devices(explicit_mouse.as_deref());
    if mice.is_empty() {
        return Err("No physical mouse devices found in /dev/input/event*".to_string());
    }

    let bind_addr: SocketAddr = format!("0.0.0.0:{}", listen_port).parse().unwrap();
    let socket = crate::app::launch::bind_patiently(|| UdpSocket::bind(bind_addr))
        .map_err(|e| format!("Failed to bind UDP {}: {}", bind_addr, e))?;
    crate::update::note_service_up();
    crate::lowlat::mark(&socket, crate::lowlat::Class::Input, "input", Some(peer_addr));
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set UDP socket nonblocking: {}", e))?;

    // Initialize sequence counter with current Unix timestamp in microseconds, so that a restart
    // normally carries on upward. `PeerLink` copes when the clock has gone back instead.
    let init_seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    let peer_socket = PeerSocket::new(socket, peer_addr, channel);

    let clock = Instant::now();
    let now_ms = || clock.elapsed().as_millis() as u64;

    let mut cfg_snapshot = spatial_state
        .read()
        .map(|guard| guard.config.clone())
        .map_err(|_| "Spatial state lock poisoned".to_string())?;
    let mut core = Core {
        router: Router::new(RouterConfig {
            me: DEBIAN,
            peer: WINDOWS,
            main: crate::settings::main_device(),
            sharing: crate::settings::sharing(),
            pointer_speed: crate::settings::pointer_speed(),
        }),
        backend: LinuxBackend {
            vkbd,
            vmouse,
            vrel,
            mice,
            keyboards,
            block_mice: false,
            block_keyboards: false,
            track: CursorTrack::new(mono_us(), (3200, 720)),
            forwarded: HeldInput::default(),
            grab_ops: Arc::new(EvdevGrab),
        },
        peer: UdpPeer {
            socket: &peer_socket,
            seq: init_seq,
            presses: PressLog::default(),
            motion: MotionBatch::default(),
            clock,
        },
        edge: EdgeWatch::new(),
        holding: None,
        refusals: RefusalLog::new(),
    };

    let mut hotkeys = HotkeyFilter::default();
    let mut heartbeat = HeartbeatPace::default();
    let mut last_telemetry_sync = Instant::now();
    let mut link = PeerLink::default();
    let mut link_watch = crate::linklog::LinkWatch::new(now_ms());
    // Each time round is half a millisecond's sleep and the work, so anything past a millisecond is late.
    let mut loop_clock = crate::linklog::LoopClock::new("input", 1);

    let mut udp_buf = [0u8; MAX_DATAGRAM];
    let mut opened = Vec::new();
    let mut handshake_known = None;
    let mut ev_buf = [InputEvent::default(); 64];

    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, handle_sigint as *const () as usize);
    }

    println!(
        "[Debian] Spatial API ready on port 24880"
    );

    let mut claim_due = true;

    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) || crate::settings::restart_requested() {
            // Input first: the D-Bus call below can hang if KWin does.
            println!("\n[Debian] Shutting down cleanly; releasing all EVIOCGRAB locks.");
            core.backend.set_blocking(false, false);
            core.backend.finish_releases(Duration::from_secs(1));
            core.backend.release_held();
            crate::mute::shutdown();
            let _ = Command::new("qdbus6")
                .args([
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.unloadScript",
                    "mesh_kvm_cursor_watch",
                ])
                .output();
            break Ok(());
        }

        loop_clock.tick();
        // Every switch is stamped with the newest time the router has seen.
        core.route(Event::Tick { now_ms: now_ms() });
        crate::audio::set_peer_online(core.router.online());
        crate::secure::control::set_peer_online(core.router.online());
        link.set_online(core.router.online());
        core.peer.presses.set_reliable(link.peer_version(now_ms()) == PeerVersion::Same);
        if !core.router.online() {
            core.peer.presses.forget();
        }

        // Check WebUI actions (manual toggle / panic). If the lock is busy they wait for the next
        // iteration and the last config stands.
        let mut manual_toggle = false;
        let mut web_panic = false;
        if let Some(requests) = take_studio_requests(&spatial_state) {
            manual_toggle = requests.manual_switch;
            web_panic = requests.panic;
            cfg_snapshot = requests.config;
        }

        // Outside the Studio lock, so a busy lock cannot hold up a toggle that frees input.
        for event in crate::settings::take_router_events() {
            core.route(event);
        }

        core.router.set_pointer_speed(crate::settings::pointer_speed());
        let is_game_captive = cfg_snapshot.manual_game_lock
            || (cfg_snapshot.auto_game_lock && kwin_game_active.load(Ordering::Relaxed));
        if core.router.captive() != is_game_captive {
            core.route(Event::SetCaptive(is_game_captive));
        }

        if web_panic {
            println!("[PANIC] Panic requested from the Studio!");
            core.route(Event::Panic);
        }

        if manual_toggle {
            core.route(Event::ManualSwitch);
        }

        // 1. Drain incoming UDP packets on port 24850
        while let Ok((len, src_addr)) = peer_socket.socket.recv_from(&mut udp_buf) {
            let Some(pkt) = peer_socket.open(&udp_buf[..len], src_addr, now_ms(), &mut opened, &mut |what| link.note(what, now_ms())) else {
                continue;
            };
            let was_online = core.router.online();
            core.route(Event::PeerAlive { now_ms: now_ms() });
            if came_up(was_online, core.router.online()) {
                heartbeat.link_up();
            }
            if !link.arrived(&pkt, now_ms()) {
                continue;
            }
            match pkt.payload {
                Payload::KeyEvent { scancode, pressed } => {
                    core.route(Event::PeerKey { scancode, pressed });
                }
                Payload::MouseMoveDelta { dx, dy } => {
                    let actions = core.route(Event::PeerMove { dx, dy });
                    if actions.iter().any(|a| matches!(a, Action::InjectMove { .. })) {
                        core.backend.track.moved(mono_us(), dx, dy);
                        core.push_edge(&cfg_snapshot, dx, dy, true);
                    }
                }
                Payload::MouseButton { button, pressed } => {
                    core.route(Event::PeerButton { button, pressed });
                }
                Payload::MouseWheel { delta_x, delta_y } => {
                    core.route(Event::PeerWheel { dx: delta_x, dy: delta_y });
                }
                Payload::FocusSwitchAt { target_node, x, y } => {
                    let (x, y) = cfg_snapshot.place_on_own(NodeId::Debian, x, y);
                    core.route(Event::PeerFocus { target: target_node as DeviceId, x, y });
                }
                Payload::ActiveView { active_node } => {
                    core.route(Event::PeerActiveView { active: active_node as DeviceId });
                }
                Payload::PanicRelease => {
                    println!("[PANIC] Received PanicRelease from Windows!");
                    core.route(Event::PeerPanic);
                }
                Payload::PeerDisplayInfo { name, monitors } => {
                    crate::spatial::note_peer_displays(&spatial_state, &name, monitors);
                }
                Payload::EdgeMap { edge, start, end, resistance } => {
                    core.edge.set_span(EdgeSpan { edge, start, end }, resistance);
                }
                Payload::GameLock { locked } => {
                    core.route(Event::PeerGameLock(locked));
                }
                Payload::LayoutRevision { revision, editor } => {
                    crate::spatial::note_peer_revision(&spatial_state, revision, editor);
                }
                Payload::LinkState(state) => {
                    for event in link.heard_state(&state, now_ms()) {
                        core.route(event);
                    }
                }
                Payload::Presses { newest, sent_ms, events } => {
                    for event in link.take_presses(newest, sent_ms, &events, now_ms()) {
                        core.route(press_event(event));
                    }
                }
                _ => {}
            }
        }
        for event in link.due_presses(now_ms()) {
            core.route(press_event(event));
        }
        peer_socket.tick(now_ms());
        if let Some(at) = peer_socket.handshake_news(&mut handshake_known) {
            link.handshake_heard(at);
        }

        // A laptop that outlived a Debian restart may still think it is active. This waits for a
        // session, as it would go nowhere before that. A main laptop must not hear it: it would
        // take it as a claim and forward to a Debian it may not reach.
        if claim_due && peer_socket.channel.security() == LinkSecurity::Encrypted {
            claim_due = false;
            if core.router.main() == DEBIAN {
                apply(
                    &[Action::SendFocus { target: DEBIAN, x: KEEP_CURSOR, y: KEEP_CURSOR }],
                    &mut core.backend,
                    &mut core.peer,
                );
            }
        }

        // KWin's newest position, which may decide a push that was waiting on it.
        if let Some(sample) = positions.newest() {
            core.hear_position(&cfg_snapshot, sample);
        }

        // 2. Read local keyboards. Grabbing only hides events from the desktop, so hotkeys are
        // seen here whichever device is active.
        for i in 0..core.backend.keyboards.len() {
            loop {
                let count = core.backend.keyboards[i].read_events(&mut ev_buf);
                if count == 0 {
                    break;
                }
                for ev in &ev_buf[..count] {
                    track_held(&mut core.backend.keyboards[i].held, ev);
                    if ev.type_ != EV_KEY {
                        continue;
                    }
                    match hotkeys.feed(ev.code, ev.value) {
                        KeyRoute::Forward { scancode, pressed } => {
                            let (to_router, hand_back) = core.backend.key_route(i, ev, !pressed);
                            let mut forwarded = false;
                            if to_router {
                                let actions = core.route(Event::LocalKey { scancode, pressed });
                                forwarded = actions.iter().any(|a| matches!(a, Action::ForwardKey { .. }));
                            }
                            core.backend.hand_back_key(i, ev, hand_back, forwarded);
                        }
                        KeyRoute::Consumed => {}
                        KeyRoute::Hotkey(Hotkey::Panic) => {
                            println!("[PANIC] Local Fail-Safe Hotkey triggered!");
                            core.route(Event::Panic);
                        }
                        KeyRoute::Hotkey(Hotkey::ManualSwitch) => {
                            println!("[Debian] Manual focus toggle (Ctrl+Alt+F11)");
                            core.route(Event::ManualSwitch);
                        }
                        KeyRoute::Hotkey(Hotkey::GameLock) => {
                            if let Ok(mut guard) = spatial_state.write() {
                                guard.config.manual_game_lock = !guard.config.manual_game_lock;
                                let locked = guard.config.manual_game_lock;
                                guard.save();
                                println!(
                                    "[Debian] Manual Game Captive Lock (Ctrl+Alt+G): {}",
                                    if locked { "LOCKED" } else { "UNLOCKED" }
                                );
                            }
                        }
                    }
                }
            }
        }

        // 3. Read physical mice
        for i in 0..core.backend.mice.len() {
            let mut acc_dx: i32 = 0;
            let mut acc_dy: i32 = 0;
            let mut acc_wheel_x: i32 = 0;
            let mut acc_wheel_y: i32 = 0;

            loop {
                let count = core.backend.mice[i].read_events(&mut ev_buf);
                if count == 0 {
                    break;
                }
                for ev in &ev_buf[..count] {
                    track_held(&mut core.backend.mice[i].held, ev);
                    match ev.type_ {
                        EV_REL => match ev.code {
                            REL_X => acc_dx += ev.value,
                            REL_Y => acc_dy += ev.value,
                            REL_WHEEL => acc_wheel_y += ev.value * WHEEL_DELTA,
                            REL_HWHEEL => acc_wheel_x += ev.value * WHEEL_DELTA,
                            _ => {}
                        },
                        EV_KEY => {
                            if let Some(button) = button_id(ev.code) {
                                let pressed = ev.value != 0;
                                let (to_router, hand_back) = core.backend.mouse_route(i, ev, !pressed);
                                let mut forwarded = false;
                                if to_router {
                                    let actions = core.route(Event::LocalButton { button, pressed });
                                    forwarded = actions.iter().any(|a| matches!(a, Action::ForwardButton { .. }));
                                }
                                core.backend.hand_back_button(i, ev, button, hand_back, forwarded);
                            }
                        }
                        EV_SYN if ev.code == SYN_REPORT => {
                            let (to_router, hand_back) = core.backend.mouse_route(i, ev, false);
                            if acc_dx != 0 || acc_dy != 0 {
                                let mut forwarded = false;
                                if to_router {
                                    let actions = core.route(Event::LocalMove { dx: acc_dx, dy: acc_dy });
                                    forwarded = actions.iter().any(|a| matches!(a, Action::ForwardMove { .. }));
                                }
                                if hand_back && !forwarded {
                                    core.backend.vrel.move_rel(acc_dx, acc_dy);
                                }
                                // Movement the laptop got didn't move this cursor.
                                if !forwarded {
                                    let at = motion_time_us(core.backend.mice[i].clock, event_time_us(ev), mono_us());
                                    core.backend.track.moved(at, acc_dx, acc_dy);
                                }
                                core.push_edge(&cfg_snapshot, acc_dx, acc_dy, false);
                                acc_dx = 0;
                                acc_dy = 0;
                            }
                            if acc_wheel_x != 0 || acc_wheel_y != 0 {
                                let mut forwarded = false;
                                if to_router {
                                    let actions = core.route(Event::LocalWheel {
                                        dx: acc_wheel_x,
                                        dy: acc_wheel_y,
                                    });
                                    forwarded = actions.iter().any(|a| matches!(a, Action::ForwardWheel { .. }));
                                }
                                if hand_back && !forwarded {
                                    core.backend.vrel.wheel(acc_wheel_x, acc_wheel_y);
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

        // Straight after the reads, so held state is as fresh as it can be.
        core.backend.sync_grabs(core.router.held_forwarded());

        core.peer.send_due();

        // Update live WebUI telemetry every 40ms
        if last_telemetry_sync.elapsed() >= Duration::from_millis(40) {
            last_telemetry_sync = Instant::now();
            if let Ok(mut guard) = spatial_state.write() {
                guard.telemetry.active_node = if core.router.active() == WINDOWS {
                    "Windows".to_string()
                } else {
                    "Debian".to_string()
                };
                guard.telemetry.panic_mode = core.router.panicked();
                (guard.telemetry.cursor_x, guard.telemetry.cursor_y) = core.backend.track.shown();
                guard.telemetry.game_captive_active = is_game_captive;
                guard.telemetry.game_reason = if cfg_snapshot.manual_game_lock {
                    "Manual Lock (Ctrl+Alt+G)".to_string()
                } else if cfg_snapshot.auto_game_lock && kwin_game_active.load(Ordering::Relaxed) {
                    "Auto: Fullscreen / Game Active".to_string()
                } else {
                    String::new()
                };
                guard.telemetry.peer_online = core.router.online();
                guard.telemetry.main_conflict = core.router.main_conflict();
                guard.telemetry.link = link.quality(&core.router, now_ms());
            }
            let look = crate::linklog::Look::of(&mut core.router, &mut link, &mut loop_clock, now_ms());
            crate::linklog::note(link_watch.look(look));
        }

        // Heartbeat, with what the other device needs to check its own edges (every 400ms)
        if heartbeat.due(now_ms()) {
            core.edge.use_own_layout(&cfg_snapshot, NodeId::Debian);
            for payload in crate::spatial::heartbeat_payloads(&cfg_snapshot, NodeId::Debian) {
                core.peer.send(payload);
            }
            if let Some(tail) = core.peer.presses.tail(now_ms()) {
                core.peer.send(tail);
            }
            let order = core.peer.presses.newest();
            let main = core.router.main() == DEBIAN;
            let state = link.next_state(core.router.held_forwarded(), core.router.panicked(), main, order, now_ms());
            for _ in 0..REPORT_COPIES {
                core.peer.send(Payload::LinkState(state));
            }
        }

        std::thread::sleep(Duration::from_micros(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(f: &mut HotkeyFilter, code: u16) -> KeyRoute {
        f.feed(code, 1)
    }
    fn forward(scancode: u16, pressed: bool) -> KeyRoute {
        KeyRoute::Forward { scancode, pressed }
    }
    fn ctrl_alt() -> HotkeyFilter {
        let mut f = HotkeyFilter::default();
        press(&mut f, KEY_LEFTCTRL);
        press(&mut f, KEY_LEFTALT);
        f
    }

    const UPDATE: &str = "method call time=1731300000.123456 sender=:1.42 -> destination=org.meshkvm.Cursor serial=99 path=/Cursor; interface=org.meshkvm.Cursor; member=Update";

    #[test]
    fn a_position_is_read_whole_from_what_dbus_monitor_prints() {
        let mut r = MonitorReader::default();
        let lines = [UPDATE, "   int32 2500", "   int32 1439", "   int32 0", "   int32 77", "   int32 123456789"];
        let got: Vec<Reading> = lines.iter().filter_map(|l| r.feed(l)).collect();
        assert_eq!(got, vec![Reading { x: 2500, y: 1439, game: false, seq: Some(77), kwin_ms: Some(123_456_789), bad_stamp: false }]);
        // Numbers that come through as doubles are read too, and a game flag.
        let lines = [UPDATE, "   double 10", "   double 20.4", "   int32 1", "   uint32 78", "   int64 5"];
        let got: Vec<Reading> = lines.iter().filter_map(|l| r.feed(l)).collect();
        assert_eq!(got, vec![Reading { x: 10, y: 20, game: true, seq: Some(78), kwin_ms: Some(5), bad_stamp: false }]);
    }

    #[test]
    fn a_stamp_that_comes_as_a_double_is_no_stamp() {
        // KWin sends JavaScript numbers as int32 today; a double here has lost the milliseconds.
        let mut r = MonitorReader::default();
        let lines = [UPDATE, "   int32 2500", "   int32 1439", "   int32 0", "   int32 77", "   double 7.34568e+08"];
        let got: Vec<Reading> = lines.iter().filter_map(|l| r.feed(l)).collect();
        assert_eq!(got, vec![Reading { x: 2500, y: 1439, game: false, seq: Some(77), kwin_ms: None, bad_stamp: true }]);
        let lines = [UPDATE, "   int32 1", "   int32 2", "   int32 0", "   int32 78", "   int32 734568123"];
        assert_eq!(lines.iter().filter_map(|l| r.feed(l)).next().map(|g| (g.kwin_ms, g.bad_stamp)), Some((Some(734_568_123), false)));
    }

    #[test]
    fn movement_is_timed_when_the_kernel_saw_it() {
        // Read 40 ms late after a stall, it is still 40 ms old.
        assert_eq!(motion_time_us(libc::CLOCK_MONOTONIC, 1_000_000, 1_040_000), 1_000_000);
        assert_eq!(motion_time_us(libc::CLOCK_MONOTONIC, 2_000_000, 1_040_000), 1_040_000, "never from the future");
        assert_eq!(motion_time_us(libc::CLOCK_REALTIME, 1_000_000, 1_040_000), 1_040_000);
        assert_eq!(motion_time_us(libc::CLOCK_MONOTONIC, 0, 1_040_000), 1_040_000);
    }

    #[test]
    fn the_script_sends_whole_numbers() {
        for arg in ["Math.round(p.x) | 0", "Math.round(p.y) | 0", "isGameWindow() | 0", "seq | 0", "(now % 1000000000) | 0"] {
            assert!(KWIN_SCRIPT.contains(arg), "{arg}");
        }
    }

    #[test]
    fn a_position_from_an_older_script_still_counts_once_its_message_ends() {
        let mut r = MonitorReader::default();
        for line in [UPDATE, "   int32 1", "   int32 2", "   int32 0"] {
            assert_eq!(r.feed(line), None);
        }
        assert_eq!(r.feed(UPDATE), Some(Reading { x: 1, y: 2, game: false, seq: None, kwin_ms: None, bad_stamp: false }));
    }

    #[test]
    fn anything_else_dbus_monitor_prints_is_passed_over() {
        let mut r = MonitorReader::default();
        let other = UPDATE.replace("member=Update", "member=Other");
        let lines = [other.as_str(), "   int32 1", "   int32 2", "   int32 0", "   int32 3", "   int32 4", "", "signal time=1 sender=org.freedesktop.DBus -> destination=:1.9 serial=2 path=/; interface=x; member=NameAcquired", "   string \":1.9\"", "   int32 nope"];
        assert!(lines.iter().all(|l| r.feed(l).is_none()));
        // A message cut short by the next one isn't taken for a whole one.
        assert_eq!(r.feed(UPDATE), None);
        r.feed("   int32 5");
        assert_eq!(r.feed(UPDATE), None);
    }

    #[test]
    fn hotkeys_need_ctrl_and_alt() {
        let mut f = HotkeyFilter::default();
        assert_eq!(press(&mut f, KEY_F11), forward(KEY_F11, true));
        press(&mut f, KEY_RIGHTCTRL);
        assert_eq!(press(&mut f, KEY_F12), forward(KEY_F12, true));
        press(&mut f, KEY_RIGHTALT);
        assert_eq!(press(&mut f, KEY_G), KeyRoute::Hotkey(Hotkey::GameLock));
    }

    #[test]
    fn hotkey_table() {
        for (code, want) in [
            (KEY_F11, Hotkey::ManualSwitch),
            (KEY_F12, Hotkey::Panic),
            (KEY_SCROLLLOCK, Hotkey::Panic),
            (KEY_G, Hotkey::GameLock),
        ] {
            assert_eq!(press(&mut ctrl_alt(), code), KeyRoute::Hotkey(want));
        }
    }

    #[test]
    fn ctrl_and_alt_themselves_are_forwarded() {
        let mut f = HotkeyFilter::default();
        assert_eq!(f.feed(KEY_LEFTCTRL, 1), forward(KEY_LEFTCTRL, true));
        assert_eq!(f.feed(KEY_LEFTALT, 1), forward(KEY_LEFTALT, true));
        assert_eq!(f.feed(KEY_LEFTALT, 2), forward(KEY_LEFTALT, true));
        assert_eq!(f.feed(KEY_LEFTALT, 0), forward(KEY_LEFTALT, false));
    }

    #[test]
    fn repeat_counts_as_pressed_and_zero_as_released() {
        let mut f = HotkeyFilter::default();
        assert_eq!(f.feed(30, 1), forward(30, true));
        assert_eq!(f.feed(30, 2), forward(30, true));
        assert_eq!(f.feed(30, 0), forward(30, false));
    }

    #[test]
    fn hotkey_repeat_and_release_are_never_forwarded() {
        let mut f = ctrl_alt();
        assert_eq!(f.feed(KEY_F11, 1), KeyRoute::Hotkey(Hotkey::ManualSwitch));
        assert_eq!(f.feed(KEY_F11, 2), KeyRoute::Consumed);
        // Modifiers let go first: the held key is still the hotkey's, not a typed F11.
        f.feed(KEY_LEFTCTRL, 0);
        f.feed(KEY_LEFTALT, 0);
        assert_eq!(f.feed(KEY_F11, 2), KeyRoute::Consumed);
        assert_eq!(f.feed(KEY_F11, 0), KeyRoute::Consumed);
        // Released, so the next plain press is an ordinary key.
        assert_eq!(f.feed(KEY_F11, 1), forward(KEY_F11, true));
    }

    #[test]
    fn hotkey_repeat_does_not_fire_again() {
        let mut f = ctrl_alt();
        f.feed(KEY_F12, 1);
        for _ in 0..5 {
            assert_eq!(f.feed(KEY_F12, 2), KeyRoute::Consumed);
        }
    }

    #[test]
    fn modifier_on_either_side_counts_until_both_are_up() {
        let mut f = ctrl_alt();
        f.feed(KEY_RIGHTCTRL, 1);
        f.feed(KEY_LEFTCTRL, 0);
        assert_eq!(f.feed(KEY_F12, 1), KeyRoute::Hotkey(Hotkey::Panic));
        f.feed(KEY_F12, 0);
        f.feed(KEY_RIGHTCTRL, 0);
        assert_eq!(f.feed(KEY_F12, 1), forward(KEY_F12, true));
    }

    fn key_event(code: u16, value: i32) -> InputEvent {
        InputEvent { type_: EV_KEY, code, value, ..Default::default() }
    }

    #[test]
    fn held_set_follows_key_events() {
        let mut held = HashSet::new();
        track_held(&mut held, &key_event(30, 1));
        track_held(&mut held, &key_event(31, 1));
        track_held(&mut held, &key_event(30, 2));
        track_held(&mut held, &key_event(30, 0));
        assert_eq!(held, HashSet::from([31]));
        let rel = InputEvent { type_: EV_REL, code: 31, value: 0, ..Default::default() };
        track_held(&mut held, &rel);
        assert_eq!(held, HashSet::from([31]));
    }

    #[test]
    fn dropped_events_forget_held_keys() {
        // A lost release must not leave the device ungrabbable.
        let mut held = HashSet::from([31]);
        let drop = InputEvent { type_: EV_SYN, code: SYN_DROPPED, value: 0, ..Default::default() };
        track_held(&mut held, &drop);
        assert!(held.is_empty());
    }

    #[test]
    fn a_grab_can_start_with_only_a_forwarded_key_down() {
        let mut forwarded = HeldInput::default();
        forwarded.set_key(KEY_LEFTCTRL, true);
        let held = |keys: &[u16]| keys.iter().copied().collect::<HashSet<u16>>();
        assert!(free_to_grab(&held(&[]), &HeldInput::default()));
        assert!(free_to_grab(&held(&[KEY_LEFTCTRL]), &forwarded));
        assert!(!free_to_grab(&held(&[KEY_LEFTCTRL]), &HeldInput::default()));
        assert!(!free_to_grab(&held(&[KEY_LEFTCTRL, KEY_G]), &forwarded), "the desktop may have seen that one");
    }

    #[test]
    fn grab_waits_for_held_keys_to_release() {
        // A release swallowed by the grab would leave the key stuck down on Debian.
        assert!(!should_grab(true, false, false));
        assert!(should_grab(true, false, true));
    }

    #[test]
    fn grab_stays_on_while_keys_are_held() {
        assert!(should_grab(true, true, false));
    }

    #[test]
    fn ungrab_never_waits() {
        assert!(!should_grab(false, true, false));
        assert!(!should_grab(false, true, true));
        assert!(!should_grab(false, false, true));
    }

    #[test]
    fn input_goes_to_the_router_when_nothing_blocks_or_the_grab_holds() {
        assert!(reaches_router(false, false, false));
        assert!(reaches_router(true, true, false));
    }

    #[test]
    fn desktop_keeps_new_input_while_a_grab_waits() {
        // It already has the event, so forwarding it too would give it two destinations.
        assert!(!reaches_router(true, false, false));
    }

    #[test]
    fn releases_always_reach_the_router() {
        // A key forwarded earlier is only cleared, here and on the peer, by its release.
        assert!(reaches_router(true, false, true));
        assert!(reaches_router(true, true, true));
        assert!(reaches_router(false, false, true));
    }

    #[test]
    fn a_lost_event_is_handed_back_only_while_debian_has_the_input() {
        // Debian active, a release under way, the grab kept this one from the desktop.
        assert_eq!(local_route(false, false, true, false), (true, true));
        // Back to the laptop before the release landed: it goes there, and nothing on Debian.
        assert_eq!(local_route(true, false, true, false), (true, false));
        // The desktop saw it itself, with the laptop wanting a grab that waits.
        assert_eq!(local_route(true, false, false, false), (false, false));
        assert_eq!(local_route(true, false, false, true), (true, false), "releases always go");
        // An ordinary grab.
        assert_eq!(local_route(true, true, true, false), (true, false));
        // A release that failed, with Debian in charge: handed back.
        assert_eq!(local_route(false, true, true, false), (true, true));
        // Nothing grabbed at all.
        assert_eq!(local_route(false, false, false, false), (true, false));
    }

    #[test]
    fn wheel_keeps_the_remainder_between_events() {
        let mut carry = 0;
        assert_eq!(take_notches(&mut carry, 120), 1);
        assert_eq!(take_notches(&mut carry, -240), -2);
        assert_eq!(take_notches(&mut carry, 40), 0);
        assert_eq!(take_notches(&mut carry, 40), 0);
        assert_eq!(take_notches(&mut carry, 40), 1);
        assert_eq!(carry, 0);
    }

    #[test]
    fn button_ids_round_trip() {
        for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            assert_eq!(button_code(button_id(code).unwrap()), Some(code));
        }
        assert_eq!(button_id(KEY_F11), None);
        assert_eq!(button_code(0), None);
        assert_eq!(button_code(6), None);
    }
}
