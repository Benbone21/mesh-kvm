//! Talks to the local mesh-kvm service (and, through its peer proxy, to the other device).

use super::details::{self, Fetched};
use super::model::{older_message, release_route, Control, Device, DeviceStatus, LayoutState, Release, Snapshot};
use super::pairing::PairingStatus;
use crate::platform::DisplayInfo;
use crate::webui::{http_call_port, http_call_port_waiting, UI_PORT};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long an error stays up before a later change that works may clear it.
const ERROR_HOLD: Duration = Duration::from_secs(5);
/// How often the window polls.
pub const WINDOW_POLL: Duration = Duration::from_millis(200);
/// How often the window asks what the monitors can do. That rarely changes, and a change made
/// through the app is noticed from the status, so it stays out of the quick poll.
const DISPLAYS_EVERY: Duration = Duration::from_secs(2);
/// What a service answers for an action it doesn't have, which an older one does for newer controls.
const UNKNOWN_ACTION: &str = "unknown action";
const LOCALHOST: &str = "127.0.0.1";
/// What the peer proxy answers when the other device doesn't.
const PEER_UNREACHABLE: u16 = 502;
/// How long a pairing action may take to answer: `unpair` tells the other device, and the others
/// wait for it. The longest is about five seconds.
const PAIRING_WAIT: Duration = Duration::from_secs(6);
/// A one way link has to hold this long before the link line says so while the link is down.
const ONE_WAY_HOLD: Duration = Duration::from_secs(1);
/// How long a setting's error stays under its row when no later change clears it.
const ROW_ERROR_SHOWN: Duration = Duration::from_secs(10);
const NOT_RUNNING: &str = "mesh-kvm isn't running on this device. Start mesh-kvm, then try again.";
/// The most often this device's connection details are asked for while they're shown, and the
/// other device's, which go over the link. Each ask waits for the one before it to come back.
pub const DETAILS_EVERY: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];
/// How often a details thread that isn't wanted looks whether the client has gone.
const DETAILS_IDLE_CHECK: Duration = Duration::from_secs(1);

struct Job {
    /// The device's name as a sentence starts it, for any error.
    title: String,
    path: String,
    body: String,
    /// The setting row this change is for, so a problem is kept with that row.
    row: Option<(Device, Control)>,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    /// Why the last change didn't go through, worded for the person to read.
    last_error: Mutex<Option<(String, Instant)>>,
    /// Why a setting's last change didn't go through, kept for its row.
    row_errors: Mutex<HashMap<(Device, Control), (String, Instant)>>,
    /// Rows whose change was refused, until the window takes them to show the polled value again.
    refused: Mutex<Vec<(Device, Control)>>,
    error_hold: Duration,
    stop: AtomicBool,
    /// The window wants to see `/api/pairing`, which holds a code and so isn't asked for otherwise.
    pairing_wanted: AtomicBool,
    /// The window shows the connection details, which aren't asked for otherwise.
    details_wanted: Mutex<bool>,
    details_changed: Condvar,
    /// This device's details, then the other device's.
    details: Mutex<[Fetched; 2]>,
}

/// What the service said to a pairing action.
#[derive(Debug, Clone, PartialEq)]
pub enum PairingReply {
    Done,
    /// It said no, worded for the person to read.
    Refused(String),
    /// No answer. After a pairing the service restarts, so this is not a failure by itself.
    NoAnswer,
}

/// Tells when `one_way` has been true for `ONE_WAY_HOLD` without a break.
#[derive(Default)]
struct OneWayHold {
    since: Option<Instant>,
}

impl OneWayHold {
    fn held(&mut self, one_way: bool, now: Instant) -> bool {
        if !one_way {
            self.since = None;
            return false;
        }
        now.duration_since(*self.since.get_or_insert(now)) >= ONE_WAY_HOLD
    }
}

/// Keeps the latest `Snapshot` fresh on a background thread, and sends changes on another, so
/// the UI thread never waits on the network.
pub struct Client {
    port: u16,
    shared: Arc<Shared>,
    jobs: Sender<Job>,
}

impl Client {
    /// `on_update` runs on the poll thread after the first poll and after each change.
    pub fn start(interval: Duration, on_update: Box<dyn Fn() + Send>) -> Self {
        Self::start_holding(UI_PORT, interval, ERROR_HOLD, on_update, None)
    }

    /// `start`, and also keeps up with what the monitors can do, which only the window shows.
    pub fn start_with_displays(interval: Duration, on_update: Box<dyn Fn() + Send>) -> Self {
        Self::start_holding(UI_PORT, interval, ERROR_HOLD, on_update, Some(DISPLAYS_EVERY))
    }

    #[cfg(test)]
    pub(super) fn start_on(port: u16, on_update: Box<dyn Fn() + Send>) -> Self {
        Self::start_holding(port, WINDOW_POLL, ERROR_HOLD, on_update, Some(DISPLAYS_EVERY))
    }

    fn start_holding(
        port: u16,
        interval: Duration,
        error_hold: Duration,
        on_update: Box<dyn Fn() + Send>,
        displays_every: Option<Duration>,
    ) -> Self {
        let shared = Arc::new(Shared {
            snapshot: Mutex::default(),
            last_error: Mutex::default(),
            row_errors: Mutex::default(),
            refused: Mutex::default(),
            error_hold,
            stop: AtomicBool::new(false),
            pairing_wanted: AtomicBool::new(false),
            details_wanted: Mutex::new(false),
            details_changed: Condvar::new(),
            details: Mutex::default(),
        });
        let on_update: Arc<Mutex<Box<dyn Fn() + Send>>> = Arc::new(Mutex::new(on_update));
        for which in 0..2 {
            let (shared, on_update) = (shared.clone(), on_update.clone());
            std::thread::Builder::new()
                .name("mesh-kvm-app-details".into())
                .spawn(move || poll_details(port, which, &shared, &|| (lock(&on_update))()))
                .expect("start a details thread");
        }
        let poller = shared.clone();
        std::thread::Builder::new()
            .name("mesh-kvm-app-poll".into())
            .spawn(move || {
                let mut last: Option<(Snapshot, Option<String>)> = None;
                let mut displays = DisplayLook::default();
                let mut one_way = OneWayHold::default();
                let mut pairing: Option<PairingStatus> = None;
                while !poller.stop.load(Ordering::Relaxed) {
                    let mut snap = poll_once(port);
                    let link = snap.local.as_mut().and_then(|l| l.link.as_mut());
                    let held = one_way.held(link.as_ref().is_some_and(|l| l.one_way), Instant::now());
                    if let Some(link) = link {
                        link.one_way_held = held;
                    }
                    // A failed ask is a service that is busy or restarting, so the last answer stays.
                    let unlinked = snap.local.as_ref().is_some_and(|l| !l.secure.is_empty() && l.secure != "encrypted");
                    if unlinked || poller.pairing_wanted.load(Ordering::Relaxed) {
                        pairing = pairing_at(port).or(pairing);
                    }
                    snap.pairing = pairing.clone();
                    if let Some(every) = displays_every {
                        displays.fill(&mut snap, port, every);
                    }
                    *lock(&poller.snapshot) = snap.clone();
                    let now = (snap, lock(&poller.last_error).as_ref().map(|(m, _)| m.clone()));
                    if last.as_ref() != Some(&now) {
                        (lock(&on_update))();
                    }
                    last = Some(now);
                    std::thread::sleep(interval);
                }
            })
            .expect("start the poll thread");
        let (jobs, queue) = mpsc::channel::<Job>();
        let sender = shared.clone();
        std::thread::Builder::new()
            .name("mesh-kvm-app-send".into())
            .spawn(move || {
                for job in queue {
                    let reply = http_call_port(LOCALHOST, port, "POST", &job.path, &job.body);
                    let problem = send_problem(&job.title, reply);
                    if let Some(row) = job.row {
                        let mut errors = lock(&sender.row_errors);
                        // A change to that row that works is the answer to its old error.
                        match problem {
                            Some(message) => {
                                errors.insert(row, (message, Instant::now()));
                                lock(&sender.refused).push(row);
                            }
                            None => {
                                errors.remove(&row);
                            }
                        }
                        continue;
                    }
                    let mut error = lock(&sender.last_error);
                    match problem {
                        Some(message) => *error = Some((message, Instant::now())),
                        // A change that works clears an old error, but not one the person
                        // has had no time to read.
                        None if error.as_ref().is_some_and(|(_, at)| at.elapsed() >= sender.error_hold) => *error = None,
                        None => {}
                    }
                }
            })
            .expect("start the send thread");
        Self { port, shared, jobs }
    }

    /// Says whether the window needs the pairing status, so it is asked for only then.
    pub fn want_pairing(&self, wanted: bool) {
        self.shared.pairing_wanted.store(wanted, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn wants_pairing(&self) -> bool {
        self.shared.pairing_wanted.load(Ordering::Relaxed)
    }

    /// Says whether the connection details are shown, so they are asked for only then. Hiding
    /// them forgets the last answers, so that none are shown stale when they open again.
    pub fn want_details(&self, wanted: bool) {
        let mut was = lock(&self.shared.details_wanted);
        if *was == wanted {
            return;
        }
        *was = wanted;
        if !wanted {
            *lock(&self.shared.details) = Default::default();
        }
        self.shared.details_changed.notify_all();
    }

    #[cfg(test)]
    pub(super) fn wants_details(&self) -> bool {
        *lock(&self.shared.details_wanted)
    }

    /// This device's connection details, then the other device's, as last asked.
    pub fn details(&self) -> [Fetched; 2] {
        lock(&self.shared.details).clone()
    }

    /// Sends a pairing action such as `{"action": "pair_enter", "code": "482917"}` to this device.
    ///
    /// This can block for seconds, so the UI must call it off the UI thread.
    pub fn pairing_action(&self, action: serde_json::Value) -> PairingReply {
        let reply =
            http_call_port_waiting(LOCALHOST, self.port, "POST", "/api/local/action", &action.to_string(), PAIRING_WAIT);
        match reply {
            Ok((200, body)) => {
                let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                if body["ok"] != false {
                    return PairingReply::Done;
                }
                PairingReply::Refused(body["error"].as_str().unwrap_or("mesh-kvm didn't accept that.").to_string())
            }
            Ok(_) => PairingReply::Refused("mesh-kvm didn't accept that. Restart it, then try again.".into()),
            Err(_) => PairingReply::NoAnswer,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.snapshot).clone()
    }

    /// Why the last change didn't go through. It stays for at least five seconds, until
    /// dismissed or until a later change that works clears it after that.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.last_error).as_ref().map(|(m, _)| m.clone())
    }

    /// Why the last change to one setting didn't go through, for that setting's row. It goes
    /// when a later change to that setting works, or after `ROW_ERROR_SHOWN`.
    pub fn row_error(&self, key: (Device, Control)) -> Option<String> {
        let now = Instant::now();
        lock(&self.shared.row_errors).get(&key).and_then(|entry| still_shown(entry, now)).map(str::to_string)
    }

    /// How long until the next row error goes, so the window can be drawn again then.
    pub fn next_row_error_change(&self) -> Option<Duration> {
        let now = Instant::now();
        lock(&self.shared.row_errors)
            .values()
            .filter_map(|(_, at)| (*at + ROW_ERROR_SHOWN).checked_duration_since(now))
            .filter(|left| !left.is_zero())
            .min()
    }

    /// The settings whose change was refused since the last call, each listed once per refusal.
    pub fn take_refused(&self) -> Vec<(Device, Control)> {
        std::mem::take(&mut *lock(&self.shared.refused))
    }

    pub fn dismiss_error(&self) {
        *lock(&self.shared.last_error) = None;
    }

    /// The path to reach `device`'s own API: here, or through the peer proxy.
    fn route(&self, device: Device, path: &str) -> Option<String> {
        let snapshot = self.snapshot();
        let here = snapshot.local?.node;
        Some(if here == device.node() { path.to_string() } else { format!("/api/peer{path}") })
    }

    fn send(&self, device: Device, path: &str, body: serde_json::Value, row: Option<(Device, Control)>) {
        match self.route(device, path) {
            Some(path) => {
                let title = self.snapshot().title(device);
                let _ = self.jobs.send(Job { title, path, body: body.to_string(), row });
            }
            None => match row {
                Some(row) => {
                    lock(&self.shared.row_errors).insert(row, (NOT_RUNNING.into(), Instant::now()));
                    lock(&self.shared.refused).push(row);
                }
                None => *lock(&self.shared.last_error) = Some((NOT_RUNNING.into(), Instant::now())),
            },
        }
    }

    /// Sends an action such as `{"action": "set_autostart", "enabled": true}` to one device.
    pub fn local_action(&self, device: Device, action: serde_json::Value) {
        self.send(device, "/api/local/action", action, None);
    }

    /// Sends one setting's new value to `device`. A problem is kept for that setting's row.
    pub fn send_control(&self, device: Device, control: Control, value: f64) {
        self.send(device, "/api/local/action", control.action(value), Some((device, control)));
    }

    /// This device, which is where layout-wide actions and layout edits go.
    fn send_here(&self, path: &str, body: serde_json::Value) {
        match self.snapshot().this_device() {
            Some(device) => self.send(device, path, body, None),
            None => *lock(&self.shared.last_error) = Some((NOT_RUNNING.into(), Instant::now())),
        }
    }

    /// Sends a layout-wide action such as `toggle_focus` or `panic` to this device's service.
    pub fn master_action(&self, action: &str) {
        self.send_here("/api/action", serde_json::json!({ "action": action }));
    }

    /// Release all input on both devices, or only here if this service is too old for that.
    pub fn release_all_input(&self) {
        match release_route(&self.snapshot()) {
            Some(Release::Everywhere) => self.master_action("panic"),
            Some(Release::Only(device)) => self.local_action(device, serde_json::json!({ "action": "panic" })),
            None => {}
        }
    }

    /// Saves a layout edit on this device, which passes it on. `body` is what `layout_edit` made.
    pub fn save_layout(&self, body: serde_json::Value) {
        self.send_here("/api/layout", body);
    }

    /// Gives this device's service the other device's address. Returns the service's reason if it
    /// refuses, which is worded for the person to read.
    ///
    /// This blocks on the network, so the UI must call it off the UI thread.
    pub fn set_peer(&self, address: &str) -> Result<(), String> {
        let address = crate::settings::validate_peer(address)?;
        let body = serde_json::json!({ "action": "set_peer", "peer": address }).to_string();
        match http_call_port(LOCALHOST, self.port, "POST", "/api/local/action", &body) {
            Ok((200, reply)) => {
                let reply: serde_json::Value = serde_json::from_str(&reply).unwrap_or_default();
                if reply["ok"] == false {
                    return Err(reply["error"].as_str().unwrap_or("mesh-kvm didn't accept that address.").to_string());
                }
                Ok(())
            }
            _ => Err("mesh-kvm didn't answer on this device. Check that it's running, then try again.".to_string()),
        }
    }

    /// Makes `device` the main one on both devices, changing the other device first. Returns the
    /// first error, worded for the person to read.
    ///
    /// This blocks on the network for up to a few seconds, so the UI must call it off the UI thread.
    pub fn set_main_device(&self, device: Device) -> Result<(), String> {
        let snapshot = self.snapshot();
        if snapshot.local.is_none() {
            return Err("mesh-kvm isn't running on this device. Start mesh-kvm, then choose the main device again.".into());
        }
        if snapshot.peer.is_none() {
            return Err(format!(
                "{} is out of reach, so the main device can't change. Reconnect it, then try again.",
                snapshot.title(snapshot.in_order()[1])
            ));
        }
        let body = serde_json::json!({ "action": "set_main_device", "device": device.id() }).to_string();
        for target in [device.other(), device] {
            let path = self.route(target, "/api/local/action").ok_or("The service stopped. Start mesh-kvm, then try again.")?;
            match http_call_port(LOCALHOST, self.port, "POST", &path, &body) {
                Ok((200, reply)) => {
                    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap_or_default();
                    if reply["ok"] == false {
                        if reply["error"] == UNKNOWN_ACTION {
                            return Err(older_message(&snapshot.title(target)));
                        }
                        return Err(reply["error"].as_str().map_or_else(
                            || format!("{} didn't accept the change. Try again.", snapshot.title(target)),
                            str::to_string,
                        ));
                    }
                }
                Ok((PEER_UNREACHABLE, _)) | Err(_) => {
                    return Err(format!("Couldn't reach {}. Check that mesh-kvm is running on it, then try again.", snapshot.name(target)))
                }
                Ok((status, _)) => {
                    return Err(format!(
                        "{} didn't accept the change (status {status}). Restart mesh-kvm on it, then try again.",
                        snapshot.title(target)
                    ))
                }
            }
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.details_changed.notify_all();
    }
}

/// Asks for one device's connection details, here (`which` 0) or through the peer proxy, at most
/// once each `DETAILS_EVERY` and only while they're wanted. An ask that's slow to come back delays
/// the next rather than adding to it, and the other device's thread can wait on a slow link
/// without holding this device's up.
fn poll_details(port: u16, which: usize, shared: &Shared, on_update: &dyn Fn()) {
    let path = if which == 0 { details::PATH.to_string() } else { format!("/api/peer{}", details::PATH) };
    loop {
        {
            let wanted = lock(&shared.details_wanted);
            let (wanted, _) = shared
                .details_changed
                .wait_timeout_while(wanted, DETAILS_IDLE_CHECK, |w| !*w && !shared.stop.load(Ordering::Relaxed))
                .unwrap_or_else(|e| e.into_inner());
            if !*wanted {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                continue;
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let asked = Instant::now();
        let got = details::fetched(http_call_port(LOCALHOST, port, "GET", &path, ""), Instant::now());
        // Hidden while this was asked: the answer isn't kept. The wish is held while the answer goes
        // in, so a hide can't come between and leave it there to be shown stale.
        let wanted = lock(&shared.details_wanted);
        if *wanted {
            lock(&shared.details)[which] = got;
            drop(wanted);
            on_update();
        } else {
            drop(wanted);
        }
        let next = asked + DETAILS_EVERY[which];
        let wanted = lock(&shared.details_wanted);
        // Woken early only to be hidden, or to stop.
        let _ = shared.details_changed.wait_timeout_while(wanted, next.saturating_duration_since(Instant::now()), |w| {
            *w && !shared.stop.load(Ordering::Relaxed)
        });
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A row error's message while it is still to be shown at `now`.
fn still_shown(entry: &(String, Instant), now: Instant) -> Option<&str> {
    (now.saturating_duration_since(entry.1) < ROW_ERROR_SHOWN).then_some(entry.0.as_str())
}

/// What went wrong with a queued change, if anything. The service's own error text here is for
/// developers, such as a JSON parse error, so none of it reaches the person.
fn send_problem(title: &str, reply: Result<(u16, String), String>) -> Option<String> {
    let refused = || format!("{title} didn't accept that change. Try again, or restart mesh-kvm on it.");
    match reply {
        Ok((200, body)) => {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if body["ok"] == false && body["error"] == UNKNOWN_ACTION {
                return Some(older_message(title));
            }
            (body["ok"] == false).then(refused)
        }
        Ok((PEER_UNREACHABLE, _)) => {
            Some(format!("Couldn't reach {title}, so that change wasn't made. Check that mesh-kvm is running on it, then try again."))
        }
        Ok(_) => Some(refused()),
        Err(_) => Some("mesh-kvm didn't answer on this device, so that change may not have been made. Check that it's running, then try again.".into()),
    }
}

fn get(port: u16, path: &str) -> Option<String> {
    match http_call_port(LOCALHOST, port, "GET", path, "") {
        Ok((200, body)) => Some(body),
        _ => None,
    }
}

fn poll_once(port: u16) -> Snapshot {
    let Some(local) = get(port, "/api/local").and_then(|b| serde_json::from_str::<DeviceStatus>(&b).ok()) else {
        return Snapshot::default();
    };
    let peer = get(port, "/api/peer/api/local").and_then(|b| serde_json::from_str::<DeviceStatus>(&b).ok());
    // Every device keeps its own copy of the layout.
    let layout = get(port, "/api/state").and_then(|b| LayoutState::parse(&b));
    Snapshot { local: Some(local), peer, layout, ..Default::default() }
}

fn pairing_at(port: u16) -> Option<PairingStatus> {
    serde_json::from_str(&get(port, "/api/pairing")?).ok()
}

fn displays_at(port: u16, path: &str) -> Option<Vec<DisplayInfo>> {
    #[derive(Deserialize)]
    struct Listing {
        displays: Vec<DisplayInfo>,
    }
    let listing: Listing = serde_json::from_str(&get(port, path)?).ok()?;
    Some(listing.displays)
}

/// The last answers from `/api/displays`, fetched every so often and at once after a mode change.
#[derive(Default)]
struct DisplayLook {
    local: Vec<DisplayInfo>,
    peer: Vec<DisplayInfo>,
    looked_at: Option<Instant>,
    /// The change in the last status, to notice one starting or ending.
    change: Vec<Option<String>>,
}

impl DisplayLook {
    fn fill(&mut self, snap: &mut Snapshot, port: u16, every: Duration) {
        let change: Vec<Option<String>> = [&snap.local, &snap.peer]
            .into_iter()
            .map(|d| d.as_ref()?.display_change.change.as_ref().map(|c| c.stage.clone()))
            .collect();
        let due = self.looked_at.is_none_or(|at| at.elapsed() >= every);
        if due || change != self.change {
            self.looked_at = Some(Instant::now());
            self.local = displays_at(port, "/api/displays").unwrap_or_default();
            self.peer = if snap.peer.is_some() {
                displays_at(port, "/api/peer/api/displays").unwrap_or_default()
            } else {
                Vec::new()
            };
        }
        self.change = change;
        snap.local_displays = self.local.clone();
        snap.peer_displays = self.peer.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    type Log = Arc<Mutex<Vec<(String, String, String)>>>;

    /// A status for `stub` that closes the connection with no reply.
    const HANG_UP: u16 = 0;

    const STATE: &str = r#"{"config":{"devices":[{"node":"Debian","x":0,"y":0,"monitors":[{"name":"DP-2 (360Hz Native)","x":0,"y":0,"w":2560,"h":1440}]},{"node":"Windows","x":0,"y":1440,"monitors":[{"name":"Display 1","x":0,"y":0,"w":1920,"h":1080}]}],"attach_side":"Bottom","future_field":7},"telemetry":{"active_node":"Debian","peer_online":true}}"#;

    /// A tiny HTTP server on an ephemeral port. `route` gets (method, path, body) and returns
    /// (status, body). Every request is logged.
    fn stub(route: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static) -> (u16, Log) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Log = Arc::default();
        let (route, seen) = (Arc::new(route), log.clone());
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let (route, seen) = (route.clone(), seen.clone());
                std::thread::spawn(move || {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    let (head_end, len) = loop {
                        let n = s.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&data).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let len = text[..i]
                                .lines()
                                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                                .unwrap_or(0usize);
                            if data.len() >= i + 4 + len {
                                break (i + 4, len);
                            }
                        }
                    };
                    let text = String::from_utf8_lossy(&data).to_string();
                    let mut first = text.lines().next().unwrap_or("").split_whitespace();
                    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                    let body = String::from_utf8_lossy(&data[head_end..head_end + len]).to_string();
                    let (status, out) = route(&method, &path, &body);
                    seen.lock().unwrap().push((method, path, body));
                    if status == HANG_UP {
                        return;
                    }
                    let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
                });
            }
        });
        (port, log)
    }

    fn local_json(node: &str) -> String {
        format!(r#"{{"node":"{node}","device_name":"{node}-box","peer_online":true}}"#)
    }

    /// A service where this device is `node` and the other answers unless `peer_status` says not.
    fn pair(node: &'static str, peer_status: u16, action_reply: fn(&str) -> String) -> (u16, Log) {
        stub(move |method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, local_json(node)),
            ("GET", "/api/peer/api/local") if peer_status == 200 => {
                (200, local_json(if node == "debian" { "windows" } else { "debian" }))
            }
            ("GET", "/api/peer/api/local") => (peer_status, r#"{"error":"peer unreachable"}"#.into()),
            ("GET", "/api/state") => (200, STATE.into()),
            ("POST", p) => (200, action_reply(p)),
            _ => (404, "{}".into()),
        })
    }

    fn ok_reply(_: &str) -> String {
        r#"{"ok":true}"#.into()
    }

    fn start(port: u16) -> (Client, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let client = Client::start_on(port, Box::new(move || {
            let _ = tx.send(());
        }));
        (client, rx)
    }

    fn start_holding(port: u16, hold: Duration) -> Client {
        Client::start_holding(port, WINDOW_POLL, hold, Box::new(|| {}), None)
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn ready(client: &Client) {
        wait_for("a full snapshot", || {
            let s = client.snapshot();
            s.local.is_some() && s.peer.is_some() && s.layout.is_some()
        });
    }

    fn posts(log: &Log) -> Vec<(String, String)> {
        log.lock().unwrap().iter().filter(|r| r.0 == "POST").map(|r| (r.1.clone(), r.2.clone())).collect()
    }

    #[test]
    fn unreachable_service_gives_empty_snapshot() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (client, rx) = start(port);
        rx.recv_timeout(Duration::from_secs(1)).expect("a first update");
        assert_eq!(client.snapshot(), Snapshot::default());
    }

    #[test]
    fn peer_502_gives_no_peer() {
        let (port, _) = pair("debian", 502, ok_reply);
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        wait_for("the layout", || client.snapshot().layout.is_some());
        assert!(client.snapshot().peer.is_none());
    }

    #[test]
    fn actions_route_to_the_right_device() {
        let (port, log) = pair("windows", 200, ok_reply);
        let (client, _rx) = start(port);
        ready(&client);
        client.local_action(Device::Debian, serde_json::json!({"action": "set_autostart"}));
        client.local_action(Device::Windows, serde_json::json!({"action": "panic"}));
        client.master_action("toggle_focus");
        client.save_layout(serde_json::json!({ "edge_resistance": 60 }));
        wait_for("four posts", || posts(&log).len() == 4);
        let p = posts(&log);
        assert_eq!(p[0].0, "/api/peer/api/local/action");
        assert!(p[0].1.contains("set_autostart"));
        assert_eq!(p[1].0, "/api/local/action");
        assert_eq!(p[2].0, "/api/action");
        assert!(p[2].1.contains("toggle_focus"));
        assert_eq!(p[3].0, "/api/layout");
        let saved: serde_json::Value = serde_json::from_str(&p[3].1).unwrap();
        assert_eq!(saved["edge_resistance"], 60);
    }

    #[test]
    fn either_device_sends_layout_actions_and_edits_to_itself() {
        for node in ["debian", "windows"] {
            let (port, log) = pair(node, 200, ok_reply);
            let (client, _rx) = start(port);
            ready(&client);
            client.master_action("panic");
            client.save_layout(serde_json::json!({ "edge_resistance": 60 }));
            wait_for("the posts", || posts(&log).len() == 2);
            assert_eq!(posts(&log)[0].0, "/api/action", "{node}");
            assert_eq!(posts(&log)[1].0, "/api/layout", "{node}");
        }
    }

    #[test]
    fn the_layout_is_read_here_even_with_the_other_device_gone() {
        for node in ["debian", "windows"] {
            let (port, log) = pair(node, 502, ok_reply);
            let (client, _rx) = start(port);
            wait_for("the layout", || client.snapshot().layout.is_some());
            let asked: Vec<String> = log.lock().unwrap().iter().map(|r| r.1.clone()).collect();
            assert!(asked.iter().all(|path| path != "/api/peer/api/state"), "{asked:?}");
        }
    }

    #[test]
    fn an_address_goes_to_this_device_and_a_refusal_comes_back_as_written() {
        let (port, log) = pair("debian", 502, |_| r#"{"ok":false,"error":"That's this device's own address."}"#.into());
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        assert_eq!(client.set_peer("192.168.8.210").unwrap_err(), "That's this device's own address.");
        let p = posts(&log);
        assert_eq!(p[0].0, "/api/local/action");
        let body: serde_json::Value = serde_json::from_str(&p[0].1).unwrap();
        assert_eq!((body["action"].as_str(), body["peer"].as_str()), (Some("set_peer"), Some("192.168.8.210")));
    }

    #[test]
    fn a_bad_address_never_leaves_the_app_and_a_good_one_is_put_in_plain_form() {
        let (port, log) = pair("debian", 502, ok_reply);
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        assert!(client.set_peer("my laptop").unwrap_err().contains("like 192.168.1.20"));
        assert!(posts(&log).is_empty());
        client.set_peer(" 192.168.8.210:24850 ").unwrap();
        assert!(posts(&log)[0].1.contains(r#""peer":"192.168.8.210""#), "{:?}", posts(&log));
    }

    #[test]
    fn an_address_with_no_service_says_what_to_do() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (client, rx) = start(port);
        rx.recv_timeout(Duration::from_secs(1)).expect("a first update");
        assert!(client.set_peer("192.168.8.210").unwrap_err().contains("didn't answer"));
    }

    #[test]
    fn main_device_changes_the_other_device_first() {
        let (port, log) = pair("debian", 200, ok_reply);
        let (client, _rx) = start(port);
        ready(&client);
        client.set_main_device(Device::Debian).unwrap();
        let p = posts(&log);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].0, "/api/peer/api/local/action");
        assert_eq!(p[1].0, "/api/local/action");
        let body: serde_json::Value = serde_json::from_str(&p[1].1).unwrap();
        assert_eq!((body["action"].as_str(), body["device"].as_u64()), (Some("set_main_device"), Some(0)));
    }

    #[test]
    fn main_device_returns_the_first_error() {
        // The peer (changed first) refuses, so the local device is never asked.
        let (port, log) = pair("debian", 200, |path| {
            if path.starts_with("/api/peer") {
                r#"{"ok":false,"error":"The devices aren't linked. Reconnect them, then change the main device."}"#.into()
            } else {
                r#"{"ok":true}"#.into()
            }
        });
        let (client, _rx) = start(port);
        ready(&client);
        let err = client.set_main_device(Device::Debian).unwrap_err();
        assert!(err.contains("aren't linked"), "{err}");
        assert_eq!(posts(&log).len(), 1);
        // The second post failing is reported too.
        let (port, log) = pair("debian", 200, |path| {
            if path.starts_with("/api/peer") {
                r#"{"ok":true}"#.into()
            } else {
                r#"{"ok":false,"error":"nope"}"#.into()
            }
        });
        let (client, _rx) = start(port);
        ready(&client);
        assert_eq!(client.set_main_device(Device::Debian).unwrap_err(), "nope");
        assert_eq!(posts(&log).len(), 2);
    }

    #[test]
    fn main_device_refuses_when_the_peer_is_unreachable() {
        let (port, log) = pair("debian", 502, ok_reply);
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        let err = client.set_main_device(Device::Windows).unwrap_err();
        assert!(err.contains("out of reach"), "{err}");
        assert!(posts(&log).is_empty());
    }

    #[test]
    fn main_device_errors_leave_out_transport_details() {
        let (port, _) = stub(|method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, local_json("debian")),
            ("GET", "/api/peer/api/local") => (200, local_json("windows")),
            ("GET", "/api/state") => (200, STATE.into()),
            _ => (HANG_UP, String::new()),
        });
        let (client, _rx) = start(port);
        ready(&client);
        let err = client.set_main_device(Device::Debian).unwrap_err();
        assert_eq!(err, "Couldn't reach windows-box. Check that mesh-kvm is running on it, then try again.");
    }

    #[test]
    fn a_refused_change_sets_a_plain_error_and_wakes_the_window() {
        let (port, _) = pair("debian", 200, |_| r#"{"ok":false,"error":"bad request"}"#.into());
        let (client, rx) = start(port);
        ready(&client);
        while rx.try_recv().is_ok() {}
        client.local_action(Device::Windows, serde_json::json!({"action": "nonsense"}));
        wait_for("the error", || client.last_error().is_some());
        let err = client.last_error().unwrap();
        assert!(err.starts_with("windows-box didn't accept"), "{err}");
        assert!(!err.contains("bad request"), "{err}");
        rx.recv_timeout(Duration::from_secs(1)).expect("an update for the error");
    }

    #[test]
    fn release_all_input_falls_back_to_this_device_without_the_layout() {
        let (port, log) = stub(|method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, local_json("windows")),
            ("POST", _) => (200, r#"{"ok":true}"#.into()),
            _ => (404, "{}".into()),
        });
        let (client, _rx) = start(port);
        wait_for("the local status", || client.snapshot().local.is_some());
        client.release_all_input();
        wait_for("the request", || log.lock().unwrap().iter().any(|(m, _, _)| m == "POST"));
        let posts: Vec<_> = log.lock().unwrap().iter().filter(|(m, _, _)| m == "POST").cloned().collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1, "/api/local/action");
        assert!(posts[0].2.contains("panic"));
    }

    #[test]
    fn an_unknown_action_reply_means_an_older_service() {
        let (port, _) = pair("debian", 200, |_| r#"{"ok":false,"error":"unknown action"}"#.into());
        let (client, _rx) = start(port);
        ready(&client);
        client.local_action(Device::Windows, serde_json::json!({"action": "set_sharing", "mouse": false}));
        wait_for("the error", || client.last_error().is_some());
        assert_eq!(
            client.last_error().unwrap(),
            "windows-box is running an older mesh-kvm. Update it, or sign out and back in."
        );
        let err = client.set_main_device(Device::Debian).unwrap_err();
        assert_eq!(err, "windows-box is running an older mesh-kvm. Update it, or sign out and back in.");
    }

    #[test]
    fn a_refused_control_is_kept_for_its_row_and_not_the_window() {
        let (port, log) = pair("debian", 200, |_| r#"{"ok":false}"#.into());
        let (client, _rx) = start(port);
        ready(&client);
        let key = (Device::Debian, Control::Autostart);
        client.send_control(key.0, key.1, 1.0);
        wait_for("the request", || log.lock().unwrap().iter().any(|(m, _, _)| m == "POST"));
        wait_for("the row error", || client.row_error(key).is_some());
        assert!(client.row_error(key).unwrap().contains("didn't accept that change"));
        assert_eq!(client.last_error(), None);
        assert_eq!(client.take_refused(), [key]);
        assert!(client.take_refused().is_empty());
    }

    #[test]
    fn a_row_error_goes_by_itself_after_a_while() {
        let at = Instant::now();
        let entry = ("Laptop didn't accept that change.".to_string(), at);
        assert_eq!(still_shown(&entry, at + ROW_ERROR_SHOWN - Duration::from_millis(1)), Some(entry.0.as_str()));
        assert_eq!(still_shown(&entry, at + ROW_ERROR_SHOWN), None);
    }

    #[test]
    fn a_control_that_works_clears_its_rows_error_at_once() {
        let refuse = Arc::new(AtomicBool::new(true));
        let answer = refuse.clone();
        let (port, _) = stub(move |method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, local_json("debian")),
            ("GET", "/api/peer/api/local") => (200, local_json("windows")),
            ("GET", "/api/state") => (200, STATE.into()),
            ("POST", _) if answer.load(Ordering::SeqCst) => (200, r#"{"ok":false}"#.into()),
            ("POST", _) => (200, r#"{"ok":true}"#.into()),
            _ => (404, "{}".into()),
        });
        let client = start_holding(port, ERROR_HOLD);
        ready(&client);
        let key = (Device::Debian, Control::Autostart);
        client.send_control(key.0, key.1, 1.0);
        wait_for("the row error", || client.row_error(key).is_some());
        refuse.store(false, Ordering::SeqCst);
        client.send_control(key.0, key.1, 1.0);
        wait_for("the row error to clear", || client.row_error(key).is_none());
    }

    #[test]
    fn a_change_that_works_clears_the_error() {
        let (port, _) = pair("debian", 200, |path| {
            if path.ends_with("/api/layout") {
                r#"{"ok":false,"error":"missing field"}"#.into()
            } else {
                r#"{"ok":true}"#.into()
            }
        });
        let client = start_holding(port, Duration::from_millis(100));
        ready(&client);
        client.save_layout(serde_json::json!({ "edge_resistance": 60 }));
        wait_for("the error", || client.last_error().is_some());
        std::thread::sleep(Duration::from_millis(150));
        client.master_action("panic");
        wait_for("the error to clear", || client.last_error().is_none());
    }

    #[test]
    fn a_change_that_works_cannot_clear_a_fresh_error() {
        let (port, log) = pair("debian", 200, |path| {
            if path.ends_with("/api/layout") {
                r#"{"ok":false,"error":"missing field"}"#.into()
            } else {
                r#"{"ok":true}"#.into()
            }
        });
        let client = start_holding(port, Duration::from_secs(60));
        ready(&client);
        client.save_layout(serde_json::json!({ "edge_resistance": 60 }));
        wait_for("the error", || client.last_error().is_some());
        let before = client.last_error();
        client.master_action("panic");
        wait_for("the action to be sent", || posts(&log).iter().any(|(p, _)| p.ends_with("/api/action")));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(client.last_error(), before);
    }

    #[test]
    fn dismissing_clears_the_error() {
        let (port, _) = pair("debian", 200, |_| r#"{"ok":false}"#.into());
        let (client, _rx) = start(port);
        ready(&client);
        client.master_action("panic");
        wait_for("the error", || client.last_error().is_some());
        client.dismiss_error();
        assert_eq!(client.last_error(), None);
    }

    #[test]
    fn an_action_with_no_service_says_so() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (client, rx) = start(port);
        rx.recv_timeout(Duration::from_secs(1)).expect("a first update");
        client.master_action("panic");
        assert!(client.last_error().unwrap().contains("Start mesh-kvm"));
    }

    const DISPLAYS: &str = r#"{"displays":[{"id":"DP-2","name":"Big","x":0,"y":0,"can_change":true,"current":{"width":1920,"height":1080,"millihertz":60000,"id":"1"},"modes":[{"width":1920,"height":1080,"millihertz":60000,"id":"1"}]}]}"#;

    fn displays_stub(status: &'static str) -> (u16, Log) {
        stub(move |method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, status.to_string()),
            ("GET", "/api/peer/api/local") => (200, local_json("windows")),
            ("GET", "/api/state") => (200, STATE.into()),
            ("GET", "/api/displays") | ("GET", "/api/peer/api/displays") => (200, DISPLAYS.into()),
            _ => (404, "{}".into()),
        })
    }

    fn asks_for_displays(log: &Log) -> usize {
        log.lock().unwrap().iter().filter(|r| r.1 == "/api/displays").count()
    }

    #[test]
    fn monitors_are_asked_about_much_less_often_than_the_status() {
        let (port, log) = displays_stub(r#"{"node":"debian","peer_online":true}"#);
        let (client, _rx) = start(port);
        wait_for("both lists", || {
            let s = client.snapshot();
            !s.local_displays.is_empty() && !s.peer_displays.is_empty()
        });
        assert_eq!(client.snapshot().displays(Device::Windows)[0].name, "Big");
        wait_for("a few polls", || log.lock().unwrap().iter().filter(|r| r.1 == "/api/local").count() >= 4);
        assert_eq!(asks_for_displays(&log), 1, "once up to now");
    }

    #[test]
    fn the_tray_never_asks_about_monitors() {
        let (port, log) = displays_stub(r#"{"node":"debian","peer_online":true}"#);
        let client = start_holding(port, Duration::from_secs(5));
        wait_for("the local device", || client.snapshot().local.is_some());
        assert_eq!(asks_for_displays(&log), 0);
    }

    #[test]
    fn a_mode_change_starting_is_a_reason_to_look_again() {
        let (mut look, mut snap) = (DisplayLook::default(), Snapshot::default());
        let (port, log) = displays_stub("{}");
        look.fill(&mut snap, port, Duration::from_secs(60));
        look.fill(&mut snap, port, Duration::from_secs(60));
        assert_eq!(asks_for_displays(&log), 1);
        let mut status = DeviceStatus::default();
        status.display_change.change = Some(Default::default());
        snap.local = Some(status);
        look.fill(&mut snap, port, Duration::from_secs(60));
        assert_eq!(asks_for_displays(&log), 2);
    }

    #[test]
    fn send_problems_say_what_to_do_next() {
        let problem = |reply| send_problem("Desk tower", reply).unwrap();
        assert!(problem(Ok((502, "{}".into()))).contains("Couldn't reach Desk tower"));
        assert!(problem(Ok((404, "{}".into()))).contains("didn't accept"));
        let down = problem(Err("connect 127.0.0.1:24880: refused".into()));
        assert!(down.contains("Check that it's running") && !down.contains("refused"), "{down}");
        assert_eq!(send_problem("Desk tower", Ok((200, r#"{"ok":true}"#.into()))), None);
        assert_eq!(send_problem("Desk tower", Ok((200, "not json".into()))), None);
    }

    fn secure_local(secure: &str) -> String {
        format!(r#"{{"node":"debian","device_name":"debian-box","peer_online":false,"secure":"{secure}"}}"#)
    }

    fn pairing_stub(secure: &'static str) -> (u16, Log) {
        stub(move |method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, secure_local(secure)),
            ("GET", "/api/pairing") => (200, r#"{"state":"showing","code":"482917","seconds_left":90}"#.into()),
            _ => (404, "{}".into()),
        })
    }

    fn asks_for_pairing(log: &Log) -> usize {
        log.lock().unwrap().iter().filter(|r| r.1 == "/api/pairing").count()
    }

    #[test]
    fn pairing_is_not_polled_when_encrypted_and_nobody_is_looking() {
        let (port, log) = pairing_stub("encrypted");
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(asks_for_pairing(&log), 0);
        client.want_pairing(true);
        wait_for("the pairing status", || client.snapshot().pairing.is_some());
        assert_eq!(client.snapshot().pairing.unwrap().code.as_deref(), Some("482917"));
    }

    #[test]
    fn pairing_is_polled_while_the_link_is_not_encrypted() {
        for secure in ["unpaired", "connecting"] {
            let (port, log) = pairing_stub(secure);
            let (client, _rx) = start(port);
            wait_for("the pairing status", || client.snapshot().pairing.is_some());
            assert!(asks_for_pairing(&log) > 0, "{secure}");
        }
        // A service from before pairing says nothing, and has no such path.
        let (port, log) = stub(|_, path, _| if path == "/api/local" { (200, local_json("debian")) } else { (404, "{}".into()) });
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(asks_for_pairing(&log), 0);
    }

    #[test]
    fn a_failed_pairing_poll_keeps_what_was_known() {
        let up = Arc::new(AtomicBool::new(true));
        let answer = up.clone();
        let (port, _) = stub(move |method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, secure_local("unpaired")),
            ("GET", "/api/pairing") if answer.load(Ordering::SeqCst) => (200, r#"{"state":"entering"}"#.into()),
            _ => (HANG_UP, String::new()),
        });
        let (client, _rx) = start(port);
        wait_for("the pairing status", || client.snapshot().pairing.is_some());
        up.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(client.snapshot().pairing.map(|p| p.state), Some("entering".to_string()));
    }

    #[test]
    fn a_pairing_action_waits_longer_than_a_status_call_does() {
        let (port, log) = stub(|method, path, body| {
            if method == "POST" && path == "/api/local/action" {
                std::thread::sleep(Duration::from_millis(1700));
                return if body.contains("pair_enter") {
                    (200, r#"{"ok":false,"error":"The code is 6 digits."}"#.into())
                } else {
                    (200, r#"{"ok":true}"#.into())
                };
            }
            (404, "{}".into())
        });
        let (client, _rx) = start(port);
        assert_eq!(client.pairing_action(serde_json::json!({ "action": "unpair" })), PairingReply::Done);
        assert_eq!(
            client.pairing_action(serde_json::json!({ "action": "pair_enter", "code": "1" })),
            PairingReply::Refused("The code is 6 digits.".into())
        );
        assert_eq!(posts(&log).len(), 2);
    }

    #[test]
    fn a_pairing_action_with_no_service_has_no_answer() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (client, _rx) = start(port);
        assert_eq!(client.pairing_action(serde_json::json!({ "action": "pair_cancel" })), PairingReply::NoAnswer);
    }

    fn asks_for_details(log: &Log) -> [usize; 2] {
        let log = log.lock().unwrap();
        let count = |path: &str| log.iter().filter(|r| r.1 == path).count();
        [count("/api/local/link"), count("/api/peer/api/local/link")]
    }

    fn details_stub(peer_status: u16) -> (u16, Log) {
        stub(move |method, path, _| match (method, path) {
            ("GET", "/api/local") => (200, local_json("debian")),
            ("GET", "/api/local/link") => (200, r#"{"version":"0.9.4","now_ms":5,"online":true}"#.into()),
            ("GET", "/api/peer/api/local/link") => (peer_status, r#"{"error":"not found"}"#.into()),
            _ => (404, "{}".into()),
        })
    }

    #[test]
    fn the_details_are_asked_for_only_while_shown_and_at_most_once_a_second() {
        let (port, log) = details_stub(404);
        let (client, _rx) = start(port);
        wait_for("the local device", || client.snapshot().local.is_some());
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(asks_for_details(&log), [0, 0], "hidden, nothing is asked");
        client.want_details(true);
        wait_for("both answers", || client.details().iter().all(|f| *f != Fetched::Waiting));
        let [here, peer] = client.details();
        assert!(matches!(here, Fetched::Got(ref d, _) if d.version == "0.9.4"), "{here:?}");
        assert_eq!(peer, Fetched::Missing(details::Missing::NeedsUpdate), "an older device has no such path");
        std::thread::sleep(Duration::from_millis(1500));
        let [here, peer] = asks_for_details(&log);
        assert!((1..=2).contains(&here), "this device at most once a second: {here} in 1.5 s");
        assert_eq!(peer, 1, "the other device at most once each 2 s");
        client.want_details(false);
        assert_eq!(client.details(), [Fetched::Waiting, Fetched::Waiting], "nothing is kept to show stale");
        std::thread::sleep(Duration::from_millis(100));
        let hidden = asks_for_details(&log);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(asks_for_details(&log), hidden, "hidden again, nothing more is asked");
    }

    #[test]
    fn a_peer_out_of_reach_says_so_and_this_devices_details_still_come() {
        let (port, _) = details_stub(502);
        let (client, _rx) = start(port);
        client.want_details(true);
        wait_for("both answers", || client.details().iter().all(|f| *f != Fetched::Waiting));
        assert_eq!(client.details()[1], Fetched::Missing(details::Missing::OutOfReach));
        assert!(matches!(client.details()[0], Fetched::Got(..)));
    }

    #[test]
    fn a_one_way_link_counts_as_held_only_after_a_second() {
        let (mut hold, start) = (OneWayHold::default(), Instant::now());
        assert!(!hold.held(true, start));
        assert!(!hold.held(true, start + Duration::from_millis(900)));
        assert!(hold.held(true, start + Duration::from_millis(1000)));
        assert!(!hold.held(false, start + Duration::from_millis(1100)), "it cleared");
        assert!(!hold.held(true, start + Duration::from_millis(1200)), "and starts again");
        assert!(hold.held(true, start + Duration::from_millis(2300)));
    }
}
