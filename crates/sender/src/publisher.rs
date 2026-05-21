//! WebRTC publisher (mesh mode). Connects to the relay's WebSocket signaling,
//! then creates a direct peer-to-peer PeerConnection per subscriber. The
//! `Arc<TrackLocalStaticSample>` is shared across every PC so the encoder
//! pipeline runs once regardless of subscriber count — webrtc-rs fans the
//! samples out into per-PC RTP streams internally.
//!
//! Negotiation: the relay just routes messages; the publisher initiates an
//! Offer per subscriber on join (both for subscribers already in the room
//! at Welcome time and for those arriving via PeerJoined). Sdp answers and
//! IceCandidates are routed back by the relay tagging `from = subscriber_id`,
//! which selects the right PC in our `pcs` map.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Mutex as PMutex;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message as WsMessage,
};
use vtcast_protocol::{
    Envelope, IceServer, Message, PeerId, Role, RoomCode, SdpKind, PROTOCOL_VERSION,
};

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::{APIBuilder, API};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

pub struct Publisher {
    /// Shared H.264 sample track. Caller writes encoded access units here;
    /// webrtc-rs forwards each sample into every active subscriber PC.
    pub track: Arc<TrackLocalStaticSample>,
}

/// The pump's terminal reason. Whichever happens first ends the pump.
#[derive(Debug)]
#[allow(dead_code)]
pub enum PumpEnded {
    /// The relay closed the WebSocket from its side, or read errored.
    WsClosed,
    /// The relay sent an Error envelope (RoomNotFound, version mismatch, …).
    RelayError(String),
    /// Local outbound channel closed (nothing left to send).
    OutboundClosed,
}

struct State {
    api: API,
    track: Arc<TrackLocalStaticSample>,
    /// ICE servers as advertised by the relay in Welcome (STUN + ephemeral
    /// TURN). Captured once and reused as the RTCConfiguration for every
    /// per-subscriber PC we create.
    ice_servers: PMutex<Vec<IceServer>>,
    /// Active PeerConnections keyed by the subscriber's relay-assigned id.
    pcs: PMutex<HashMap<PeerId, Arc<RTCPeerConnection>>>,
    out_tx: mpsc::UnboundedSender<Envelope>,
}

impl Publisher {
    pub async fn connect(
        relay_url: &str,
        room: &str,
    ) -> Result<(Self, tokio::task::JoinHandle<PumpEnded>)> {
        let ws_url = relay_url
            .replace("http://", "ws://")
            .replace("https://", "wss://")
            .trim_end_matches('/')
            .to_string()
            + "/ws";

        tracing::info!(%ws_url, %room, "connecting to relay");
        let (ws_stream, _resp) = connect_async(&ws_url)
            .await
            .with_context(|| format!("connect_async {ws_url}"))?;
        let (mut ws_out, mut ws_in) = ws_stream.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Envelope>();

        // Build one API + media engine + interceptor registry. The same API
        // mints every per-subscriber PC — webrtc-rs's API::new_peer_connection
        // takes &self, so sharing across many PCs is fine.
        let mut media = MediaEngine::default();
        media
            .register_default_codecs()
            .map_err(|e| anyhow!("register_default_codecs: {e}"))?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media)
            .map_err(|e| anyhow!("register_default_interceptors: {e}"))?;
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "vtcast".to_owned(),
        ));

        let state = Arc::new(State {
            api,
            track: Arc::clone(&track),
            ice_servers: PMutex::new(Vec::new()),
            pcs: PMutex::new(HashMap::new()),
            out_tx: out_tx.clone(),
        });

        send_envelope(
            &out_tx,
            Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                role: Role::Publisher,
                room: RoomCode(room.to_string()),
            },
        );

        let state_in = Arc::clone(&state);
        let inbound = tokio::spawn(async move {
            while let Some(frame) = ws_in.next().await {
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = ?e, "ws inbound");
                        return PumpEnded::WsClosed;
                    }
                };
                let text = match frame {
                    WsMessage::Text(t) => t,
                    WsMessage::Close(_) => return PumpEnded::WsClosed,
                    _ => continue,
                };
                let env: Envelope = match serde_json::from_str(&text) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::debug!(error = ?e, "bad envelope");
                        continue;
                    }
                };
                if let Message::Error { code, detail } = &env.msg {
                    tracing::error!(?code, %detail, "relay error, shutting down");
                    return PumpEnded::RelayError(detail.clone());
                }
                if let Err(e) = handle_inbound(&state_in, env).await {
                    tracing::warn!(error = ?e, "inbound handler");
                }
            }
            PumpEnded::WsClosed
        });

        let outbound = tokio::spawn(async move {
            while let Some(env) = out_rx.recv().await {
                let s = match serde_json::to_string(&env) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = ?e, "serialize outbound");
                        continue;
                    }
                };
                if ws_out.send(WsMessage::Text(s.into())).await.is_err() {
                    return PumpEnded::WsClosed;
                }
            }
            PumpEnded::OutboundClosed
        });

        // First-to-complete races inbound vs outbound. Whichever exits, the
        // pump aborts the other and returns the terminal reason.
        let pump = tokio::spawn(async move {
            use futures_util::future::{select, Either};
            match select(inbound, outbound).await {
                Either::Left((r, h)) => {
                    h.abort();
                    r.unwrap_or(PumpEnded::WsClosed)
                }
                Either::Right((r, h)) => {
                    h.abort();
                    r.unwrap_or(PumpEnded::WsClosed)
                }
            }
        });

        Ok((Self { track }, pump))
    }
}

fn send_envelope(out_tx: &mpsc::UnboundedSender<Envelope>, msg: Message) {
    let _ = out_tx.send(Envelope { seq: 0, msg });
}

fn to_rtc_ice_server(s: &IceServer) -> RTCIceServer {
    RTCIceServer {
        urls: s.urls.clone(),
        username: s.username.clone().unwrap_or_default(),
        credential: s.credential.clone().unwrap_or_default(),
        ..Default::default()
    }
}

async fn handle_inbound(state: &Arc<State>, env: Envelope) -> Result<()> {
    match env.msg {
        Message::Welcome { peer_id, room_state } => {
            tracing::info!(?peer_id, peers = ?room_state.peers, "welcome");
            *state.ice_servers.lock().unwrap() = room_state.ice_servers;
            // Offer to subscribers already in the room.
            for peer in room_state.peers {
                if peer.role == Role::Subscriber {
                    if let Err(e) = connect_to_subscriber(state, peer.peer_id).await {
                        tracing::warn!(?peer.peer_id, error = ?e, "initial subscriber connect failed");
                    }
                }
            }
        }
        Message::PeerJoined { peer } => {
            if peer.role == Role::Subscriber {
                if let Err(e) = connect_to_subscriber(state, peer.peer_id).await {
                    tracing::warn!(?peer.peer_id, error = ?e, "subscriber connect failed");
                }
            }
        }
        Message::PeerLeft { peer_id } => {
            let pc = state.pcs.lock().unwrap().remove(&peer_id);
            if let Some(pc) = pc {
                let _ = pc.close().await;
                tracing::info!(?peer_id, "subscriber left, pc closed");
            }
        }
        Message::Sdp { kind, sdp, from, .. } => {
            let Some(from) = from else {
                tracing::debug!("Sdp without `from` dropped");
                return Ok(());
            };
            if kind == SdpKind::Answer {
                let pc = state.pcs.lock().unwrap().get(&from).cloned();
                let Some(pc) = pc else {
                    tracing::debug!(?from, "answer for unknown subscriber");
                    return Ok(());
                };
                let desc = RTCSessionDescription::answer(sdp)
                    .map_err(|e| anyhow!("parse answer: {e}"))?;
                pc.set_remote_description(desc)
                    .await
                    .map_err(|e| anyhow!("set_remote_description: {e}"))?;
                tracing::debug!(?from, "answer applied");
            }
        }
        Message::IceCandidate { candidate, from, .. } => {
            let Some(from) = from else {
                tracing::debug!("IceCandidate without `from` dropped");
                return Ok(());
            };
            let pc = state.pcs.lock().unwrap().get(&from).cloned();
            let Some(pc) = pc else {
                tracing::debug!(?from, "ice for unknown subscriber");
                return Ok(());
            };
            let init: RTCIceCandidateInit = serde_json::from_str(&candidate)
                .map_err(|e| anyhow!("parse ice candidate: {e}"))?;
            pc.add_ice_candidate(init)
                .await
                .map_err(|e| anyhow!("add_ice_candidate: {e}"))?;
        }
        Message::Error { code, detail } => {
            tracing::error!(?code, %detail, "relay returned error");
            return Err(anyhow!("relay error: {detail}"));
        }
        Message::Ping { nonce } => {
            send_envelope(&state.out_tx, Message::Pong { nonce });
        }
        _ => {}
    }
    Ok(())
}

async fn connect_to_subscriber(state: &Arc<State>, sub_id: PeerId) -> Result<()> {
    let ice_servers: Vec<RTCIceServer> = state
        .ice_servers
        .lock().unwrap()
        .iter()
        .map(to_rtc_ice_server)
        .collect();
    let cfg = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };
    let pc = Arc::new(
        state
            .api
            .new_peer_connection(cfg)
            .await
            .map_err(|e| anyhow!("new_peer_connection: {e}"))?,
    );

    let track_local: Arc<dyn TrackLocal + Send + Sync> = state.track.clone();
    pc.add_track(track_local)
        .await
        .map_err(|e| anyhow!("add_track: {e}"))?;

    let out_tx = state.out_tx.clone();
    pc.on_ice_candidate(Box::new(move |c| {
        let out_tx = out_tx.clone();
        Box::pin(async move {
            if let Some(c) = c {
                if let Ok(init) = c.to_json() {
                    if let Ok(s) = serde_json::to_string(&init) {
                        let _ = out_tx.send(Envelope {
                            seq: 0,
                            msg: Message::IceCandidate {
                                candidate: s,
                                from: None,
                                to: Some(sub_id),
                            },
                        });
                    }
                }
            }
        })
    }));

    pc.on_peer_connection_state_change(Box::new(move |st: RTCPeerConnectionState| {
        Box::pin(async move {
            tracing::info!(?sub_id, ?st, "pc state");
        })
    }));

    let offer = pc
        .create_offer(None)
        .await
        .map_err(|e| anyhow!("create_offer: {e}"))?;
    pc.set_local_description(offer)
        .await
        .map_err(|e| anyhow!("set_local_description: {e}"))?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("local_description None"))?;

    state.pcs.lock().unwrap().insert(sub_id, Arc::clone(&pc));

    send_envelope(
        &state.out_tx,
        Message::Sdp {
            kind: SdpKind::Offer,
            sdp: local.sdp,
            from: None,
            to: Some(sub_id),
        },
    );

    tracing::info!(?sub_id, "subscriber PC created, offer sent");
    Ok(())
}
