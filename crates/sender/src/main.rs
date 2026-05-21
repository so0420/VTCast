//! vtcast-cli — a thin driver over the [`vtcast_sender`] library. All
//! pipeline logic lives in lib.rs so the Tauri desktop app sees the same
//! behaviour.

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use vtcast_sender::{ChromaKey, Config, EncoderBackend, EncoderKind, Pipeline, PipelineEvent, SourceKind};

#[derive(Parser, Debug)]
#[command(name = "vtcast-cli", version)]
struct Args {
    /// Relay base URL (e.g. https://vtcast.jamku.me or http://localhost:17239)
    #[arg(long, default_value = "https://vtcast.jamku.me")]
    relay: String,

    /// Room code. If omitted, the sender asks the relay to mint a fresh
    /// one and prints it so you can paste it into your OBS Browser Source
    /// URL.
    #[arg(long)]
    room: Option<String>,

    /// Spout sender name. Defaults to the first sender found.
    #[arg(long)]
    sender: Option<String>,

    /// Encoder target frame rate
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Encoder target bitrate in kbps. Side-by-side packing doubles the
    /// frame width, so 8000 here is ~4 Mbps for the real picture, which
    /// is HD territory at 1080p30.
    #[arg(long, default_value_t = 8000)]
    bitrate_kbps: u32,

    /// Video encoder backend. nvenc/qsv/amf use the corresponding hardware
    /// path through ffmpeg; libx264 is the software fallback. (Ignored
    /// when --backend mf is used.)
    #[arg(long, value_enum, default_value_t = EncoderKind::Libx264)]
    encoder: EncoderKind,

    /// Encoding pipeline: ffmpeg subprocess (default) or in-process Media
    /// Foundation (Windows only, no external dependency).
    #[arg(long, value_enum, default_value_t = EncoderBackend::Ffmpeg)]
    backend: EncoderBackend,

    /// Capture source: Spout shared texture (default), Window via Windows
    /// Graphics Capture, or Display via DDA (planned).
    #[arg(long, value_enum, default_value_t = SourceKind::Spout)]
    source: SourceKind,

    /// Window title (substring) when --source window. Ignored otherwise.
    #[arg(long)]
    window_title: Option<String>,

    /// Chroma key (e.g. "0,255,0,60,30" for green/threshold60/softness30).
    /// Pixels within `threshold` of the key color become transparent; the
    /// next `softness` band feathers back to opaque. Skip to leave the
    /// captured rectangle fully opaque.
    #[arg(long)]
    chroma_key: Option<String>,

    /// Maximum width of the packed (side-by-side) frame after capture.
    /// 4096 fits NVENC H.264; raise only if your encoder backend handles
    /// larger frames. Captures bigger than this are box-filter downscaled
    /// to fit while keeping aspect ratio.
    #[arg(long, default_value_t = 4096)]
    max_packed_width: u32,
}

fn parse_chroma_key(s: &str) -> Result<ChromaKey> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 5 {
        return Err(anyhow::anyhow!(
            "--chroma-key wants 'r,g,b,threshold,softness' (e.g. '0,255,0,60,30')"
        ));
    }
    let p: Vec<u8> = parts
        .iter()
        .map(|s| s.trim().parse::<u8>())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("--chroma-key parse: {e}"))?;
    Ok(ChromaKey {
        r: p[0],
        g: p[1],
        b: p[2],
        threshold: p[3],
        softness: p[4],
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,vtcast_sender=debug")),
        )
        .init();

    let args = Args::parse();
    let chroma_key = match args.chroma_key.as_deref() {
        Some(s) => Some(parse_chroma_key(s)?),
        None => None,
    };
    let config = Config {
        relay_url: args.relay,
        room: args.room,
        sender_name: args.sender,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
        encoder: args.encoder,
        backend: args.backend,
        source_kind: args.source,
        source_name: args.window_title,
        chroma_key,
        max_packed_width: args.max_packed_width,
    };

    let mut pipeline = Pipeline::start(config).await?;
    let stop = pipeline.stop_handle();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("ctrl-c, stopping pipeline");
            stop.stop();
        }
    });

    while let Some(ev) = pipeline.next_event().await {
        match ev {
            PipelineEvent::Started { room, receiver_url, source } => {
                println!("room: {}", room);
                println!("OBS receiver URL: {}", receiver_url);
                tracing::info!(
                    ?source.sender_name,
                    src = format!("{}x{}", source.width, source.height),
                    adapter = %source.adapter,
                    "pipeline started"
                );
            }
            PipelineEvent::PublisherConnected { attempt } => {
                tracing::info!(attempt, "publisher connected");
            }
            PipelineEvent::PublisherDisconnected { reason, will_retry } => {
                if will_retry {
                    tracing::warn!(%reason, "publisher disconnected, will retry");
                } else {
                    tracing::error!(%reason, "publisher disconnected, not recoverable");
                }
            }
            PipelineEvent::Publishing { aus_sent } => {
                tracing::info!(aus_sent, "publishing");
            }
            PipelineEvent::Error { detail } => {
                tracing::error!(%detail, "pipeline error");
            }
            PipelineEvent::Stopped => {
                tracing::info!("pipeline stopped");
                break;
            }
        }
    }

    signal_task.abort();
    Ok(())
}
