use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub const DEFAULT_INPUT_PORT: u16 = 24850;
pub const DEFAULT_AUDIO_PORT: u16 = 24851;
pub const DEFAULT_CLIPBOARD_PORT: u16 = 24852;
pub const PACKET_MAGIC: [u8; 4] = *b"MKVM";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum NodeId {
    Debian = 0,
    Windows = 1,
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
    PeerDisplayInfo { width: i32, height: i32 },
    /// Debian to laptop with each heartbeat: which laptop edge touches Debian, and where.
    EdgeMap { edge: SwitchEdge, start: i32, end: i32, resistance: i32 },
    /// Replaces `FocusSwitch` for all new sends; `x`, `y` are laptop-local.
    FocusSwitchAt { target_node: NodeId, x: i32, y: i32 },
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
    }

    #[test]
    fn existing_indexes_unchanged() {
        // Header: magic(4) + seq(8) + source NodeId(4) = 16 bytes, then the variant index.
        let index = |p: Payload| {
            let bytes = Packet::new(0, NodeId::Debian, p).encode().unwrap();
            bytes[16..20].to_vec()
        };
        assert_eq!(index(Payload::Heartbeat), 7u32.to_le_bytes());
        assert_eq!(index(Payload::PeerDisplayInfo { width: 1, height: 1 }), 8u32.to_le_bytes());
    }
}
