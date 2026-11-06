//! Talks to the local mesh-kvm service (and, through its peer proxy, to the other device).

use super::model::{older_message, release_route, Device, Release, DeviceStatus, LayoutState, Snapshot};
use crate::webui::{http_call_port, UI_PORT};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an error stays up before a later change that works may clear it.
const ERROR_HOLD: Duration = Duration::from_secs(5);
/// How often the window polls.
pub const WINDOW_POLL: Duration = Duration::from_millis(200);
/// What a service answers for an action it doesn't have, which an older one does for newer controls.
const UNKNOWN_ACTION: &str = "unknown action";
const LOCALHOST: &str = "127.0.0.1";
/// What the peer proxy answers when the other device doesn't.
const PEER_UNREACHABLE: u16 = 502;
const NOT_RUNNING: &str = "mesh-kvm isn't running on this device. Start mesh-kvm, then try again.";

struct Job {
    device: Device,
    path: String,
    body: String,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    /// Why the last change didn't go through, worded for the person to read.
    last_error: Mutex<Option<(String, Instant)>>,
    error_hold: Duration,
    stop: AtomicBool,
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
        Self::start_every(UI_PORT, interval, on_update)
    }

    #[cfg(test)]
    pub(super) fn start_on(port: u16, on_update: Box<dyn Fn() + Send>) -> Self {
        Self::start_every(port, WINDOW_POLL, on_update)
    }

    fn start_every(port: u16, interval: Duration, on_update: Box<dyn Fn() + Send>) -> Self {
        Self::start_holding(port, interval, ERROR_HOLD, on_update)
    }

    fn start_holding(port: u16, interval: Duration, error_hold: Duration, on_update: Box<dyn Fn() + Send>) -> Self {
        let shared = Arc::new(Shared {
            snapshot: Mutex::default(),
            last_error: Mutex::default(),
            error_hold,
            stop: AtomicBool::new(false),
        });
        let poller = shared.clone();
        std::thread::Builder::new()
            .name("mesh-kvm-app-poll".into())
            .spawn(move || {
                let mut last: Option<(Snapshot, Option<String>)> = None;
                while !poller.stop.load(Ordering::Relaxed) {
                    let snap = poll_once(port);
                    *lock(&poller.snapshot) = snap.clone();
                    let now = (snap, lock(&poller.last_error).as_ref().map(|(m, _)| m.clone()));
                    if last.as_ref() != Some(&now) {
                        on_update();
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
                    let mut error = lock(&sender.last_error);
                    match send_problem(job.device, reply) {
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

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.snapshot).clone()
    }

    /// Why the last change didn't go through. It stays for at least five seconds, until
    /// dismissed or until a later change that works clears it after that.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.last_error).as_ref().map(|(m, _)| m.clone())
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

    fn send(&self, device: Device, path: &str, body: serde_json::Value) {
        match self.route(device, path) {
            Some(path) => {
                let _ = self.jobs.send(Job { device, path, body: body.to_string() });
            }
            None => *lock(&self.shared.last_error) = Some((NOT_RUNNING.into(), Instant::now())),
        }
    }

    /// Sends an action such as `{"action": "set_autostart", "enabled": true}` to one device.
    pub fn local_action(&self, device: Device, action: serde_json::Value) {
        self.send(device, "/api/local/action", action);
    }

    /// This device, which is where layout-wide actions and layout edits go.
    fn send_here(&self, path: &str, body: serde_json::Value) {
        match self.snapshot().this_device() {
            Some(device) => self.send(device, path, body),
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

    /// Saves a layout on this device, which passes it on. `body` is what
    /// `LayoutState::to_save_json` made.
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
                capitalised(device.other().name())
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
                            return Err(older_message(target));
                        }
                        return Err(reply["error"].as_str().map_or_else(
                            || format!("{} didn't accept the change. Try again.", capitalised(target.name())),
                            str::to_string,
                        ));
                    }
                }
                Ok((PEER_UNREACHABLE, _)) | Err(_) => {
                    return Err(format!("Couldn't reach {}. Check that mesh-kvm is running on it, then try again.", target.name()))
                }
                Ok((status, _)) => {
                    return Err(format!(
                        "{} didn't accept the change (status {status}). Restart mesh-kvm on it, then try again.",
                        capitalised(target.name())
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
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// What went wrong with a queued change, if anything. The service's own error text here is for
/// developers, such as a JSON parse error, so none of it reaches the person.
fn send_problem(device: Device, reply: Result<(u16, String), String>) -> Option<String> {
    let refused = || {
        format!("{} didn't accept that change. Try again, or restart mesh-kvm on it.", capitalised(device.name()))
    };
    match reply {
        Ok((200, body)) => {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if body["ok"] == false && body["error"] == UNKNOWN_ACTION {
                return Some(older_message(device));
            }
            (body["ok"] == false).then(refused)
        }
        Ok((PEER_UNREACHABLE, _)) => {
            Some(format!("Couldn't reach {}, so that change wasn't made. Check that mesh-kvm is running on it, then try again.", device.name()))
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
    Snapshot { local: Some(local), peer, layout }
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
        format!(r#"{{"node":"{node}","peer_online":true}}"#)
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
        Client::start_holding(port, WINDOW_POLL, hold, Box::new(|| {}))
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
        let mut layout = client.snapshot().layout.unwrap();
        layout.config.attach_side = "Right".into();
        client.save_layout(layout.to_save_json());
        wait_for("four posts", || posts(&log).len() == 4);
        let p = posts(&log);
        assert_eq!(p[0].0, "/api/peer/api/local/action");
        assert!(p[0].1.contains("set_autostart"));
        assert_eq!(p[1].0, "/api/local/action");
        assert_eq!(p[2].0, "/api/action");
        assert!(p[2].1.contains("toggle_focus"));
        assert_eq!(p[3].0, "/api/layout");
        let saved: serde_json::Value = serde_json::from_str(&p[3].1).unwrap();
        assert_eq!(saved["attach_side"], "Right");
        assert_eq!(saved["future_field"], 7);
    }

    #[test]
    fn either_device_sends_layout_actions_and_edits_to_itself() {
        for node in ["debian", "windows"] {
            let (port, log) = pair(node, 200, ok_reply);
            let (client, _rx) = start(port);
            ready(&client);
            client.master_action("panic");
            client.save_layout(client.snapshot().layout.unwrap().to_save_json());
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
        assert_eq!(err, "Couldn't reach the Windows laptop. Check that mesh-kvm is running on it, then try again.");
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
        assert!(err.starts_with("The Windows laptop didn't accept"), "{err}");
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
            "The Windows laptop is running an older mesh-kvm. Update it, or sign out and back in."
        );
        let err = client.set_main_device(Device::Debian).unwrap_err();
        assert_eq!(err, "The Windows laptop is running an older mesh-kvm. Update it, or sign out and back in.");
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
        client.save_layout(client.snapshot().layout.unwrap().to_save_json());
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
        client.save_layout(client.snapshot().layout.unwrap().to_save_json());
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

    #[test]
    fn send_problems_say_what_to_do_next() {
        let problem = |reply| send_problem(Device::Windows, reply).unwrap();
        assert!(problem(Ok((502, "{}".into()))).contains("Couldn't reach the Windows laptop"));
        assert!(problem(Ok((404, "{}".into()))).contains("didn't accept"));
        let down = problem(Err("connect 127.0.0.1:24880: refused".into()));
        assert!(down.contains("Check that it's running") && !down.contains("refused"), "{down}");
        assert_eq!(send_problem(Device::Windows, Ok((200, r#"{"ok":true}"#.into()))), None);
        assert_eq!(send_problem(Device::Windows, Ok((200, "not json".into()))), None);
    }
}
