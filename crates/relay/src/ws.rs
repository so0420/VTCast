//! WebSocket signaling endpoint. The relay no longer touches media — it just
//! routes Sdp + IceCandidate messages between peers in the same room.
//! Publishers and subscribers negotiate WebRTC peer-to-peer; the relay
//! also runs an embedded TURN server for fallback when direct UDP fails.

use crate::{
    room::{Peer, Room},
    AppState,
};
use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::mpsc;
use vtcast_protocol::{
    Envelope, ErrorCode, Message, PeerId, Role, RoomState, PROTOCOL_VERSION,
};

#[derive(Serialize)]
pub struct NewRoomResponse {
    pub code: String,
}

pub async fn new_room(State(state): State<AppState>) -> impl IntoResponse {
    let room = state.rooms.create_room();
    let body = NewRoomResponse {
        code: room.code.as_str().to_string(),
    };
    tracing::info!(code = %body.code, "room created");
    (StatusCode::OK, Json(body))
}

pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender_ws, mut receiver_ws) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();

    let send_task = tokio::spawn(async move {
        // Keepalive: signaling goes silent once negotiation is done, and
        // idle WebSockets get killed by proxies in front of the relay —
        // Cloudflare drops them after ~100 s of no traffic, which tore down
        // every session's signaling every couple of minutes (the publisher
        // then rebuilt its track + PCs, freezing receivers for seconds each
        // time). A protocol-level Ping every 30 s keeps traffic flowing in
        // both directions (browsers and tungstenite auto-Pong) with zero
        // client-side changes.
        let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(30));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                env = rx.recv() => {
                    let Some(env) = env else { break };
                    let s = match serde_json::to_string(&env) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(error = ?e, "serialize outbound");
                            continue;
                        }
                    };
                    if sender_ws.send(WsMessage::Text(s.into())).await.is_err() {
                        break;
                    }
                }
                _ = keepalive.tick() => {
                    if sender_ws.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // === Hello phase ===
    let first = match receiver_ws.next().await {
        Some(Ok(WsMessage::Text(t))) => t,
        _ => return,
    };
    let env: Envelope = match serde_json::from_str(&first) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = ?e, "hello not valid envelope");
            return;
        }
    };
    let (role, room_code) = match env.msg {
        Message::Hello {
            protocol_version,
            role,
            room,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                let _ = tx.send(error_envelope(
                    ErrorCode::VersionMismatch,
                    format!(
                        "relay speaks v{}, client sent v{}",
                        PROTOCOL_VERSION, protocol_version
                    ),
                ));
                return;
            }
            (role, room)
        }
        other => {
            tracing::debug!(?other, "first message was not Hello");
            return;
        }
    };

    // Publishers may reconnect after a transient WS drop (network blip, VTS
    // asset swap freezing the source momentarily, etc.). If the room has
    // already been GC'd because the publisher was the only peer, resurrect
    // it with the same code so the in-flight session survives. Subscribers
    // still get a clean RoomNotFound for genuinely bad codes.
    let room = match state.rooms.get(&room_code) {
        Some(r) => r,
        None => match role {
            Role::Publisher => state.rooms.get_or_resurrect(&room_code),
            _ => {
                let _ = tx.send(error_envelope(
                    ErrorCode::RoomNotFound,
                    format!("no room with code {}", room_code.as_str()),
                ));
                return;
            }
        },
    };

    let peer_id = state.rooms.mint_peer_id();

    // Welcome + register. ice_servers includes Google's public STUN (cheap,
    // standard) plus the relay's embedded TURN with ephemeral credentials
    // so peers fall back to relayed media if symmetric NAT defeats direct
    // UDP hole-punching.
    let _ = tx.send(Envelope {
        seq: 0,
        msg: Message::Welcome {
            peer_id,
            room_state: RoomState {
                code: room.code.clone(),
                peers: room.snapshot(),
                ice_servers: state
                    .turn
                    .as_ref()
                    .map(|t| t.ice_servers_for_peer())
                    .unwrap_or_else(crate::turn::stun_only_ice_servers),
            },
        },
    });

    {
        let peer = Peer {
            id: peer_id,
            role,
            tx: tx.clone(),
        };
        let info = peer.info();
        room.peers.write().insert(peer_id, peer);
        room.broadcast_except(
            peer_id,
            &Envelope {
                seq: 0,
                msg: Message::PeerJoined { peer: info },
            },
        );
    }
    tracing::info!(?peer_id, ?role, code = %room.code.as_str(), "peer joined");

    // === Inbound loop: route Sdp + IceCandidate to the target peer ===
    while let Some(frame) = receiver_ws.next().await {
        let text = match frame {
            Ok(WsMessage::Text(t)) => t,
            Ok(WsMessage::Close(_)) => break,
            Ok(_) => continue,
            Err(_) => break,
        };
        let env: Envelope = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = ?e, "drop malformed envelope");
                continue;
            }
        };

        match env.msg {
            Message::Sdp { kind, sdp, to, .. } => {
                let Some(target_id) = to else {
                    tracing::debug!(?peer_id, "Sdp without `to` field dropped");
                    continue;
                };
                forward_to(
                    &room,
                    peer_id,
                    target_id,
                    env.seq,
                    Message::Sdp {
                        kind,
                        sdp,
                        from: Some(peer_id),
                        to: Some(target_id),
                    },
                );
            }
            Message::IceCandidate { candidate, to, .. } => {
                let Some(target_id) = to else {
                    tracing::debug!(?peer_id, "IceCandidate without `to` field dropped");
                    continue;
                };
                forward_to(
                    &room,
                    peer_id,
                    target_id,
                    env.seq,
                    Message::IceCandidate {
                        candidate,
                        from: Some(peer_id),
                        to: Some(target_id),
                    },
                );
            }
            Message::Pong { .. } => {}
            Message::Hello { .. } => {
                tracing::debug!("duplicate Hello dropped");
            }
            _ => {
                tracing::debug!("unsupported inbound message");
            }
        }
    }

    // Cleanup
    room.peers.write().remove(&peer_id);
    room.broadcast_except(
        peer_id,
        &Envelope {
            seq: 0,
            msg: Message::PeerLeft { peer_id },
        },
    );
    tracing::info!(?peer_id, code = %room.code.as_str(), "peer left");
    state.rooms.gc_if_empty(&room.code);
    drop(tx);
    let _ = send_task.await;
}

fn forward_to(room: &Arc<Room>, from: PeerId, to: PeerId, seq: u64, msg: Message) {
    let Some(target_tx) = room.peer_tx(to) else {
        tracing::debug!(?from, ?to, "forward target not in room");
        return;
    };
    let _ = target_tx.send(Envelope { seq, msg });
}

fn error_envelope(code: ErrorCode, detail: String) -> Envelope {
    Envelope {
        seq: 0,
        msg: Message::Error { code, detail },
    }
}
