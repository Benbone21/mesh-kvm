use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub const DEFAULT_INPUT_PORT: u16 = 24850;
pub const DEFAULT_AUDIO_PORT: u16 = 24851;
pub const DEFAULT_CLIPBOARD_PORT: u16 = 24852;
pub const PACKET_MAGIC: [u8; 4] = *b"MKVM";
/// The most a packet can be: 16 monitors with long names, under one Ethernet frame.
pub const MAX_PACKET_SIZE: usize = 1400;
/// What `LinkState` carries is understood by devices on the same number. Changes when packets
/// change in a way both devices must update together for.
pub const LINK_PROTOCOL: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum NodeId {
    Debian = 0,
    Windows = 1,
}

impl NodeId {
    /// The other device of the pair.
    pub fn other(self) -> NodeId {
        match self {
            NodeId::Debian => NodeId::Windows,
            NodeId::Windows => NodeId::Debian,
        }
    }
}

impl TryFrom<u8> for NodeId {
    type Error = u8;

    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            0 => Ok(NodeId::Debian),
            1 => Ok(NodeId::Windows),
            other => Err(other),
        }
    }
}

impl FromStr for NodeId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "debian" | "linux" | "deb" => Ok(NodeId::Debian),
            "windows" | "win" | "win11" => Ok(NodeId::Windows),
            other => Err(format!(
                "Invalid role '{}': expected 'debian' or 'windows'",
                other
            )),
        }
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeId::Debian => write!(f, "Debian"),
            NodeId::Windows => write!(f, "Windows"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl FromStr for SwitchEdge {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" | "l" => Ok(SwitchEdge::Left),
            "right" | "r" => Ok(SwitchEdge::Right),
            "top" | "up" | "t" => Ok(SwitchEdge::Top),
            "bottom" | "down" | "b" => Ok(SwitchEdge::Bottom),
            other => Err(format!(
                "Invalid switch edge '{}': expected 'left', 'right', 'top', or 'bottom'",
                other
            )),
        }
    }
}

impl std::fmt::Display for SwitchEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwitchEdge::Left => write!(f, "left"),
            SwitchEdge::Right => write!(f, "right"),
            SwitchEdge::Top => write!(f, "top"),
            SwitchEdge::Bottom => write!(f, "bottom"),
        }
    }
}

/// One monitor. `x` and `y` are in its own device's desktop, and can be negative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRect {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Keys and mouse buttons that are down: evdev scancodes below 256, and buttons from 1. Nothing
/// above those can be injected on either device. A fixed size, so that it can ride in router events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeldInput {
    keys: [u64; 4],
    buttons: u8,
}

impl HeldInput {
    pub fn set_key(&mut self, scancode: u16, held: bool) {
        if let Some(word) = self.keys.get_mut(usize::from(scancode) / 64) {
            let bit = 1u64 << (scancode % 64);
            *word = if held { *word | bit } else { *word & !bit };
        }
    }

    pub fn set_button(&mut self, button: u8, held: bool) {
        if (1..=8).contains(&button) {
            let bit = 1u8 << (button - 1);
            self.buttons = if held { self.buttons | bit } else { self.buttons & !bit };
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = u16> + '_ {
        (0..256u16).filter(|code| self.keys[usize::from(*code) / 64] & (1u64 << (code % 64)) != 0)
    }

    pub fn buttons(&self) -> impl Iterator<Item = u8> + '_ {
        (1..=8u8).filter(|button| self.buttons & (1u8 << (button - 1)) != 0)
    }

    /// What's held here and not in `other`.
    pub fn without(&self, other: &HeldInput) -> HeldInput {
        let mut keys = self.keys;
        for (word, theirs) in keys.iter_mut().zip(other.keys) {
            *word &= !theirs;
        }
        HeldInput { keys, buttons: self.buttons & !other.buttons }
    }

    /// What's held in both.
    pub fn and(&self, other: &HeldInput) -> HeldInput {
        self.without(&self.without(other))
    }

    /// What's held in either.
    pub fn or(&self, other: &HeldInput) -> HeldInput {
        let mut keys = self.keys;
        for (word, theirs) in keys.iter_mut().zip(other.keys) {
            *word |= theirs;
        }
        HeldInput { keys, buttons: self.buttons | other.buttons }
    }

    pub fn without_buttons(&self) -> HeldInput {
        HeldInput { keys: self.keys, buttons: 0 }
    }
}

/// A key or button going down or up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressEvent {
    /// A scancode, or a button number if `button` says so. Linux has keys numbered above 255, so
    /// the two can't share one range.
    pub input: u16,
    pub button: bool,
    pub pressed: bool,
    /// When this happened, on the sender's clock.
    pub at_ms: u64,
}

impl PressEvent {
    pub fn key(scancode: u16, pressed: bool) -> Self {
        PressEvent { input: scancode, button: false, pressed, at_ms: 0 }
    }

    pub fn button(button: u8, pressed: bool) -> Self {
        PressEvent { input: u16::from(button), button: true, pressed, at_ms: 0 }
    }
}

/// How the link looks from one end, and what that end is holding down on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkState {
    pub protocol: u16,
    /// This heartbeat's number, from 1.
    pub beat: u32,
    /// The newest of the other device's heartbeats heard here, or 0 for none yet.
    pub echo: u32,
    /// How long ago that one was heard, so that the other device can work out the round trip.
    pub echo_age_ms: u32,
    /// The share of the other device's packets that aren't arriving here, in tenths of a percent.
    pub loss_tenths: u16,
    /// Released with panic, so this device has refused to be driven until someone says otherwise.
    pub panicked: bool,
    /// The newest numbered press this report knows of. A device that has already acted on a newer
    /// one has nothing to learn from it.
    pub press_order: u32,
    /// The sender counts itself as the main device.
    pub main: bool,
    /// When this was sent, on the sender's clock.
    pub sent_ms: u64,
    /// What this device has forwarded and not yet released.
    pub held: HeldInput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub magic: [u8; 4],
    pub seq: u64,
    pub source: NodeId,
    pub payload: Payload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    MouseMoveDelta { dx: i32, dy: i32 },
    MouseButton { button: u8, pressed: bool },
    MouseWheel { delta_x: i32, delta_y: i32 },
    KeyEvent { scancode: u16, pressed: bool },
    FocusSwitch { target_node: NodeId, cursor_y_ratio: f32 },
    FocusAck { active_node: NodeId, seq_ref: u64 },
    PanicRelease,
    Heartbeat,
    /// Each way with every heartbeat: what the sender is called, and all its monitors.
    PeerDisplayInfo { name: String, monitors: Vec<MonitorRect> },
    /// Each way with every heartbeat: which of the receiver's edges touches the sender, and
    /// where, in the receiver's desktop coordinates.
    EdgeMap { edge: SwitchEdge, start: i32, end: i32, resistance: i32 },
    /// Replaces `FocusSwitch` for all new sends; `x`, `y` are in the target's desktop coordinates.
    FocusSwitchAt { target_node: NodeId, x: i32, y: i32 },
    /// Each way with every heartbeat: the sender's manual game lock forbids leaving the active device.
    GameLock { locked: bool },
    /// Main device to the other, about once a second: which device main believes is active.
    ActiveView { active_node: NodeId },
    /// Each way with every heartbeat: the layout the sender holds. A device that sees a newer
    /// one than its own fetches it.
    LayoutRevision { revision: u64, editor: NodeId },
    /// Each way with every heartbeat. Older devices can't read this and drop it, so it never
    /// arriving is how they are told apart.
    LinkState(LinkState),
    /// Keys and buttons, numbered so that they can't be lost or land out of order. `newest` is the
    /// number of the last of `events`, and the ones before it are the events just before that,
    /// so any one packet that gets through repairs what was lost ahead of it. Older devices can't
    /// read this, so a device sends `KeyEvent` and `MouseButton` until it has heard a `LinkState`.
    /// `sent_ms` is when it left, on the sender's clock, to tell how far behind a packet is.
    Presses { newest: u32, sent_ms: u64, events: Vec<PressEvent> },
}

impl Packet {
    #[inline]
    pub fn new(seq: u64, source: NodeId, payload: Payload) -> Self {
        Self {
            magic: PACKET_MAGIC,
            seq,
            source,
            payload,
        }
    }

    #[inline]
    pub fn encode(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    #[inline]
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 4 || bytes[0..4] != PACKET_MAGIC {
            return Err("Invalid packet magic".to_string());
        }
        bincode::deserialize(bytes).map_err(|e| e.to_string())
    }
}

pub const MOUSE_BTN_LEFT: u8 = 1;
pub const MOUSE_BTN_RIGHT: u8 = 2;
pub const MOUSE_BTN_MIDDLE: u8 = 3;
pub const MOUSE_BTN_SIDE: u8 = 4;
pub const MOUSE_BTN_EXTRA: u8 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(payload: Payload) -> Payload {
        let bytes = Packet::new(1, NodeId::Debian, payload).encode().unwrap();
        Packet::decode(&bytes).unwrap().payload
    }

    #[test]
    fn new_payloads_round_trip() {
        match round_trip(Payload::EdgeMap { edge: SwitchEdge::Bottom, start: -5, end: 900, resistance: 55 }) {
            Payload::EdgeMap { edge, start, end, resistance } => {
                assert_eq!((edge, start, end, resistance), (SwitchEdge::Bottom, -5, 900, 55));
            }
            other => panic!("wrong payload: {other:?}"),
        }
        match round_trip(Payload::FocusSwitchAt { target_node: NodeId::Windows, x: 12, y: -34 }) {
            Payload::FocusSwitchAt { target_node, x, y } => {
                assert_eq!((target_node, x, y), (NodeId::Windows, 12, -34));
            }
            other => panic!("wrong payload: {other:?}"),
        }
        match round_trip(Payload::GameLock { locked: true }) {
            Payload::GameLock { locked } => assert!(locked),
            other => panic!("wrong payload: {other:?}"),
        }
        match round_trip(Payload::ActiveView { active_node: NodeId::Windows }) {
            Payload::ActiveView { active_node } => assert_eq!(active_node, NodeId::Windows),
            other => panic!("wrong payload: {other:?}"),
        }
        match round_trip(Payload::LayoutRevision { revision: u64::MAX, editor: NodeId::Windows }) {
            Payload::LayoutRevision { revision, editor } => assert_eq!((revision, editor), (u64::MAX, NodeId::Windows)),
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn link_state_round_trips_with_what_is_held() {
        let mut held = HeldInput::default();
        held.set_key(30, true);
        held.set_key(255, true);
        held.set_button(MOUSE_BTN_EXTRA, true);
        let state = LinkState { protocol: LINK_PROTOCOL, beat: 7, echo: 6, echo_age_ms: 120, loss_tenths: 25, panicked: true, press_order: 9, main: true, sent_ms: 7, held };
        match round_trip(Payload::LinkState(state)) {
            Payload::LinkState(back) => {
                assert_eq!(back, state);
                assert_eq!(back.held.keys().collect::<Vec<_>>(), vec![30, 255]);
                assert_eq!(back.held.buttons().collect::<Vec<_>>(), vec![MOUSE_BTN_EXTRA]);
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn held_input_compares_and_ignores_what_it_cannot_hold() {
        let (mut a, mut b) = (HeldInput::default(), HeldInput::default());
        a.set_key(1, true);
        a.set_key(100, true);
        a.set_button(MOUSE_BTN_LEFT, true);
        b.set_key(100, true);
        b.set_button(MOUSE_BTN_LEFT, true);
        assert_eq!(a.without(&b).keys().collect::<Vec<_>>(), vec![1]);
        assert_eq!(a.without(&b).buttons().count(), 0);
        assert_eq!(a.and(&b), b);
        assert_eq!(a.without_buttons().buttons().count(), 0);
        a.set_key(100, false);
        assert_eq!(a.keys().collect::<Vec<_>>(), vec![1]);
        let before = a;
        a.set_key(300, true);
        a.set_button(0, true);
        a.set_button(9, true);
        assert_eq!(a, before);
    }

    #[test]
    fn a_device_without_link_state_fails_to_read_it_and_nothing_else() {
        // An older build has one variant fewer, which bincode refuses. This is the index it sees.
        let state = LinkState { protocol: LINK_PROTOCOL, beat: 1, echo: 0, echo_age_ms: 0, loss_tenths: 0, panicked: false, press_order: 0, main: false, sent_ms: 0, held: HeldInput::default() };
        let bytes = Packet::new(0, NodeId::Debian, Payload::LinkState(state)).encode().unwrap();
        assert_eq!(bytes[16..20], 14u32.to_le_bytes());
        assert!(bytes.len() < MAX_PACKET_SIZE);
    }

    fn display_info(count: usize) -> Payload {
        let monitor = |i: usize| MonitorRect {
            name: format!("Display {i} with a fairly long name"),
            x: -2560 * i as i32,
            y: -1440,
            w: 2560,
            h: 1440,
        };
        Payload::PeerDisplayInfo { name: "Laptop with a long name".into(), monitors: (0..count).map(monitor).collect() }
    }

    #[test]
    fn every_monitor_travels_with_its_name_and_negative_position() {
        match round_trip(display_info(3)) {
            Payload::PeerDisplayInfo { name, monitors } => {
                assert_eq!(name, "Laptop with a long name");
                assert_eq!(monitors.len(), 3);
                assert_eq!((monitors[2].x, monitors[2].y, monitors[2].w), (-5120, -1440, 2560));
                assert_eq!(monitors[2].name, "Display 2 with a fairly long name");
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn the_most_monitors_a_device_reports_fit_the_receive_buffer() {
        let bytes = Packet::new(1, NodeId::Windows, display_info(16)).encode().unwrap();
        assert!(bytes.len() <= MAX_PACKET_SIZE, "{}", bytes.len());
    }

    #[test]
    fn a_packet_claiming_more_monitors_than_it_holds_is_rejected() {
        let mut bytes = Packet::new(1, NodeId::Windows, display_info(1)).encode().unwrap();
        // Header (16), variant index (4), then the name's length and text, then the list's length.
        let list = 20 + 8 + "Laptop with a long name".len();
        bytes[list..list + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Packet::decode(&bytes).is_err());
    }

    #[test]
    fn existing_indexes_unchanged() {
        // Header: magic(4) + seq(8) + source NodeId(4) = 16 bytes, then the variant index.
        let index = |p: Payload| {
            let bytes = Packet::new(0, NodeId::Debian, p).encode().unwrap();
            bytes[16..20].to_vec()
        };
        assert_eq!(index(Payload::Heartbeat), 7u32.to_le_bytes());
        assert_eq!(index(Payload::PeerDisplayInfo { name: String::new(), monitors: vec![] }), 8u32.to_le_bytes());
        assert_eq!(index(Payload::GameLock { locked: true }), 11u32.to_le_bytes());
        assert_eq!(index(Payload::ActiveView { active_node: NodeId::Windows }), 12u32.to_le_bytes());
    }
}
