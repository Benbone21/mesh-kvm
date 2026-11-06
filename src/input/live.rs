//! One whole device's input over the real network stack, run from `tests/netem_link.py`, which sets
//! up what it needs: `cargo test` skips it. Everything from the router to the socket is the real
//! thing, and only the keyboard and mouse are pretend: this records what would have been injected,
//! so the test can't reach the real desktop.

use super::link::PeerLink;
use super::{apply, from_peer, InputBackend, PeerSender};
use crate::protocol::{NodeId, Packet, Payload, MAX_PACKET_SIZE, PACKET_MAGIC};
use crate::router::{DeviceId, Event, Router, RouterConfig, Sharing, KEEP_CURSOR};
use std::collections::BTreeSet;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const HEARTBEAT: Duration = Duration::from_millis(400);
const STATUS_EVERY: Duration = Duration::from_millis(20);

fn number(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
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
        if pressed {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }
    fn inject_wheel(&mut self, _dx: i32, _dy: i32) {}
    fn inject_key(&mut self, scancode: u16, pressed: bool) {
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
    socket: UdpSocket,
    peer: SocketAddr,
    node: NodeId,
    seq: u64,
}

impl PeerSender for Udp {
    fn send(&mut self, payload: Payload) {
        let packet = Packet::new(self.seq, self.node, payload);
        self.seq = self.seq.wrapping_add(1);
        if let Ok(bytes) = packet.encode() {
            let _ = self.socket.send_to(&bytes, self.peer);
        }
    }
}

struct Device {
    router: Router,
    recorder: Recorder,
    udp: Udp,
    link: PeerLink,
    start: Instant,
}

impl Device {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }

    fn route(&mut self, event: Event) {
        let actions = self.router.handle(event).actions;
        apply(&actions, &mut self.recorder, &mut self.udp);
    }

    fn receive(&mut self, bytes: &[u8]) {
        let now_ms = self.now_ms();
        let Ok(packet) = Packet::decode(bytes) else {
            if bytes.starts_with(&PACKET_MAGIC) {
                self.link.unreadable(now_ms);
            }
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
            "ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
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

#[test]
#[ignore = "run by tests/netem_link.py"]
fn whole_device() {
    let me = number("MK_ME", 0) as DeviceId;
    let socket = UdpSocket::bind(("127.0.0.1", number("MK_LISTEN", 24961) as u16)).unwrap();
    socket.set_nonblocking(true).unwrap();
    let peer = SocketAddr::from(([127, 0, 0, 1], number("MK_PEER", 24962) as u16));
    let now_us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
    let node = if me == 0 { NodeId::Debian } else { NodeId::Windows };
    let mut device = Device {
        router: Router::new(RouterConfig {
            me,
            peer: 1 - me,
            main: number("MK_MAIN", 0) as DeviceId,
            sharing: Sharing { mouse: true, keyboard: true },
            pointer_speed: 1.0,
        }),
        recorder: Recorder::default(),
        udp: Udp { socket: socket.try_clone().unwrap(), peer, node, seq: number("MK_SEQ", now_us) },
        link: PeerLink::default(),
        start: Instant::now(),
    };
    if device.router.main() == me {
        device.udp.send(Payload::FocusSwitchAt { target_node: node, x: KEEP_CURSOR, y: KEEP_CURSOR });
    }

    let (lines, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });

    let end = Instant::now() + Duration::from_secs(number("MK_SECONDS", 10));
    let (mut next_beat, mut next_status) = (Instant::now(), Instant::now());
    let mut buf = [0u8; MAX_PACKET_SIZE];
    while Instant::now() < end {
        let now_ms = device.now_ms();
        device.route(Event::Tick { now_ms });
        device.link.set_online(device.router.online());
        while let Ok((len, from)) = socket.recv_from(&mut buf) {
            if from_peer(from, peer) {
                device.receive(&buf[..len]);
            }
        }
        while let Ok(line) = read.try_recv() {
            device.act(&line);
        }
        let now = Instant::now();
        if now >= next_beat {
            next_beat = now + HEARTBEAT;
            device.udp.send(Payload::Heartbeat);
            let state = device.link.next_state(device.router.held_forwarded(), device.router.panicked(), device.now_ms());
            device.udp.send(Payload::LinkState(state));
        }
        if now >= next_status {
            next_status = now + STATUS_EVERY;
            println!("STATUS {}", device.status());
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(Duration::from_micros(500));
    }
}
