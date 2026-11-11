//! Low-latency Wi-Fi. Both devices reach each other only over Wi-Fi, so every packet crosses it
//! twice. While the devices are linked, the laptop keeps its Wi-Fi from scanning in the background
//! (each scan takes the radio off its channel for a moment, which shows as a 100-500 ms spike), and
//! both devices mark input and sound so that Wi-Fi sends them ahead of other traffic.
//!
//! Everything here runs on a thread of its own: the calls can wait on a system service, and
//! nothing may hold up the input loops or hooks. Every failure is logged once and otherwise
//! ignored.

pub mod policy;
pub mod qos;
#[cfg(windows)]
mod win;

pub use qos::Class;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// The Low-latency Wi-Fi setting. On unless the person turned it off.
pub static LOW_LATENCY_WIFI: AtomicBool = AtomicBool::new(true);
/// How often the thread looks at the link when nothing wakes it.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// How long stopping waits for streaming mode to go off.
const STOP_WAIT: Duration = Duration::from_secs(1);

enum Msg {
    Wake,
    Stop(SyncSender<()>),
    /// A restart that was stopped for didn't happen after all.
    Resume,
}

static WAKE: OnceLock<SyncSender<Msg>> = OnceLock::new();
/// Each registered socket. The thread keeps how each one stands; this lock is only held to copy.
static SOCKETS: Mutex<Vec<qos::Target>> = Mutex::new(Vec::new());

/// A socket registered for marking, to tell where its packets go when that changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark(usize);

impl Mark {
    /// The socket now sends to `dest`. Cheap when it's where it sent before.
    pub fn sends_to(self, dest: SocketAddr) {
        let changed = {
            let mut sockets = SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
            match sockets.get_mut(self.0) {
                Some(target) if target.dest != Some(dest) => {
                    target.dest = Some(dest);
                    true
                }
                _ => false,
            }
        };
        if changed {
            wake();
        }
    }
}

/// Registers a socket for marking, with where it sends if that's fixed. The socket must stay open
/// as long as the service runs, as the service's sockets do.
pub fn mark(socket: &std::net::UdpSocket, class: Class, name: &'static str, dest: Option<SocketAddr>) -> Mark {
    #[cfg(windows)]
    let raw = std::os::windows::io::AsRawSocket::as_raw_socket(socket);
    #[cfg(not(windows))]
    let raw = std::os::fd::AsRawFd::as_raw_fd(socket) as u64;
    let target = qos::Target { name, class, raw, dest };
    let mark = {
        let mut sockets = SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        sockets.push(target);
        Mark(sockets.len() - 1)
    };
    wake();
    mark
}

/// The setting changed: the thread brings everything in line.
pub fn set_enabled(on: bool) {
    LOW_LATENCY_WIFI.store(on, Ordering::SeqCst);
    wake();
}

pub fn enabled() -> bool {
    LOW_LATENCY_WIFI.load(Ordering::SeqCst)
}

fn wake() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.try_send(Msg::Wake);
    }
}

/// Starts the thread, once. Until it runs, which it never does in tests, nothing is changed.
pub fn start() {
    let (tx, rx) = sync_channel(64);
    if WAKE.set(tx).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("mesh-kvm-lowlat".into()).spawn(move || run(rx));
}

/// The service is going: streaming mode goes off, waited for briefly. Process exit would close
/// the handle anyway; this says so in the log first.
pub fn stop() {
    let Some(tx) = WAKE.get() else { return };
    let (done, wait) = sync_channel(1);
    if tx.try_send(Msg::Stop(done)).is_ok() {
        let _ = wait.recv_timeout(STOP_WAIT);
    }
}

/// Undoes `stop`, for a service that carries on after a restart failed.
pub fn resume() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.try_send(Msg::Resume);
    }
}

fn log(word: &'static str, pairs: policy::Pairs) {
    let details = pairs.into_iter().fold(crate::linklog::Details::new(), |d, (k, v)| d.text(k, &v));
    crate::linklog::record(word, details);
}

#[cfg(windows)]
fn backends() -> (Option<Box<dyn policy::Wlan>>, Box<dyn qos::Marker>) {
    (Some(Box::new(win::NativeWifi::new())), Box::new(win::QwaveMarker::new()))
}

#[cfg(target_os = "linux")]
fn backends() -> (Option<Box<dyn policy::Wlan>>, Box<dyn qos::Marker>) {
    // Only the laptop's scans showed up as spikes, and NetworkManager has no per-app switch for them.
    (None, Box::new(qos::TosMarker))
}

fn run(rx: Receiver<Msg>) {
    let (mut wlan, mut marker) = backends();
    let mut wifi = policy::WifiPolicy::default();
    let mut entries: Vec<qos::Entry> = Vec::new();
    let mut stopped = false;
    loop {
        let stopping = match rx.recv_timeout(LOOK_EVERY) {
            Ok(Msg::Stop(done)) => Some(done),
            Ok(Msg::Resume) => {
                stopped = false;
                None
            }
            Ok(Msg::Wake) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if stopped {
            continue;
        }
        if let Some(wlan) = wlan.as_deref_mut() {
            let said = match stopping {
                Some(_) => wifi.stop(wlan),
                None => wifi.step(look(), wlan),
            };
            if let Some(pairs) = said {
                log("wifi_mode", pairs);
            }
        }
        if let Some(done) = stopping {
            let _ = done.send(());
            // Nothing goes back on after a stop, as the process is on its way out, unless a
            // restart fails and it carries on. Packet marks stay: they go with the sockets.
            stopped = true;
            continue;
        }
        let on = enabled();
        let targets = SOCKETS.lock().unwrap_or_else(PoisonError::into_inner).clone();
        for (i, target) in targets.into_iter().enumerate() {
            match entries.get_mut(i) {
                Some(entry) => entry.target = target,
                None => entries.push(qos::Entry::new(target)),
            }
        }
        for entry in entries.iter_mut() {
            if let Some(pairs) = entry.sync(on, marker.as_mut()) {
                log("qos", pairs);
            }
        }
    }
}

fn look() -> policy::Look {
    policy::Look {
        now: Instant::now(),
        link_up: crate::audio::AUDIO_PEER_ONLINE.load(Ordering::Relaxed),
        setting: enabled(),
        net_changes: crate::linklog::network_changes(),
        sleeps: crate::linklog::sleeps(),
    }
}
