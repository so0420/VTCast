//! Room registry — in-memory, no persistence. Rooms hold connected peers'
//! WebSocket outbound channels keyed by PeerId. The relay only routes
//! signaling messages; media flows peer-to-peer via WebRTC.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use vtcast_protocol::{Envelope, PeerId, PeerInfo, RoomCode, Role};

pub struct Peer {
    pub id: PeerId,
    pub role: Role,
    pub tx: mpsc::UnboundedSender<Envelope>,
}

impl Peer {
    pub fn info(&self) -> PeerInfo {
        PeerInfo {
            peer_id: self.id,
            role: self.role,
        }
    }
}

pub struct Room {
    pub code: RoomCode,
    pub peers: RwLock<HashMap<PeerId, Peer>>,
}

impl Room {
    pub fn new(code: RoomCode) -> Self {
        Self {
            code,
            peers: RwLock::new(HashMap::new()),
        }
    }

    pub fn broadcast_except(&self, except: PeerId, env: &Envelope) {
        let peers = self.peers.read();
        for (id, peer) in peers.iter() {
            if *id == except {
                continue;
            }
            let _ = peer.tx.send(env.clone());
        }
    }

    pub fn snapshot(&self) -> Vec<PeerInfo> {
        self.peers.read().values().map(|p| p.info()).collect()
    }

    /// Look up a peer's outbound channel by id, for unicast routing of Sdp /
    /// IceCandidate messages. Returns None if the peer isn't in this room
    /// (raced disconnect, spoofed `to`, etc.).
    pub fn peer_tx(&self, id: PeerId) -> Option<mpsc::UnboundedSender<Envelope>> {
        self.peers.read().get(&id).map(|p| p.tx.clone())
    }
}

pub struct RoomRegistry {
    rooms: RwLock<HashMap<RoomCode, Arc<Room>>>,
    next_peer_id: AtomicU64,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            next_peer_id: AtomicU64::new(1),
        }
    }

    pub fn mint_peer_id(&self) -> PeerId {
        PeerId(self.next_peer_id.fetch_add(1, Ordering::Relaxed))
    }

    pub fn create_room(&self) -> Arc<Room> {
        loop {
            let code = vtcast_protocol::generate_room_code(&mut || rand::random::<u64>());
            let mut map = self.rooms.write();
            if !map.contains_key(&code) {
                let room = Arc::new(Room::new(code.clone()));
                map.insert(code, Arc::clone(&room));
                return room;
            }
        }
    }

    pub fn get(&self, code: &RoomCode) -> Option<Arc<Room>> {
        self.rooms.read().get(code).cloned()
    }

    /// Look up a room by code, creating it with that exact code if absent.
    /// Used when a Publisher reconnects after a transient WS drop: the room
    /// may have been GC'd between the disconnect and the retry, but the
    /// Publisher still owns the code it minted via /api/new-room.
    pub fn get_or_resurrect(&self, code: &RoomCode) -> Arc<Room> {
        if let Some(room) = self.rooms.read().get(code).cloned() {
            return room;
        }
        let mut map = self.rooms.write();
        if let Some(room) = map.get(code).cloned() {
            return room;
        }
        let room = Arc::new(Room::new(code.clone()));
        map.insert(code.clone(), Arc::clone(&room));
        tracing::info!(code = %code.as_str(), "room resurrected by publisher");
        room
    }

    pub fn gc_if_empty(&self, code: &RoomCode) {
        let mut map = self.rooms.write();
        if let Some(room) = map.get(code) {
            if room.peers.read().is_empty() {
                map.remove(code);
                tracing::debug!(code = %code.as_str(), "room garbage collected");
            }
        }
    }
}
