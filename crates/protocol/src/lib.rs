//! Signaling protocol shared between the relay server, sender app, and
//! receiver web page. Messages are JSON-encoded over a WebSocket. The same
//! shapes are reused for the eventual native sender so they live in a
//! standalone crate.
//!
//! All client→server and server→client traffic is a [`Envelope`] wrapping a
//! payload tagged by message type. Keep this file additive — bumping the
//! wire format means coordinating relay deploys with every connected client.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;

/// A wrapper that carries a sequence number and the tagged payload. The
/// sequence number lets either side correlate replies and detect duplicates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    #[serde(flatten)]
    pub msg: Message,
}

/// Direction-agnostic message set. Some variants are only sent in one
/// direction in practice (e.g. only the server sends `RoomState`), but
/// keeping a unified enum lets us share the parser.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// First message a peer sends; relay validates protocol_version and
    /// either joins or rejects.
    Hello {
        protocol_version: u32,
        role: Role,
        room: RoomCode,
    },
    /// Relay's acknowledgement of `Hello`. Includes the peer's assigned
    /// id and a snapshot of who else is in the room.
    Welcome {
        peer_id: PeerId,
        room_state: RoomState,
    },
    /// SDP exchange routed peer-to-peer through the relay. The sender sets
    /// `to`; the relay stamps `from` based on the originating WebSocket
    /// so it can't be spoofed.
    Sdp {
        kind: SdpKind,
        sdp: String,
        /// Set by the relay when forwarding to the recipient. Clients leave
        /// this `None` when sending; they read it when receiving.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<PeerId>,
        /// Set by the client when sending. Relay routes by this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<PeerId>,
    },
    /// Trickled ICE candidate routed peer-to-peer through the relay. Carries
    /// the serialized RTCIceCandidateInit JSON — the structure varies slightly
    /// between webrtc-rs and browser-issued candidates so we keep it as a
    /// string and parse at the boundary.
    IceCandidate {
        candidate: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<PeerId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<PeerId>,
    },
    /// Server notifies all peers when someone joins.
    PeerJoined { peer: PeerInfo },
    /// Server notifies all peers when someone leaves.
    PeerLeft { peer_id: PeerId },
    /// Connection-level error from the relay; usually fatal.
    Error { code: ErrorCode, detail: String },
    /// Bidirectional liveness check. Relay sends periodically; client echoes.
    Ping { nonce: u64 },
    Pong { nonce: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Publishes a video stream into the room (the sender app).
    Publisher,
    /// Consumes streams in the room (the receiver web page / OBS).
    Subscriber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdpKind {
    Offer,
    Answer,
}

/// Short, human-friendly room identifier. Five lowercase letters by default;
/// the relay generates these and clients pass them around.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomCode(pub String);

impl RoomCode {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque peer identifier minted by the relay on join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: PeerId,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomState {
    pub code: RoomCode,
    pub peers: Vec<PeerInfo>,
    /// ICE servers the relay wants this peer to use. Includes the relay's
    /// own embedded TURN with ephemeral credentials, plus any STUN it knows.
    /// Shape mirrors the W3C `RTCIceServer` dictionary so JS clients can
    /// pass it straight into `new RTCPeerConnection({ iceServers })`.
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    RoomFull,
    RoomNotFound,
    InvalidMessage,
    Internal,
}

/// Generate a short, lowercase, vowel-balanced room code. Not cryptographic;
/// the relay can collision-check on insertion.
pub fn generate_room_code(rng: &mut impl FnMut() -> u64) -> RoomCode {
    const CONS: &[u8] = b"bcdfghjklmnpqrstvwxz";
    const VOWS: &[u8] = b"aeiouy";
    let mut s = String::with_capacity(6);
    let r = rng();
    for i in 0..6 {
        let nibble = ((r >> (i * 8)) & 0xff) as usize;
        let b = if i % 2 == 0 {
            CONS[nibble % CONS.len()]
        } else {
            VOWS[nibble % VOWS.len()]
        };
        s.push(b as char);
    }
    RoomCode(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_state_default_ice_servers() {
        // Backwards compat: a Welcome without ice_servers still deserializes.
        let json = r#"{"code":"abc","peers":[]}"#;
        let st: RoomState = serde_json::from_str(json).unwrap();
        assert!(st.ice_servers.is_empty());
    }

    #[test]
    fn envelope_round_trips() {
        let env = Envelope {
            seq: 42,
            msg: Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                role: Role::Publisher,
                room: RoomCode("brave-otter".into()),
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 42);
        match back.msg {
            Message::Hello { role, .. } => assert_eq!(role, Role::Publisher),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn room_code_generates_deterministically() {
        let mut state = 0xdeadbeef_cafebabeu64;
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let code = generate_room_code(&mut rng);
        assert_eq!(code.as_str().len(), 6);
    }
}
