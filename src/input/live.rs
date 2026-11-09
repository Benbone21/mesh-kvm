//! One whole device's input over the real network stack, run from `tests/netem_link.py`, which sets
//! up what it needs: `cargo test` skips it. Everything from the router to the socket is the real
//! thing, and only the keyboard and mouse are pretend: this records what would have been injected,
//! so the test can't reach the real desktop.

use super::link::{press_event, MotionBatch, PeerLink, PeerVersion, PressLog, REPORT_COPIES};
use super::{apply, InputBackend, PeerSender, PeerSocket};
use crate::protocol::{NodeId, Packet, Payload, PressEvent, MAX_DATAGRAM};
use crate::router::{DeviceId, Event, Router, RouterConfig, Sharing, KEEP_CURSOR};
use crate::secure::datagram::{Channel, Label, LinkSecurity, Role};
use crate::secure::keys::{Loaded, Pairing};
use std::collections::BTreeSet;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HEARTBEAT: Duration = Duration::from_millis(400);
const STATUS_EVERY: Duration = Duration::from_millis(20);
/// Where buttons start in what's logged.
const BUTTONS: u16 = 256;

fn number(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

/// An address from the environment, or loopback. Two namespaces need more than that.
fn host(name: &str) -> IpAddr {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Every key and button as it happens, for the test to check their order and how long they took.
fn log_injected(input: u16, pressed: bool) {
    println!("INJECT {}", serde_json::json!({ "ms": wall_ms(), "input": input, "pressed": pressed }));
}

#[derive(Default)]
struct Recorder {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
    blocking: (bool, bool),
    moves: u64,
    presses: u64,
}

impl InputBackend for Recorder {
    fn set_blocking(&mut self, mouse: bool, keyboard: bool) {
        self.blocking = (mouse, keyboard);
    }
    fn inject_move(&mut self, _dx: i32, _dy: i32) {
        self.moves += 1;
    }
    fn inject_button(&mut self, button: u8, pressed: bool) {
        log_injected(BUTTONS + u16::from(button), pressed);
        if pressed {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }
    fn inject_wheel(&mut self, _dx: i32, _dy: i32) {}
    fn inject_key(&mut self, scancode: u16, pressed: bool) {
        log_injected(scancode, pressed);
        if pressed {
            self.presses += u64::from(self.keys.insert(scancode));
        } else {
            self.keys.remove(&scancode);
        }
    }
    fn warp_cursor(&mut self, _x: i32, _y: i32) {}
    fn release_held(&mut self) {
        self.keys.clear();
        self.buttons.clear();
    }
}

struct Udp {
    socket: PeerSocket,
    node: NodeId,
    seq: u64,
    presses: PressLog,
    motion: MotionBatch,
    start: Instant,
    showing: Showing,
}

/// What `tests/capture_link.py` asks for: the exact bytes of each packet with keys in it, so it can
/// look for them on the wire, and for its control run those bytes going out in the clear as well.
struct Showing {
    print: bool,
    plain_to: Option<(UdpSocket, SocketAddr)>,
}

impl Showing {
    fn from_env() -> Showing {
        let plain_to = std::env::var("MK_PLAIN_TO").ok().map(|target| {
            let target: SocketAddr = target.parse().expect("MK_PLAIN_TO is an address");
            (UdpSocket::bind(("0.0.0.0", 0)).unwrap(), target)
        });
        Showing { print: std::env::var_os("MK_SHOW_KEYS").is_some(), plain_to }
    }

    fn keys_sent(&self, bytes: &[u8]) {
        if self.print {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            println!("KEYS {hex}");
        }
        if let Some((socket, target)) = &self.plain_to {
            let _ = socket.send_to(bytes, target);
        }
    }
}

impl Udp {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }

    /// Movement that waited out the gap, and presses due to go again, as the real loops send them.
    fn send_due(&mut self) {
        let now_ms = self.now_ms();
        if let Some(motion) = self.motion.due(now_ms) {
            self.send_now(motion);
        }
        if let Some(repeat) = self.presses.repeat(now_ms) {
            self.send(repeat);
        }
    }

    fn send_now(&mut self, payload: Payload) {
        let has_keys = matches!(payload, Payload::Presses { .. } | Payload::KeyEvent { .. });
        let packet = Packet::new(self.seq, self.node, payload);
        self.seq = self.seq.wrapping_add(1);
        if let Ok(bytes) = packet.encode() {
            if has_keys {
                self.showing.keys_sent(&bytes);
            }
            self.socket.send(&bytes);
        }
    }
}

impl PeerSender for Udp {
    fn send(&mut self, payload: Payload) {
        for payload in self.motion.send(payload, self.now_ms()) {
            self.send_now(payload);
        }
    }

    fn number_press(&mut self, event: PressEvent) -> Option<Payload> {
        self.presses.record(event, self.start.elapsed().as_millis() as u64 + 1)
    }
}

struct Device {
    router: Router,
    recorder: Recorder,
    udp: Udp,
    link: PeerLink,
    start: Instant,
    opened: Vec<u8>,
}

impl Device {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }

    fn route(&mut self, event: Event) {
        let actions = self.router.handle(event).actions;
        apply(&actions, &mut self.recorder, &mut self.udp);
    }

    fn receive(&mut self, datagram: &[u8], from: SocketAddr) {
        let now_ms = self.now_ms();
        let link = &mut self.link;
        let Some(packet) = self.udp.socket.open(datagram, from, now_ms, &mut self.opened, &mut |what| link.note(what, now_ms)) else {
            return;
        };
        self.route(Event::PeerAlive { now_ms });
        if !self.link.arrived(&packet, now_ms) {
            return;
        }
        match packet.payload {
            Payload::KeyEvent { scancode, pressed } => self.route(Event::PeerKey { scancode, pressed }),
            Payload::MouseMoveDelta { dx, dy } => self.route(Event::PeerMove { dx, dy }),
            Payload::MouseButton { button, pressed } => self.route(Event::PeerButton { button, pressed }),
            Payload::MouseWheel { delta_x, delta_y } => self.route(Event::PeerWheel { dx: delta_x, dy: delta_y }),
            Payload::FocusSwitchAt { target_node, x, y } => self.route(Event::PeerFocus { target: target_node as DeviceId, x, y }),
            Payload::ActiveView { active_node } => self.route(Event::PeerActiveView { active: active_node as DeviceId }),
            Payload::PanicRelease => self.route(Event::PeerPanic),
            Payload::LinkState(state) => {
                for event in self.link.heard_state(&state, now_ms) {
                    self.route(event);
                }
            }
            Payload::Presses { newest, sent_ms, events } => {
                for event in self.link.take_presses(newest, sent_ms, &events, now_ms) {
                    self.route(press_event(event));
                }
            }
            _ => {}
        }
    }

    /// Each line on stdin is one thing the person did.
    fn act(&mut self, line: &str) {
        let action: serde_json::Value = serde_json::from_str(line).unwrap();
        let pressed = action["pressed"].as_bool().unwrap_or(false);
        if let Some(scancode) = action["key"].as_u64() {
            self.route(Event::LocalKey { scancode: scancode as u16, pressed });
        } else if let Some(button) = action["button"].as_u64() {
            self.route(Event::LocalButton { button: button as u8, pressed });
        } else if let Some(dx) = action["move"].as_i64() {
            self.route(Event::LocalMove { dx: dx as i32, dy: 0 });
        } else if action["cross"].as_bool().is_some() {
            self.route(Event::EdgeCrossed { x: 1, y: 1, from_peer: false });
        } else if action["switch"].as_bool().is_some() {
            self.route(Event::ManualSwitch);
        } else if action["panic"].as_bool().is_some() {
            self.route(Event::Panic);
        }
    }

    fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "ms": wall_ms(),
            "waits": self.link.press_stats(),
            "online": self.router.online(),
            "active": self.router.active(),
            "blocking": [self.recorder.blocking.0, self.recorder.blocking.1],
            "held": self.recorder.keys,
            "buttons": self.recorder.buttons,
            "moves": self.recorder.moves,
            "presses": self.recorder.presses,
            "forwarding": self.router.held_forwarded().keys().collect::<Vec<_>>(),
            "link": self.link.quality(&self.router, self.now_ms()),
        })
    }
}

fn channel_from_test_pairing() -> Arc<Channel> {
    let path = std::env::var("MK_PAIRING").expect("MK_PAIRING names this device's pairing file");
    match Pairing::load_from(Path::new(&path)) {
        Loaded::Ready(pairing) => Arc::new(Channel::new(&pairing, Label::Input, Role::Both)),
        Loaded::Damaged(problem) => panic!("{problem}"),
    }
}

#[test]
#[ignore = "run by tests/netem_link.py"]
fn whole_device() {
    let me = number("MK_ME", 0) as DeviceId;
    let socket = UdpSocket::bind((host("MK_HOST"), number("MK_LISTEN", 24961) as u16)).unwrap();
    socket.set_nonblocking(true).unwrap();
    let peer = SocketAddr::new(host("MK_PEER_HOST"), number("MK_PEER", 24962) as u16);
    let now_us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
    let node = if me == 0 { NodeId::Debian } else { NodeId::Windows };
    let start = Instant::now();
    let mut device = Device {
        router: Router::new(RouterConfig {
            me,
            peer: 1 - me,
            main: number("MK_MAIN", 0) as DeviceId,
            sharing: Sharing { mouse: true, keyboard: true },
            pointer_speed: 1.0,
        }),
        recorder: Recorder::default(),
        udp: Udp {
            socket: PeerSocket::new(socket.try_clone().unwrap(), peer, channel_from_test_pairing()),
            node,
            seq: number("MK_SEQ", now_us),
            presses: PressLog::default(),
            motion: MotionBatch::default(),
            start,
            showing: Showing::from_env(),
        },
        link: PeerLink::default(),
        start,
        opened: Vec::new(),
    };
    let mut claim_due = true;
    let mut handshake_known = None;

    let (lines, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });

    let end = Instant::now() + Duration::from_secs(number("MK_SECONDS", 10));
    let (mut next_beat, mut next_status) = (Instant::now(), Instant::now());
    let mut buf = [0u8; MAX_DATAGRAM];
    while Instant::now() < end {
        let now_ms = device.now_ms();
        device.route(Event::Tick { now_ms });
        device.link.set_online(device.router.online());
        device.udp.presses.set_reliable(device.link.peer_version(now_ms) == PeerVersion::Same);
        if !device.router.online() {
            device.udp.presses.forget();
        }
        while let Ok((len, from)) = socket.recv_from(&mut buf) {
            device.receive(&buf[..len], from);
        }
        for event in device.link.due_presses(device.now_ms()) {
            device.route(press_event(event));
        }
        device.udp.socket.tick(device.now_ms());
        if let Some(at) = device.udp.socket.handshake_news(&mut handshake_known) {
            device.link.handshake_heard(at);
        }
        if claim_due && device.udp.socket.channel.security() == LinkSecurity::Encrypted {
            claim_due = false;
            if device.router.main() == me {
                device.udp.send(Payload::FocusSwitchAt { target_node: node, x: KEEP_CURSOR, y: KEEP_CURSOR });
            }
        }
        while let Ok(line) = read.try_recv() {
            device.act(&line);
        }
        device.udp.send_due();
        let now = Instant::now();
        if now >= next_beat {
            next_beat = now + HEARTBEAT;
            device.udp.send(Payload::Heartbeat);
            let now_ms = device.now_ms();
            if let Some(tail) = device.udp.presses.tail(now_ms) {
                device.udp.send(tail);
            }
            let order = device.udp.presses.newest();
            let main = device.router.main() == me;
            let state = device.link.next_state(device.router.held_forwarded(), device.router.panicked(), main, order, now_ms);
            for _ in 0..REPORT_COPIES {
                device.udp.send(Payload::LinkState(state));
            }
        }
        if now >= next_status {
            next_status = now + STATUS_EVERY;
            println!("STATUS {}", device.status());
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(Duration::from_micros(500));
    }
}
