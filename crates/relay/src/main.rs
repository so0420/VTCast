//! vtcast-relay: peer-to-peer signaling broker + TURN fallback in one binary.
//!
//! The relay used to also run a WebRTC SFU that forwarded media; now peers
//! negotiate directly via WebRTC and the relay only routes Sdp/ICE messages
//! and provides ICE servers (Google STUN + embedded TURN). Media never
//! touches the relay's bandwidth budget except when a peer is behind a
//! symmetric NAT and falls back to TURN.

mod room;
mod turn;
mod ws;

use anyhow::Result;
use axum::{
    routing::{any, get},
    Router,
};
use rand::RngCore;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<room::RoomRegistry>,
    /// `None` when TURN is disabled (`VTCAST_TURN_ENABLED=false`) — peers get
    /// STUN-only ICE servers and no media can ever relay through this host.
    pub turn: Option<Arc<turn::TurnService>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env (CWD upward). Silent if absent — env vars set in the shell
    // or systemd unit still win.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,vtcast_relay=debug")),
        )
        .init();

    // TURN on/off switch. Default ON (TURN is the fallback for peers whose
    // NATs defeat direct P2P). Turning it OFF guarantees no media ever relays
    // through this host's bandwidth — those peers just won't connect.
    let turn_enabled = std::env::var("VTCAST_TURN_ENABLED")
        .map(|v| !matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(true);

    // TURN config from env (or sensible local-dev defaults)
    let turn_port: u16 = std::env::var("VTCAST_TURN_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3478);
    // Accepts an IP literal or a hostname (resolved once at startup). A value
    // that neither parses nor resolves is a hard error — it used to fall back
    // silently to 127.0.0.1, which advertised an unusable TURN server to every
    // remote peer and was miserable to notice.
    let turn_public_ip: IpAddr = match std::env::var("VTCAST_TURN_PUBLIC_IP") {
        Ok(v) => resolve_ip(&v).ok_or_else(|| {
            anyhow::anyhow!(
                "VTCAST_TURN_PUBLIC_IP='{v}' is neither an IP literal nor a \
                 resolvable hostname"
            )
        })?,
        Err(_) => IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    };
    let turn_advertised = std::env::var("VTCAST_TURN_ADVERTISED")
        .unwrap_or_else(|_| turn_public_ip.to_string());
    // A loopback/unspecified advertised address means every remote peer gets
    // a TURN URL pointing at *its own machine* — TURN fallback silently dead.
    // This is exactly what happens when the env vars are forgotten on a
    // public deployment, so shout about it at startup.
    let advertised_unreachable = turn_advertised
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback() || ip.is_unspecified())
        .unwrap_or(false)
        || turn_advertised == "localhost";
    if turn_enabled && advertised_unreachable {
        tracing::warn!(
            advertised = %turn_advertised,
            "TURN is advertising a loopback address — remote peers CANNOT use \
             TURN fallback. Set VTCAST_TURN_PUBLIC_IP (and optionally \
             VTCAST_TURN_ADVERTISED) to this server's public IP, and make sure \
             UDP port {} is reachable (Cloudflare does not proxy UDP).",
            turn_port
        );
    }
    let turn_secret = std::env::var("VTCAST_TURN_SECRET").unwrap_or_else(|_| {
        // Random per-process secret. Restarting the relay invalidates
        // outstanding credentials, which is fine — clients are expected
        // to re-join after the relay restarts anyway.
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        hex_encode(&bytes)
    });
    let turn = if turn_enabled {
        let t = Arc::new(
            turn::TurnService::start(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                turn_public_ip,
                turn_port,
                turn_advertised.clone(),
                turn_secret,
            )
            .await?,
        );
        tracing::info!(port = turn_port, advertised = %turn_advertised, "TURN listening (UDP)");
        Some(t)
    } else {
        tracing::info!(
            "TURN disabled (VTCAST_TURN_ENABLED=false) — peers get STUN-only \
             ICE; no media will relay through this host"
        );
        None
    };

    let state = AppState {
        rooms: Arc::new(room::RoomRegistry::new()),
        turn,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/test_rtc.html", get(test_rtc))
        .route("/r", get(receiver))
        .route("/api/new-room", get(ws::new_room))
        .route("/ws", any(ws::websocket_handler))
        .with_state(state);

    let addr: SocketAddr = std::env::var("VTCAST_RELAY_BIND")
        .unwrap_or_else(|_| "0.0.0.0:17239".to_string())
        .parse()?;

    tracing::info!(%addr, "vtcast-relay listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn test_rtc() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../assets/test_rtc.html"))
}

async fn receiver() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../assets/receiver.html"))
}

/// Parse an IP literal, or resolve a hostname to its first address (v4
/// preferred — TURN allocations here are UDP/IPv4 in practice).
fn resolve_ip(host: &str) -> Option<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }
    use std::net::ToSocketAddrs;
    let addrs: Vec<IpAddr> = (host, 0)
        .to_socket_addrs()
        .ok()?
        .map(|sa| sa.ip())
        .collect();
    addrs
        .iter()
        .find(|ip| ip.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}
