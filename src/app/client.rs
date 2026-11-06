//! Talks to the local mesh-kvm service (and, through its peer proxy, to the other device).

use super::model::{DeviceStatus, LayoutState, Snapshot};
use crate::webui::{http_call_port, UI_PORT};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POLL_EVERY: Duration = Duration::from_millis(200);
const LOCALHOST: &str = "127.0.0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Debian,
    Windows,
}

impl Device {
    /// The `node` value in `/api/local`.
    fn node(self) -> &'static str {
        match self {
            Device::Debian => "debian",
            Device::Windows => "windows",
        }
    }

    /// The `device` value of `set_main_device`.
    fn id(self) -> u8 {
        match self {
            Device::Debian => 0,
            Device::Windows => 1,
        }
    }

    fn other(self) -> Device {
        match self {
            Device::Debian => Device::Windows,
            Device::Windows => Device::Debian,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Device::Debian => "the Debian desktop",
            Device::Windows => "the Windows laptop",
        }
    }
}

struct Job {
    path: String,
    body: String,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
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
    pub fn start(on_update: Box<dyn Fn() + Send>) -> Self {
        Self::start_on(UI_PORT, on_update)
    }

    fn start_on(port: u16, on_update: Box<dyn Fn() + Send>) -> Self {
        let shared = Arc::new(Shared { snapshot: Mutex::default(), stop: AtomicBool::new(false) });
        let poller = shared.clone();
        std::thread::Builder::new()
            .name("mesh-kvm-app-poll".into())
            .spawn(move || {
                let mut last: Option<Snapshot> = None;
                while !poller.stop.load(Ordering::Relaxed) {
                    let snap = poll_once(port);
                    let changed = last.as_ref() != Some(&snap);
                    *lock(&poller.snapshot) = snap.clone();
                    if changed {
                        on_update();
                    }
                    last = Some(snap);
                    std::thread::sleep(POLL_EVERY);
                }
            })
            .expect("start the poll thread");
        let (jobs, queue) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("mesh-kvm-app-send".into())
            .spawn(move || {
                for job in queue {
                    // The next poll shows whether it worked.
                    let _ = http_call_port(LOCALHOST, port, "POST", &job.path, &job.body);
                }
            })
            .expect("start the send thread");
        Self { port, shared, jobs }
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.snapshot).clone()
    }

    /// The path to reach `device`'s own API: here, or through the peer proxy.
    fn route(&self, device: Device, path: &str) -> Option<String> {
        let snapshot = self.snapshot();
        let here = snapshot.local?.node;
        Some(if here == device.node() { path.to_string() } else { format!("/api/peer{path}") })
    }

    fn send(&self, path: Option<String>, body: serde_json::Value) {
        if let Some(path) = path {
            let _ = self.jobs.send(Job { path, body: body.to_string() });
        }
    }

    /// Sends an action such as `{"action": "set_autostart", "enabled": true}` to one device.
    pub fn local_action(&self, device: Device, action: serde_json::Value) {
        self.send(self.route(device, "/api/local/action"), action);
    }

    /// Sends a layout-wide action such as `toggle_focus` or `panic`. The Debian desktop owns these.
    pub fn master_action(&self, action: &str) {
        self.send(self.route(Device::Debian, "/api/action"), serde_json::json!({ "action": action }));
    }

    pub fn save_layout(&self, layout: &LayoutState) {
        self.send(self.route(Device::Debian, "/api/layout"), layout.to_save_json());
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
                        return Err(reply["error"].as_str().map_or_else(
                            || format!("{} didn't accept the change. Try again.", capitalised(target.name())),
                            str::to_string,
                        ));
                    }
                }
                Ok((status, _)) => {
                    return Err(format!(
                        "{} didn't accept the change (status {status}). Restart mesh-kvm on it, then try again.",
                        capitalised(target.name())
                    ))
                }
                Err(e) => {
                    return Err(format!("Couldn't reach {}. Check that mesh-kvm is running on it, then try again. ({e})", target.name()))
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
    // The layout lives on the Debian desktop.
    let state_path = if local.node == "debian" { "/api/state" } else { "/api/peer/api/state" };
    let layout = get(port, state_path).and_then(|b| LayoutState::parse(&b));
    Snapshot { local: Some(local), peer, layout }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    type Log = Arc<Mutex<Vec<(String, String, String)>>>;

    const STATE: &str = r#"{"config":{"debian_monitors":[{"name":"DP-2 (360Hz Native)","x":0,"y":0,"w":2560,"h":1440}],"deb_total_w":2560,"deb_total_h":1440,"win_x":0,"win_y":1440,"win_w":1920,"win_h":1080,"attach_side":"Bottom","future_field":7},"telemetry":{"active_node":"Debian","peer_online":true}}"#;

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
            ("GET", "/api/state") | ("GET", "/api/peer/api/state") => (200, STATE.into()),
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
        layout.config.win_x = 42;
        client.save_layout(&layout);
        wait_for("four posts", || posts(&log).len() == 4);
        let p = posts(&log);
        assert_eq!(p[0].0, "/api/peer/api/local/action");
        assert!(p[0].1.contains("set_autostart"));
        assert_eq!(p[1].0, "/api/local/action");
        assert_eq!(p[2].0, "/api/peer/api/action");
        assert!(p[2].1.contains("toggle_focus"));
        assert_eq!(p[3].0, "/api/peer/api/layout");
        let saved: serde_json::Value = serde_json::from_str(&p[3].1).unwrap();
        assert_eq!(saved["win_x"], 42);
        assert_eq!(saved["future_field"], 7);
    }

    #[test]
    fn debian_client_sends_master_actions_to_itself() {
        let (port, log) = pair("debian", 200, ok_reply);
        let (client, _rx) = start(port);
        ready(&client);
        client.master_action("panic");
        wait_for("the post", || !posts(&log).is_empty());
        assert_eq!(posts(&log)[0].0, "/api/action");
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
}
