use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub const DEFAULT_INPUT_PORT: u16 = 24850;
pub const DEFAULT_AUDIO_PORT: u16 = 24851;
pub const DEFAULT_CLIPBOARD_PORT: u16 = 24852;
pub const PACKET_MAGIC: [u8; 4] = *b"MKVM";
/// The most a packet can be: 16 monitors with long names, under one Ethernet frame.
pub const MAX_PACKET_SIZE: usize = 1400;

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
