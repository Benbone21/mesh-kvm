//! Platform input backend layer: traits the OS code implements, and `apply`, which turns
//! router `Action`s into backend calls and peer packets.

#[cfg(target_os = "linux")]
pub mod debian_edge;
#[cfg(target_os = "linux")]
pub mod grab;
pub mod link;
#[cfg(test)]
mod live;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod win_logic;
#[cfg(target_os = "windows")]
pub mod windows;

use self::link::Unopened;
use crate::protocol::{NodeId, Packet, Payload, PressEvent, PACKET_MAGIC};
use crate::router::Action;
use crate::secure::datagram::{Channel, Opened};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;

/// True while this device hides its own parked cursor, as the other device has control.
pub fn cursor_hidden() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::cursor_cloaked()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Times focus and panic packets are sent, because the UDP link is lossy.
const REPEAT_CRITICAL: usize = 3;

pub trait InputBackend {
    fn set_blocking(&mut self, mouse: bool, keyboard: bool);
    fn inject_move(&mut self, dx: i32, dy: i32);
    fn inject_button(&mut self, button: u8, pressed: bool);
    fn inject_wheel(&mut self, dx: i32, dy: i32);
    fn inject_key(&mut self, scancode: u16, pressed: bool);
    fn warp_cursor(&mut self, x: i32, y: i32);
    fn release_held(&mut self);
}

pub trait PeerSender {
    fn send(&mut self, payload: Payload);

    /// The numbered packet for a key or button, or `None` for a peer that can only read the plain one.
    fn number_press(&mut self, _event: PressEvent) -> Option<Payload> {
        None
    }
}

/// Sends a key or button. A numbered press can't be lost short of every copy and every later press
/// going, and the other device counts copies once. A plain one can't be repeated if it's a press,
/// as that looks like auto-repeat, and a lost release would stick the key, so releases are.
fn forward_press(peer: &mut dyn PeerSender, event: PressEvent, plain: Payload) {
    if let Some(numbered) = peer.number_press(event) {
        for _ in 0..REPEAT_CRITICAL {
            peer.send(numbered.clone());
        }
        return;
    }
    let copies = if event.pressed { 1 } else { REPEAT_CRITICAL };
    for _ in 0..copies {
        peer.send(plain.clone());
    }
}

/// True for a packet from the configured peer's IP. Its source port may vary.
pub fn from_peer(src: SocketAddr, peer: SocketAddr) -> bool {
    src.ip() == peer.ip()
}

/// The link's socket and what encrypts it, so that nothing goes over the network in the clear.
pub struct PeerSocket {
    pub socket: UdpSocket,
    /// Input always goes to the peer's listening port, so nothing here follows where packets come from.
    peer: SocketAddr,
    pub channel: Arc<Channel>,
}

impl PeerSocket {
    pub fn new(socket: UdpSocket, peer: SocketAddr, channel: Arc<Channel>) -> PeerSocket {
        PeerSocket { socket, peer, channel }
    }

    /// With no session yet, this is a lost packet. Safe from a hook, on a non-blocking socket.
    pub fn send(&self, bytes: &[u8]) {
        let mut sealed = Vec::new();
        if self.channel.seal(bytes, &mut sealed) {
            match self.socket.send_to(&sealed, self.peer) {
                Ok(_) => crate::linklog::packet_out(),
                Err(e) => crate::linklog::send_error(&e),
            }
        }
    }

    /// Takes in a datagram, answering it if it needs that. Only a packet that checked out comes
    /// back, so whatever counts as hearing the peer goes after this. `note` hears of anything from
    /// the peer's address that says it's on another version. Handshakes happen here, so this is for
    /// the receive thread and never a hook.
    pub fn open(&self, datagram: &[u8], from: SocketAddr, now_ms: u64, plain: &mut Vec<u8>, note: &mut dyn FnMut(Unopened)) -> Option<Packet> {
        if !from_peer(from, self.peer) {
            return None;
        }
        match self.channel.open(datagram, now_ms, plain) {
            Opened::Data(bytes) => {
                crate::linklog::packet_in();
                let packet = Packet::decode(bytes);
                if packet.is_err() && bytes.starts_with(&PACKET_MAGIC) {
                    note(Unopened::Unreadable);
                }
                packet.ok()
            }
            Opened::Reply(reply) => {
                if let Err(e) = self.socket.send_to(reply, from) {
                    crate::linklog::send_error(&e);
                }
                None
            }
            Opened::OldPeer => {
                note(Unopened::OldPeer);
                None
            }
            Opened::Heard | Opened::Refused => None,
        }
    }

    /// Sends what the channel has due. As with `open`, this is for the receive thread.
    pub fn tick(&self, now_ms: u64) {
        self.channel.tick(now_ms, &mut |packet| {
            if let Err(e) = self.socket.send_to(packet, self.peer) {
                crate::linklog::send_error(&e);
            }
        });
    }

    /// When a handshake was last heard, if that's news since `known`.
    pub fn handshake_news(&self, known: &mut Option<u64>) -> Option<u64> {
        let heard = self.channel.handshake_heard_at();
        if heard == *known {
            return None;
        }
        *known = heard;
        heard
    }
}

/// Runs router actions against the local backend and the peer link. `x`/`y` on a focus
/// switch pass through unchanged, including `router::KEEP_CURSOR`.
pub fn apply(actions: &[Action], backend: &mut dyn InputBackend, peer: &mut dyn PeerSender) {
    for action in actions {
        match *action {
            Action::ForwardMove { dx, dy } => peer.send(Payload::MouseMoveDelta { dx, dy }),
            Action::ForwardButton { button, pressed } => {
                forward_press(peer, PressEvent::button(button, pressed), Payload::MouseButton { button, pressed });
            }
            Action::ForwardWheel { dx, dy } => peer.send(Payload::MouseWheel {
                delta_x: dx,
                delta_y: dy,
            }),
            Action::ForwardKey { scancode, pressed } => {
                forward_press(peer, PressEvent::key(scancode, pressed), Payload::KeyEvent { scancode, pressed });
            }
            Action::InjectMove { dx, dy } => backend.inject_move(dx, dy),
            Action::InjectButton { button, pressed } => backend.inject_button(button, pressed),
            Action::InjectWheel { dx, dy } => backend.inject_wheel(dx, dy),
            Action::InjectKey { scancode, pressed } => backend.inject_key(scancode, pressed),
            Action::SendFocus { target, x, y } => {
                // An unknown device id has no wire representation, so nothing is sent.
                if let Ok(target_node) = NodeId::try_from(target) {
                    for _ in 0..REPEAT_CRITICAL {
                        peer.send(Payload::FocusSwitchAt { target_node, x, y });
                    }
                }
            }
            Action::SendPanic => {
                for _ in 0..REPEAT_CRITICAL {
                    peer.send(Payload::PanicRelease);
                }
            }
            Action::SendActiveView { active } => {
                if let Ok(active_node) = NodeId::try_from(active) {
                    peer.send(Payload::ActiveView { active_node });
                }
            }
            Action::WarpCursor { x, y } => backend.warp_cursor(x, y),
            Action::ReleaseHeld => backend.release_held(),
            Action::SetBlocking { mouse, keyboard } => backend.set_blocking(mouse, keyboard),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_DATAGRAM;
    use crate::router::KEEP_CURSOR;
    use crate::secure::datagram::{Label, Role};
    use crate::secure::keys::{Pairing, PeerKey};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeBackend(Vec<String>);
    impl InputBackend for FakeBackend {
        fn set_blocking(&mut self, mouse: bool, keyboard: bool) {
            self.0.push(format!("block {mouse} {keyboard}"));
        }
        fn inject_move(&mut self, dx: i32, dy: i32) {
            self.0.push(format!("move {dx} {dy}"));
        }
        fn inject_button(&mut self, button: u8, pressed: bool) {
            self.0.push(format!("button {button} {pressed}"));
        }
        fn inject_wheel(&mut self, dx: i32, dy: i32) {
            self.0.push(format!("wheel {dx} {dy}"));
        }
        fn inject_key(&mut self, scancode: u16, pressed: bool) {
            self.0.push(format!("key {scancode} {pressed}"));
        }
        fn warp_cursor(&mut self, x: i32, y: i32) {
            self.0.push(format!("warp {x} {y}"));
        }
        fn release_held(&mut self) {
            self.0.push("release".into());
        }
    }

    #[derive(Default)]
    struct FakePeer(Vec<Payload>);
    impl PeerSender for FakePeer {
        fn send(&mut self, payload: Payload) {
            self.0.push(payload);
        }
    }

    #[test]
    fn only_the_configured_peer_ip_is_heard() {
        let peer: SocketAddr = "192.168.8.210:24850".parse().unwrap();
        assert!(from_peer("192.168.8.210:24850".parse().unwrap(), peer));
        assert!(from_peer("192.168.8.210:50000".parse().unwrap(), peer));
        assert!(!from_peer("192.168.8.211:24850".parse().unwrap(), peer));
    }

    fn bound(ip: &str) -> UdpSocket {
        let socket = UdpSocket::bind((ip, 0)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        socket
    }

    fn peer_socket(pairing: &Pairing, socket: &UdpSocket, peer: &UdpSocket) -> PeerSocket {
        let channel = Arc::new(Channel::new(pairing, Label::Input, Role::Both));
        PeerSocket::new(socket.try_clone().unwrap(), peer.local_addr().unwrap(), channel)
    }

    /// Two ends that know each other, with sockets, and no session yet.
    fn unlinked() -> (PeerSocket, PeerSocket) {
        let (mut a, mut b) = (Pairing::fresh(), Pairing::fresh());
        a.peer = Some(PeerKey { public: b.identity.public, name: "B".to_string(), paired_at: 1 });
        b.peer = Some(PeerKey { public: a.identity.public, name: "A".to_string(), paired_at: 1 });
        let (socket_a, socket_b) = (bound("127.0.0.1"), bound("127.0.0.1"));
        (peer_socket(&a, &socket_a, &socket_b), peer_socket(&b, &socket_b, &socket_a))
    }

    /// The next datagram at `to`: the packet for the caller, if it was one, and what else it said.
    fn take(to: &PeerSocket, now_ms: u64) -> (Option<Packet>, Option<Unopened>) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (len, from) = to.socket.recv_from(&mut buf).unwrap();
        let mut noted = None;
        let packet = to.open(&buf[..len], from, now_ms, &mut Vec::new(), &mut |what| noted = Some(what));
        (packet, noted)
    }

    /// Handshake 1, handshake 2, and the keepalive that confirms it.
    fn linked() -> (PeerSocket, PeerSocket) {
        let (a, b) = unlinked();
        a.tick(1);
        assert!(take(&b, 1).0.is_none());
        assert!(take(&a, 1).0.is_none());
        assert!(take(&b, 1).0.is_none());
        (a, b)
    }

    fn heartbeat() -> Vec<u8> {
        Packet::new(1, NodeId::Debian, Payload::Heartbeat).encode().unwrap()
    }

    #[test]
    fn a_sealed_packet_comes_out_the_other_end() {
        let (a, b) = linked();
        a.send(&heartbeat());
        let (packet, noted) = take(&b, 2);
        assert!(matches!(packet.map(|p| p.payload), Some(Payload::Heartbeat)));
        assert_eq!(noted, None);
    }

    #[test]
    fn nothing_goes_out_with_no_session() {
        let (a, b) = unlinked();
        a.send(&heartbeat());
        b.socket.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        assert!(b.socket.recv_from(&mut [0u8; MAX_DATAGRAM]).is_err());
    }

    #[test]
    fn a_keepalive_is_no_packet() {
        let (a, b) = linked();
        let mut keepalive = Vec::new();
        assert!(a.channel.seal(&[], &mut keepalive));
        a.socket.send_to(&keepalive, a.peer).unwrap();
        let (packet, noted) = take(&b, 2);
        assert!(packet.is_none());
        assert_eq!(noted, None);
    }

    #[test]
    fn a_replayed_packet_comes_out_once() {
        let (a, b) = linked();
        let mut sealed = Vec::new();
        assert!(a.channel.seal(&heartbeat(), &mut sealed));
        a.socket.send_to(&sealed, a.peer).unwrap();
        a.socket.send_to(&sealed, a.peer).unwrap();
        assert!(take(&b, 2).0.is_some());
        assert!(take(&b, 2).0.is_none());
    }

    #[test]
    fn a_plain_packet_on_a_linked_pair_is_a_note_and_changes_nothing() {
        let (a, b) = linked();
        let (mut link, mut sender) = (link::PeerLink::default(), link::PeerLink::default());
        link.heard_state(&sender.next_state(Default::default(), false, false, 0, 2), 2);
        a.socket.send_to(&heartbeat(), a.peer).unwrap();

        let (packet, noted) = take(&b, 2);
        assert!(packet.is_none());
        link.note(noted.unwrap(), 2);
        assert!(link.old_peer(2));
        assert_eq!(link.peer_version(2), link::PeerVersion::Same, "so presses stay numbered");

        a.send(&heartbeat());
        assert!(take(&b, 3).0.is_some(), "and the session carries on");
    }

    #[test]
    fn a_session_packet_that_cant_be_read_says_other_version() {
        let (a, b) = linked();
        a.send(&PACKET_MAGIC);
        let (packet, noted) = take(&b, 2);
        assert!(packet.is_none());
        assert_eq!(noted, Some(Unopened::Unreadable));
        a.send(b"not one of ours");
        assert_eq!(take(&b, 2).1, None);
    }

    #[test]
    fn a_stranger_is_not_listened_to_at_all() {
        let (_a, b) = linked();
        let mut noted = None;
        let from = "192.0.2.1:24850".parse().unwrap();
        assert!(b.open(&heartbeat(), from, 2, &mut Vec::new(), &mut |what| noted = Some(what)).is_none());
        assert_eq!(noted, None);
    }

    #[test]
    fn a_handshake_is_answered_to_where_it_came_from() {
        let (a, b) = unlinked();
        a.tick(1);
        let mut buf = [0u8; MAX_DATAGRAM];
        let (len, _) = b.socket.recv_from(&mut buf).unwrap();
        // From the peer's address, on another port.
        let elsewhere = bound("127.0.0.1");
        let from = elsewhere.local_addr().unwrap();
        assert!(b.open(&buf[..len], from, 1, &mut Vec::new(), &mut |_| ()).is_none());
        assert!(elsewhere.recv_from(&mut buf).is_ok());
    }

    #[test]
    fn a_heard_handshake_is_news_once() {
        let (a, b) = unlinked();
        let mut known = None;
        assert_eq!(b.handshake_news(&mut known), None);
        a.tick(1);
        take(&b, 5);
        assert_eq!(b.handshake_news(&mut known), Some(5));
        assert_eq!(b.handshake_news(&mut known), None);
    }

    #[derive(Default)]
    struct NumberingPeer(Vec<Payload>, link::PressLog);
    impl PeerSender for NumberingPeer {
        fn send(&mut self, payload: Payload) {
            self.0.push(payload);
        }
        fn number_press(&mut self, event: PressEvent) -> Option<Payload> {
            self.1.record(event, 0)
        }
    }

    #[test]
    fn presses_and_releases_both_go_as_numbered_copies_to_a_peer_that_reads_them() {
        let mut peer = NumberingPeer::default();
        peer.1.set_reliable(true);
        let actions = [Action::ForwardKey { scancode: 30, pressed: true }, Action::ForwardButton { button: 1, pressed: true }];
        apply(&actions, &mut FakeBackend::default(), &mut peer);
        let key = PressEvent::key(30, true);
        let button = PressEvent::button(1, true);
        let mut want = vec![Payload::Presses { newest: 1, sent_ms: 0, events: vec![key] }; 3];
        want.extend(vec![Payload::Presses { newest: 2, sent_ms: 0, events: vec![key, button] }; 3]);
        assert_eq!(format!("{:?}", peer.0), format!("{:?}", want));
    }

    #[test]
    fn apply_dispatches_every_action() {
        let actions = [
            Action::ForwardMove { dx: 1, dy: -2 },
            Action::ForwardButton { button: 3, pressed: true },
            Action::ForwardButton { button: 2, pressed: false },
            Action::ForwardWheel { dx: 4, dy: 5 },
            Action::ForwardKey { scancode: 30, pressed: false },
            Action::InjectMove { dx: 6, dy: 7 },
            Action::InjectButton { button: 1, pressed: false },
            Action::InjectWheel { dx: 8, dy: 9 },
            Action::InjectKey { scancode: 31, pressed: true },
            Action::SendFocus { target: 1, x: 10, y: KEEP_CURSOR },
            Action::SendPanic,
            Action::SendActiveView { active: 1 },
            Action::WarpCursor { x: 11, y: 12 },
            Action::ReleaseHeld,
            Action::SetBlocking { mouse: true, keyboard: false },
        ];
        let mut b = FakeBackend::default();
        let mut p = FakePeer::default();
        apply(&actions, &mut b, &mut p);

        assert_eq!(
            b.0,
            vec![
                "move 6 7",
                "button 1 false",
                "wheel 8 9",
                "key 31 true",
                "warp 11 12",
                "release",
                "block true false",
            ]
        );

        let focus = Payload::FocusSwitchAt { target_node: NodeId::Windows, x: 10, y: KEEP_CURSOR };
        // Payload has no PartialEq, so compare Debug output.
        let mut want = vec![
            Payload::MouseMoveDelta { dx: 1, dy: -2 },
            Payload::MouseButton { button: 3, pressed: true },
            Payload::MouseButton { button: 2, pressed: false },
            Payload::MouseButton { button: 2, pressed: false },
            Payload::MouseButton { button: 2, pressed: false },
            Payload::MouseWheel { delta_x: 4, delta_y: 5 },
            Payload::KeyEvent { scancode: 30, pressed: false },
            Payload::KeyEvent { scancode: 30, pressed: false },
            Payload::KeyEvent { scancode: 30, pressed: false },
        ];
        want.extend(std::iter::repeat_with(|| focus.clone()).take(3));
        want.extend(std::iter::repeat_with(|| Payload::PanicRelease).take(3));
        // Repeated every second by the router, so one copy is enough.
        want.push(Payload::ActiveView { active_node: NodeId::Windows });
        assert_eq!(format!("{:?}", p.0), format!("{:?}", want));
    }
}
