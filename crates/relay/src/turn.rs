//! Embedded TURN server.
//!
//! Lets WebRTC peers fall back to relayed media when direct UDP can't punch
//! through NATs / firewalls. We keep the TURN server in the same process as
//! the SFU so a self-hosting user only opens one binary (and one UDP port).
//!
//! Auth: time-windowed credentials per RFC 5389 / coturn's `static-auth-secret`
//! convention. The relay generates one credential pair per joining peer and
//! ships them to the client inside the [`vtcast_protocol::RoomState`].

use anyhow::{anyhow, Context, Result};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use vtcast_protocol::IceServer;
use webrtc::turn::auth::{generate_long_term_credentials, LongTermAuthHandler};
use webrtc::turn::relay::relay_static::RelayAddressGeneratorStatic;
use webrtc::turn::server::config::{ConnConfig, ServerConfig};
use webrtc::turn::server::Server;
use webrtc::util::vnet::net::Net;

pub const REALM: &str = "vtcast";
const CREDENTIAL_LIFETIME: Duration = Duration::from_secs(600);

pub struct TurnService {
    /// UDP port the server listens on (also where clients send their
    /// allocations).
    pub port: u16,
    /// Address the client sees in its TURN URL. For LAN/dev this is just
    /// the relay host's address; for production behind a router it's the
    /// public IP.
    pub advertised_addr: String,
    shared_secret: String,
    realm: String,
    _server: Server,
}

impl TurnService {
    pub async fn start(
        listen_ip: IpAddr,
        public_ip: IpAddr,
        port: u16,
        advertised_addr: String,
        shared_secret: String,
    ) -> Result<Self> {
        let conn: Arc<dyn webrtc::util::Conn + Send + Sync> = Arc::new(
            UdpSocket::bind((listen_ip, port))
                .await
                .with_context(|| format!("bind TURN UDP {}:{}", listen_ip, port))?,
        );

        let auth_handler = Arc::new(LongTermAuthHandler::new(shared_secret.clone()));

        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                    relay_address: public_ip,
                    address: "0.0.0.0".to_string(),
                    net: Arc::new(Net::new(None)),
                }),
            }],
            realm: REALM.to_string(),
            auth_handler,
            channel_bind_timeout: Duration::from_secs(0),
            alloc_close_notify: None,
        })
        .await
        .map_err(|e| anyhow!("turn::Server::new: {e}"))?;

        Ok(Self {
            port,
            advertised_addr,
            shared_secret,
            realm: REALM.to_string(),
            _server: server,
        })
    }

    /// Build the ICE-server list a client should use: STUN + our TURN with a
    /// freshly-minted credential pair valid for ~10 minutes.
    pub fn ice_servers_for_peer(&self) -> Vec<IceServer> {
        let (username, credential) =
            match generate_long_term_credentials(&self.shared_secret, CREDENTIAL_LIFETIME) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = ?e, "generate_long_term_credentials");
                    return vec![default_stun()];
                }
            };
        vec![
            default_stun(),
            IceServer {
                urls: vec![format!("turn:{}:{}?transport=udp", self.advertised_addr, self.port)],
                username: Some(username),
                credential: Some(credential),
            },
        ]
    }

    pub fn realm(&self) -> &str {
        &self.realm
    }
}

fn default_stun() -> IceServer {
    IceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_string()],
        username: None,
        credential: None,
    }
}
