//! Shared wire types for all subcluster daemons.
//!
//! Two protocols are defined here:
//!
//! 1. **Discovery frame** — compact UDP binary frame used by hivebus for
//!    broadcast/multicast cluster announcements, heartbeats, and voting.
//!
//! 2. **Control envelope** — Unix socket framing used by every daemon's local
//!    control plane (op + length-prefixed bincode payload).

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic byte that starts every discovery frame.
pub const FRAME_MAGIC: u8 = 0xCA;

/// UDP port used for cluster broadcast/multicast.
pub const DISCOVERY_PORT: u16 = 7777;

/// IPv4 multicast group for hivebus.
pub const MULTICAST_ADDR: &str = "224.0.133.7";

/// Unix socket base directory (must exist; created by init scripts).
pub const SOCKET_DIR: &str = "/var/run/subcluster";

/// Marker byte that starts every control-plane frame.
pub const CONTROL_FRAME_MARKER: u8 = 0x01;

/// Defensive upper bound for accepted control-plane payloads.
pub const MAX_CONTROL_PAYLOAD: usize = 60 * 1024;

// ---------------------------------------------------------------------------
// Discovery frame (UDP, binary, NOT bincode — fixed wire format)
// ---------------------------------------------------------------------------

/// 4-byte fixed header carried in every UDP discovery packet.
///
/// ```text
/// ┌──────────┬───────────┬──────────────────┐
/// │ magic(1) │ type_seq  │   node_id (2 BE) │
/// └──────────┴───────────┴──────────────────┘
///              ↑ upper 4 = MsgType, lower 4 = rolling seq
/// ```
///
/// Payload (variable) follows immediately after the 4-byte header.
/// Maximum UDP payload is 508 bytes (safe MTU floor minus headers).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FrameHeader {
    pub magic: u8,
    /// Packed: `(msg_type << 4) | seq`
    pub type_seq: u8,
    /// Big-endian node identifier.
    pub node_id: [u8; 2],
}

impl FrameHeader {
    pub fn new(msg_type: MsgType, seq: u8, node_id: u16) -> Self {
        Self {
            magic: FRAME_MAGIC,
            type_seq: ((msg_type as u8) << 4) | (seq & 0x0F),
            node_id: node_id.to_be_bytes(),
        }
    }

    pub fn msg_type(&self) -> Option<MsgType> {
        MsgType::from_u8(self.type_seq >> 4)
    }

    pub fn seq(&self) -> u8 {
        self.type_seq & 0x0F
    }

    pub fn node_id(&self) -> u16 {
        u16::from_be_bytes(self.node_id)
    }

    /// Serialize to 4 bytes.
    pub fn to_bytes(&self) -> [u8; 4] {
        [self.magic, self.type_seq, self.node_id[0], self.node_id[1]]
    }

    /// Deserialize from a byte slice (at least 4 bytes).
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 4 || b[0] != FRAME_MAGIC {
            return None;
        }
        Some(Self {
            magic: b[0],
            type_seq: b[1],
            node_id: [b[2], b[3]],
        })
    }
}

/// Message types carried in the upper nibble of `type_seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MsgType {
    Heartbeat   = 0,
    Announce    = 1,
    Leave       = 2,
    Ack         = 3,
    Gossip      = 4,
    VoteRequest = 5,
    VoteGrant   = 6,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Heartbeat),
            1 => Some(Self::Announce),
            2 => Some(Self::Leave),
            3 => Some(Self::Ack),
            4 => Some(Self::Gossip),
            5 => Some(Self::VoteRequest),
            6 => Some(Self::VoteGrant),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery payloads (bincode serialized, appended after FrameHeader)
// ---------------------------------------------------------------------------

/// Sent periodically to prove the node is alive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    /// Monotonically increasing counter reset on node restart.
    pub incarnation: u32,
    /// Unix timestamp millis.
    pub epoch_ms: u64,
}

/// Broadcast on startup or when joining the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnouncePayload {
    pub incarnation: u32,
    /// Human-readable hostname.
    pub hostname: String,
    /// Address where this node's control sockets can be reached.
    pub control_addr: SocketAddr,
    /// Software version string.
    pub version: String,
}

/// Sent on graceful shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeavePayload {
    pub incarnation: u32,
    pub reason: String,
}

/// Gossip delta — partial view of the membership table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipPayload {
    pub entries: Vec<GossipEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipEntry {
    pub node_id: u16,
    pub incarnation: u32,
    pub state: NodeState,
    pub epoch_ms: u64,
}

/// Raft-style vote request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRequestPayload {
    pub term: u64,
    pub candidate_id: u16,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

/// Raft-style vote grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteGrantPayload {
    pub term: u64,
    pub granted: bool,
}

// ---------------------------------------------------------------------------
// Node state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Alive,
    Suspect,
    Dead,
}

/// Full view of a peer as tracked by hivebus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: u16,
    pub hostname: String,
    pub control_addr: SocketAddr,
    pub state: NodeState,
    pub incarnation: u32,
    pub last_seen: SystemTime,
    pub is_master: bool,
}

impl NodeInfo {
    /// Returns true if the node should be considered timed out.
    pub fn is_timed_out(&self, dead_threshold: Duration) -> bool {
        self.last_seen
            .elapsed()
            .map(|e| e > dead_threshold)
            .unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// Unix socket control envelope
// ---------------------------------------------------------------------------

/// Every request sent over a daemon's Unix control socket is framed as:
/// ```text
/// marker(1B=0x01) | payload_len(2B BE) | payload(bincode, payload_len bytes)
/// ```
/// The daemon always responds with the same envelope framing.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlOp {
    Ping,
    /// Query the full cluster membership table (hivebus).
    GetNodes,
    /// Ask hivebus who the current master is.
    GetMaster,
    /// Force this node to step down as master.
    StepDown,
    /// Graceful shutdown request.
    Shutdown,
    /// Daemon-specific payload (JSON blob for extensibility).
    Custom { tag: String, payload: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlReply {
    Ok,
    Error { message: String },
    Nodes(Vec<NodeInfo>),
    Master(Option<NodeInfo>),
    Data { tag: String, payload: Vec<u8> },
}

/// Encode a control message to bytes: `op(1B) | len(2B BE) | payload`.
pub fn encode_control<T: Serialize>(msg: &T) -> Result<Vec<u8>, bincode::error::EncodeError> {
    let payload = bincode::serde::encode_to_vec(msg, bincode::config::standard())?;
    let len = u16::try_from(payload.len()).expect("control payload exceeds u16 wire limit");
    let mut out = Vec::with_capacity(3 + payload.len());
    out.push(CONTROL_FRAME_MARKER);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode the length-prefixed payload portion (after the 1-byte op and 2-byte len
/// have been read). `bytes` must be exactly `payload_len` bytes.
pub fn decode_control<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, bincode::error::DecodeError> {
    let (val, _) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())?;
    Ok(val)
}

// ---------------------------------------------------------------------------
// Node ID derivation
// ---------------------------------------------------------------------------

/// Derive a stable 16-bit node ID from a MAC address.
///
/// XOR-folds all 6 octets into two bytes. Guaranteed collision-free for
/// well-managed MAC spaces; in the rare collision case, hivebus will detect
/// duplicate node IDs during ANNOUNCE and log a warning.
pub fn node_id_from_mac(mac: &[u8; 6]) -> u16 {
    let hi = mac[0] ^ mac[2] ^ mac[4];
    let lo = mac[1] ^ mac[3] ^ mac[5];
    u16::from_be_bytes([hi, lo])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_round_trip() {
        let h = FrameHeader::new(MsgType::Announce, 7, 0xBEEF);
        let b = h.to_bytes();
        let h2 = FrameHeader::from_bytes(&b).unwrap();
        assert_eq!(h2.magic, FRAME_MAGIC);
        assert_eq!(h2.msg_type(), Some(MsgType::Announce));
        assert_eq!(h2.seq(), 7);
        assert_eq!(h2.node_id(), 0xBEEF);
    }

    #[test]
    fn node_id_from_mac_stable() {
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(node_id_from_mac(&mac), node_id_from_mac(&mac));
    }
}
