//! vtcast-sender pipeline library.
//!
//! The CLI in `src/main.rs` is a thin driver over this crate; the Tauri
//! desktop app (`vtcast-sender-app`) drives the same [`Pipeline`] via Tauri
//! commands. All orchestration lives here so both UIs see identical
//! behaviour.

pub mod encoder;
#[cfg(windows)]
pub mod gpu_convert;
#[cfg(windows)]
pub mod mf_encoder;
#[cfg(windows)]
pub mod nvenc;
mod packer;
mod publisher;
mod resize;
mod sysprio;

pub use encoder::EncoderKind;
pub use packer::ChromaKey;

/// Which input source to capture from. Renamed `source_name` interpretation
/// follows: Spout = sender name (None = first found), Window = window title
/// substring, Display = monitor index (as string, "0" / "1" / ...).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Spout,
    Window,
    Display,
}

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use vtcast_capture::{list_senders, FrameCapture, SpoutReceiver};
use webrtc::media::Sample;

use crate::publisher::PumpEnded;

/// One second of buffered access units at 30 fps. On a real disconnect the
/// encoder reader silently drops anything past this depth so ffmpeg never
/// blocks waiting for a disconnected publisher.
const AU_CHANNEL_DEPTH: usize = 30;
const RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(8);
/// Emit a [`PipelineEvent::Publishing`] every N successfully-written AUs.
/// 150 ≈ 5 s at 30 fps; the UI uses this to update its "publishing" badge.
const PUBLISHING_EMIT_EVERY: u64 = 150;
const EVENT_CHANNEL_DEPTH: usize = 64;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum EncoderBackend {
    /// Out-of-process ffmpeg subprocess. Mature, requires ffmpeg on PATH.
    Ffmpeg,
    /// In-process Media Foundation (Windows-only). No external dep, lower
    /// startup latency. Currently uses synchronous MFTs; hardware async
    /// MFTs are planned.
    #[cfg(windows)]
    Mf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub relay_url: String,
    /// `None` means the pipeline will mint a fresh room via the relay.
    pub room: Option<String>,
    /// Spout sender name (when source_kind == Spout). `None` picks the
    /// first sender found. Kept under the old name for back-compat with
    /// stored settings; for Window / Display sources see `source_name`.
    pub sender_name: Option<String>,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: EncoderKind,
    #[serde(default = "default_backend")]
    pub backend: EncoderBackend,
    /// Selects which input source the pipeline will capture from.
    #[serde(default = "default_source_kind")]
    pub source_kind: SourceKind,
    /// Window title substring (when source_kind == Window) or monitor
    /// index as a string (when source_kind == Display). Ignored for Spout.
    #[serde(default)]
    pub source_name: Option<String>,
    /// Optional chroma-key settings; applied to the captured RGBA frame
    /// before side-by-side packing. Spout sources already carry native
    /// alpha so this is normally None for them.
    #[serde(default)]
    pub chroma_key: Option<ChromaKey>,
    /// Maximum width of the packed (side-by-side) frame the encoder is
    /// allowed to receive. The capture is box-filter downscaled to fit
    /// when the raw source's `width*2` would otherwise exceed this.
    /// 4096 is the NVENC H.264 ceiling — pick higher only if you know
    /// your encoder backend handles it.
    #[serde(default = "default_max_packed_width")]
    pub max_packed_width: u32,
}

fn default_max_packed_width() -> u32 {
    4096
}

fn default_backend() -> EncoderBackend {
    EncoderBackend::Ffmpeg
}

/// Cheap probe — `ffmpeg -version` exits in tens of milliseconds when present,
/// or fails fast with NotFound when not on PATH.
#[cfg(windows)]
fn ffmpeg_available() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    // CREATE_NO_WINDOW = 0x08000000 — keep the probe silent in the windowed
    // Tauri build (would otherwise flash a console).
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
fn default_source_kind() -> SourceKind {
    SourceKind::Spout
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relay_url: "https://vtcast.jamku.me".to_string(),
            room: None,
            sender_name: None,
            fps: 30,
            // Side-by-side packing doubles the frame width, so the encoder
            // spends bits on twice the area of the visible content. 8 Mbps
            // corresponds to ~4 Mbps for the real picture, which is HD
            // territory at 1080p / 30 fps.
            bitrate_kbps: 8000,
            encoder: EncoderKind::Libx264,
            backend: EncoderBackend::Ffmpeg,
            source_kind: SourceKind::Spout,
            source_name: None,
            chroma_key: None,
            max_packed_width: default_max_packed_width(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PipelineEvent {
    /// First event after start(). Setup succeeded, the room is resolved,
    /// and worker tasks are spawned.
    Started {
        room: String,
        receiver_url: String,
        source: SourceInfo,
    },
    /// Publisher (re-)connected to the relay on attempt N.
    PublisherConnected { attempt: u32 },
    /// Publisher lost its WebRTC/WS connection. `will_retry` is false only
    /// when the relay rejected the session permanently.
    PublisherDisconnected { reason: String, will_retry: bool },
    /// Heartbeat — total AUs delivered to write_sample so far.
    Publishing { aus_sent: u64 },
    /// Fatal error; the pipeline is winding down.
    Error { detail: String },
    /// All worker tasks have exited. Pipeline is fully stopped.
    Stopped,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceInfo {
    pub sender_name: String,
    pub width: u32,
    pub height: u32,
    pub adapter: String,
}

/// A running pipeline. Drop the handle to start tearing down — but the
/// preferred path is [`Pipeline::stop`] followed by reading events until
/// [`PipelineEvent::Stopped`].
pub struct Pipeline {
    shutdown: Arc<AtomicBool>,
    events_rx: mpsc::Receiver<PipelineEvent>,
    _orchestrator: tokio::task::JoinHandle<()>,
}

impl Pipeline {
    /// Open the source, start the encoder, and spawn the orchestrator that
    /// supervises capture / encoder I/O / publisher reconnect.
    pub async fn start(config: Config) -> Result<Self> {
        // ffmpeg backend selected but ffmpeg.exe isn't on PATH? Fall back to
        // the in-process Media Foundation encoder so first-time users on a
        // bare Windows install don't hit a hard failure. We only do this on
        // Windows where Mf is actually available.
        let mut config = config;
        #[cfg(windows)]
        if matches!(config.backend, EncoderBackend::Ffmpeg) && !ffmpeg_available() {
            tracing::warn!(
                "ffmpeg.exe not found on PATH — falling back to in-process Media Foundation encoder (set --backend mf explicitly to silence)"
            );
            config.backend = EncoderBackend::Mf;
        }

        let room = match config.room.clone() {
            Some(r) => r,
            None => mint_room(&config.relay_url).await?,
        };
        let receiver_url = format!(
            "{}/r?room={}",
            config.relay_url.trim_end_matches('/'),
            room
        );

        // GPU zero-copy fast path: any Spout source on a GPU with a usable
        // hardware encoder (NVENC on NVIDIA, Media Foundation QSV/AMF on
        // Intel/AMD). Keeps the frame in VRAM end to end — no readback, no
        // pack/convert on the CPU, no raw-RGBA pipe — which is the whole point
        // of cutting host load. Preferred regardless of the `backend` setting
        // (that now selects only the fallback for when this path isn't
        // viable). Any failure here drops through to the CPU pipeline below.
        #[cfg(windows)]
        if matches!(config.source_kind, SourceKind::Spout) {
            match try_start_gpu_spout(&config, &room, &receiver_url).await {
                Some(pipeline) => return Ok(pipeline),
                None => tracing::info!(
                    "GPU zero-copy path unavailable; using CPU capture+encode pipeline"
                ),
            }
        }

        let (rx, source) = open_capture(&config)?;
        let (raw_w, raw_h) = rx.dimensions();
        // Decide what the encoder will actually see. If the captured source
        // is too wide for the encoder (NVENC H.264 caps at 4096), the
        // capture loop will box-filter it down to fit while preserving
        // aspect ratio.
        let (src_w, src_h) =
            resize::compute_effective_dims(raw_w, raw_h, config.max_packed_width);
        if (src_w, src_h) != (raw_w, raw_h) {
            tracing::info!(
                raw = format!("{}x{}", raw_w, raw_h),
                effective = format!("{}x{}", src_w, src_h),
                cap = config.max_packed_width,
                "downscaling capture to fit encoder limit"
            );
        }
        let (packed_w, packed_h) = packer::packed_dims(src_w, src_h);
        tracing::info!(
            source = %source.sender_name,
            kind = ?config.source_kind,
            src = format!("{}x{}", src_w, src_h),
            packed = format!("{}x{}", packed_w, packed_h),
            adapter = %source.adapter,
            backend = ?config.backend,
            "capture opened"
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let frame_interval = Duration::from_secs_f64(1.0 / config.fps as f64);
        let (events_tx, events_rx) = mpsc::channel::<PipelineEvent>(EVENT_CHANNEL_DEPTH);

        let _ = events_tx
            .send(PipelineEvent::Started {
                room: room.clone(),
                receiver_url,
                source,
            })
            .await;

        let ctx = OrchestratorCtx {
            rx,
            raw_w,
            raw_h,
            src_w,
            src_h,
            packed_w,
            packed_h,
            relay: config.relay_url.clone(),
            room,
            frame_interval,
            shutdown: Arc::clone(&shutdown),
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            encoder_kind: config.encoder,
            backend: config.backend,
            chroma_key: config.chroma_key,
        };

        let orchestrator = tokio::spawn(orchestrate(ctx, events_tx));

        Ok(Pipeline {
            shutdown,
            events_rx,
            _orchestrator: orchestrator,
        })
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Cheaply cloneable stop signal so callers can shut the pipeline down
    /// from another task (e.g. a Ctrl+C handler) without holding the
    /// `Pipeline` borrow.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(Arc::clone(&self.shutdown))
    }

    /// Async-await the next event. Returns `None` once the pipeline is fully
    /// torn down (after a [`PipelineEvent::Stopped`]).
    pub async fn next_event(&mut self) -> Option<PipelineEvent> {
        self.events_rx.recv().await
    }
}

pub fn list_spout_senders() -> Result<Vec<String>> {
    list_senders().context("list_senders")
}

#[cfg(windows)]
pub fn list_capture_windows() -> Result<Vec<String>> {
    vtcast_capture::wgc::list_windows()
}

#[cfg(not(windows))]
pub fn list_capture_windows() -> Result<Vec<String>> {
    Err(anyhow!("window capture is Windows-only"))
}

fn open_capture(config: &Config) -> Result<(Box<dyn FrameCapture>, SourceInfo)> {
    match config.source_kind {
        SourceKind::Spout => open_spout_capture(config),
        SourceKind::Window => open_window_capture(config),
        SourceKind::Display => open_display_capture(config),
    }
}

fn open_spout_capture(config: &Config) -> Result<(Box<dyn FrameCapture>, SourceInfo)> {
    let sender_name = match config.sender_name.clone() {
        Some(n) => n,
        None => list_senders()
            .context("list_senders")?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Spout senders broadcasting"))?,
    };
    let spout = SpoutReceiver::open(&sender_name)?;
    let adapter = spout.adapter_name().to_string();
    let (w, h) = spout.dimensions();
    let info = SourceInfo {
        sender_name: sender_name.clone(),
        width: w,
        height: h,
        adapter,
    };
    Ok((Box::new(spout), info))
}

#[cfg(windows)]
fn open_window_capture(config: &Config) -> Result<(Box<dyn FrameCapture>, SourceInfo)> {
    let title = config
        .source_name
        .clone()
        .ok_or_else(|| anyhow!("source_name (window title) required for window capture"))?;
    let wgc = vtcast_capture::wgc::WgcCapture::open_by_title(&title, true)?;
    let (w, h) = wgc.dimensions();
    let resolved_title = wgc.source_name().to_string();
    let info = SourceInfo {
        sender_name: resolved_title,
        width: w,
        height: h,
        adapter: "WGC".to_string(),
    };
    Ok((Box::new(wgc), info))
}

#[cfg(not(windows))]
fn open_window_capture(_config: &Config) -> Result<(Box<dyn FrameCapture>, SourceInfo)> {
    Err(anyhow!("window capture is Windows-only"))
}

#[cfg(windows)]
fn open_display_capture(config: &Config) -> Result<(Box<dyn FrameCapture>, SourceInfo)> {
    let index = config
        .source_name
        .as_deref()
        .and_then(|s| s.parse::<usize>().ok());
    let dda = vtcast_capture::dda::DdaCapture::open(index)?;
    let (w, h) = dda.dimensions();
    let name = dda.source_name().to_string();
    let info = SourceInfo {
        sender_name: name,
        width: w,
        height: h,
        adapter: "DDA".to_string(),
    };
    Ok((Box::new(dda), info))
}

#[cfg(not(windows))]
fn open_display_capture(_config: &Config) -> Result<(Box<dyn FrameCapture>, SourceInfo)> {
    Err(anyhow!("display capture is Windows-only"))
}

#[cfg(windows)]
pub fn list_capture_displays() -> Result<Vec<(usize, String)>> {
    vtcast_capture::dda::list_displays()
}

#[cfg(not(windows))]
pub fn list_capture_displays() -> Result<Vec<(usize, String)>> {
    Err(anyhow!("display capture is Windows-only"))
}

/// Cheaply cloneable handle that triggers a pipeline shutdown.
#[derive(Clone)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct OrchestratorCtx {
    rx: Box<dyn FrameCapture>,
    /// Native dimensions reported by the capture source.
    raw_w: u32,
    raw_h: u32,
    /// Dimensions actually fed to the packer / encoder (== raw when no
    /// downscale is required).
    src_w: u32,
    src_h: u32,
    packed_w: u32,
    packed_h: u32,
    relay: String,
    room: String,
    frame_interval: Duration,
    shutdown: Arc<AtomicBool>,
    fps: u32,
    bitrate_kbps: u32,
    encoder_kind: EncoderKind,
    backend: EncoderBackend,
    chroma_key: Option<ChromaKey>,
}

async fn orchestrate(ctx: OrchestratorCtx, events_tx: mpsc::Sender<PipelineEvent>) {
    let OrchestratorCtx {
        rx,
        raw_w,
        raw_h,
        src_w,
        src_h,
        packed_w,
        packed_h,
        relay,
        room,
        frame_interval,
        shutdown,
        fps,
        bitrate_kbps,
        encoder_kind,
        backend,
        chroma_key,
    } = ctx;

    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(2);
    let shutdown_capture = Arc::clone(&shutdown);
    let capture_task = tokio::task::spawn_blocking(move || {
        // Yield to audio / UI threads under load — this loop does the CPU-path
        // pack/convert/resize and shouldn't compete with the streamer's audio.
        sysprio::lower_current_thread_priority("capture");
        let mut rx = rx;
        let raw_size = (raw_w * raw_h * 4) as usize;
        let src_size = (src_w * src_h * 4) as usize;
        let packed_size = (packed_w * packed_h * 4) as usize;
        let mut raw_buf = vec![0u8; raw_size];
        // src_buf points to whichever buffer the chroma-key / packer step
        // consumes. When no downscale is needed, raw == src and we skip
        // the intermediate copy/allocation.
        let needs_resize = (raw_w, raw_h) != (src_w, src_h);
        let mut resized_buf = if needs_resize {
            vec![0u8; src_size]
        } else {
            Vec::new()
        };
        let mut next = std::time::Instant::now();
        loop {
            if shutdown_capture.load(Ordering::Relaxed) {
                tracing::info!("capture loop shutting down");
                return;
            }
            if let Err(e) = rx.grab_into(&mut raw_buf) {
                tracing::warn!(error = ?e, "grab failed");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let src_slice: &mut [u8] = if needs_resize {
                resize::box_resize_rgba(&raw_buf, raw_w, raw_h, &mut resized_buf, src_w, src_h);
                &mut resized_buf
            } else {
                &mut raw_buf
            };
            if let Some(ck) = &chroma_key {
                packer::apply_chroma_key(src_slice, ck);
            }
            let mut packed = vec![0u8; packed_size];
            packer::pack_rgba_side_by_side(src_slice, src_w, src_h, &mut packed);
            if frame_tx.blocking_send(packed).is_err() {
                tracing::info!("frame channel closed, capture ending");
                return;
            }
            next += frame_interval;
            let now = std::time::Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    });

    let (au_tx, au_rx) = mpsc::channel::<Vec<u8>>(AU_CHANNEL_DEPTH);

    // Backend-specific encoder tasks. Each consumes packed-RGBA frames
    // from `frame_rx` and emits H.264 access units into `au_tx`.
    let (encode_a, encode_b) = match backend {
        EncoderBackend::Ffmpeg => {
            spawn_ffmpeg_encode(frame_rx, au_tx, packed_w, packed_h, fps, bitrate_kbps, encoder_kind)
                .ok()
                .unwrap_or_else(|| {
                    // start_encoder errors are surfaced via the orchestrator
                    // exit, not here — return dummy handles
                    let dummy_a = tokio::spawn(async {});
                    let dummy_b = tokio::spawn(async {});
                    (dummy_a, dummy_b)
                })
        }
        #[cfg(windows)]
        EncoderBackend::Mf => spawn_mf_encode(frame_rx, au_tx, packed_w, packed_h, fps, bitrate_kbps),
    };

    // The CPU-path encoders (ffmpeg subprocess / MF byte-stream) have no
    // force-IDR hook, so PLI just sets a flag nobody reads there; the NACK
    // retransmission enabled inside the publisher is what matters for them.
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let publisher_loop = tokio::spawn(publisher_worker(
        relay,
        room,
        au_rx,
        frame_interval,
        Arc::clone(&shutdown),
        events_tx.clone(),
        force_keyframe,
    ));

    tokio::select! {
        r = capture_task => tracing::info!(result = ?r, "capture exited"),
        _ = encode_a => tracing::info!("encode pipeline task a exited"),
        _ = encode_b => tracing::info!("encode pipeline task b exited"),
        r = publisher_loop => tracing::info!(reason = ?r, "publisher loop exited"),
    }

    shutdown.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = events_tx.send(PipelineEvent::Stopped).await;
}

/// Spawn the ffmpeg subprocess plus its stdin writer + stdout reader.
/// Returns two JoinHandles (writer + reader) the orchestrator awaits on
/// alongside capture and publisher.
fn spawn_ffmpeg_encode(
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    au_tx: mpsc::Sender<Vec<u8>>,
    packed_w: u32,
    packed_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    encoder_kind: EncoderKind,
) -> Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> {
    let encoder_proc = encoder::start_encoder(encoder_kind, packed_w, packed_h, fps, bitrate_kbps)?;
    let encoder::EncoderProcess {
        child: _child,
        mut stdin,
        mut stdout,
    } = encoder_proc;

    let stdin_writer = tokio::spawn(async move {
        // Keep the child alive for the duration of this task — it would
        // otherwise be reaped by Drop when this scope ends.
        let _keep = _child;
        while let Some(frame) = frame_rx.recv().await {
            if let Err(e) = stdin.write_all(&frame).await {
                tracing::warn!(error = ?e, "stdin write");
                break;
            }
        }
        drop(stdin);
        // Hold the child until ffmpeg fully exits, then let _keep drop.
        let _ = _keep;
    });

    let stdout_reader = tokio::spawn(async move {
        let mut parser = encoder::AccessUnitParser::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut aus_produced: u64 = 0;
        let mut aus_dropped: u64 = 0;
        loop {
            let n = match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = ?e, "stdout read");
                    break;
                }
            };
            for au in parser.feed(&buf[..n]) {
                aus_produced += 1;
                if au_tx.try_send(au).is_err() {
                    aus_dropped += 1;
                }
            }
        }
        if let Some(tail) = parser.finish() {
            let _ = au_tx.try_send(tail);
        }
        tracing::info!(aus_produced, aus_dropped, "ffmpeg stdout reader ended");
    });

    Ok((stdin_writer, stdout_reader))
}

/// Spawn the Media Foundation encoder on a blocking thread. Pulls
/// packed-RGBA frames, converts to NV12, encodes, and forwards access
/// units to the publisher. Returns two JoinHandles — only one is
/// meaningfully used (the second is an immediately-completing stub) so
/// the orchestrator's select! arms line up between backends.
#[cfg(windows)]
fn spawn_mf_encode(
    frame_rx: mpsc::Receiver<Vec<u8>>,
    au_tx: mpsc::Sender<Vec<u8>>,
    packed_w: u32,
    packed_h: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let encoder_task = tokio::task::spawn_blocking(move || {
        sysprio::lower_current_thread_priority("mf-encode");
        // Selection order: NVIDIA async → Intel QSV async → AMD AMF async →
        // Microsoft DX12 sync (GPU but synchronous) → MS software MFT.
        // The async-hardware path is much lower CPU but more code to drive,
        // so we fall back through them.
        for name in ["nvidia", "quick sync", "amf"] {
            match mf_encoder::AsyncMfEncoder::open(name, packed_w, packed_h, fps, bitrate_kbps) {
                Ok(enc) => {
                    tracing::info!(mft = name, "MF async hardware encoder opened");
                    run_async_mf(enc, frame_rx, au_tx, packed_w, packed_h);
                    return;
                }
                Err(e) => tracing::debug!(mft = name, error = ?e, "async MFT not usable"),
            }
        }
        for name in ["microsoft avc dx12", "h264 encoder mft"] {
            match mf_encoder::MfEncoder::open(name, packed_w, packed_h, fps, bitrate_kbps) {
                Ok(enc) => {
                    tracing::info!(mft = name, "MF sync encoder opened");
                    run_sync_mf(enc, frame_rx, au_tx, packed_w, packed_h);
                    return;
                }
                Err(e) => tracing::debug!(mft = name, error = ?e, "sync MFT not usable"),
            }
        }
        tracing::error!("no usable MF encoder found");
    });
    let stub = tokio::spawn(async move {
        std::future::pending::<()>().await;
    });
    (encoder_task, stub)
}

#[cfg(windows)]
fn run_sync_mf(
    mut encoder: mf_encoder::MfEncoder,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    au_tx: mpsc::Sender<Vec<u8>>,
    packed_w: u32,
    packed_h: u32,
) {
    let mut nv12 = vec![0u8; (packed_w as usize) * (packed_h as usize) * 3 / 2];
    let mut aus_produced: u64 = 0;
    let mut aus_dropped: u64 = 0;
    while let Some(packed_rgba) = frame_rx.blocking_recv() {
        mf_encoder::rgba_to_nv12(&packed_rgba, packed_w, packed_h, &mut nv12);
        let aus = match encoder.encode_nv12(&nv12) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = ?e, "encode_nv12 failed");
                break;
            }
        };
        for au in aus {
            aus_produced += 1;
            if au_tx.try_send(au).is_err() {
                aus_dropped += 1;
            }
        }
    }
    let tail = encoder.finish().unwrap_or_default();
    for au in tail {
        let _ = au_tx.try_send(au);
    }
    tracing::info!(aus_produced, aus_dropped, "MF sync encoder ended");
}

#[cfg(windows)]
fn run_async_mf(
    mut encoder: mf_encoder::AsyncMfEncoder,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    au_tx: mpsc::Sender<Vec<u8>>,
    packed_w: u32,
    packed_h: u32,
) {
    use std::collections::VecDeque;
    let mut nv12_buf = vec![0u8; (packed_w as usize) * (packed_h as usize) * 3 / 2];
    let mut queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(4);
    let mut aus_produced: u64 = 0;
    let mut aus_dropped: u64 = 0;

    loop {
        // Refill input queue from packer (non-blocking, cap depth at 4
        // frames so we don't burn unbounded memory if the encoder stalls).
        while queue.len() < 4 {
            match frame_rx.try_recv() {
                Ok(rgba) => {
                    mf_encoder::rgba_to_nv12(&rgba, packed_w, packed_h, &mut nv12_buf);
                    queue.push_back(nv12_buf.clone());
                }
                Err(_) => break,
            }
        }

        // If we have neither input nor outstanding encoder requests,
        // block for the next frame — otherwise stay responsive to events.
        if queue.is_empty() && !encoder.has_pending_requests() {
            match frame_rx.blocking_recv() {
                Some(rgba) => {
                    mf_encoder::rgba_to_nv12(&rgba, packed_w, packed_h, &mut nv12_buf);
                    queue.push_back(nv12_buf.clone());
                }
                None => break,
            }
        }

        let aus = match encoder.pump(&mut queue, 32) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = ?e, "async MF pump failed");
                break;
            }
        };
        for au in aus {
            aus_produced += 1;
            if au_tx.try_send(au).is_err() {
                aus_dropped += 1;
            }
        }

        // Yield a tick so the GPU has time to surface NeedInput / HaveOutput.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let tail = encoder.finish().unwrap_or_default();
    for au in tail {
        let _ = au_tx.try_send(au);
    }
    tracing::info!(aus_produced, aus_dropped, "MF async encoder ended");
}

async fn publisher_worker(
    relay: String,
    room: String,
    mut au_rx: mpsc::Receiver<Vec<u8>>,
    frame_interval: Duration,
    shutdown: Arc<AtomicBool>,
    events_tx: mpsc::Sender<PipelineEvent>,
    force_keyframe: Arc<AtomicBool>,
) {
    let mut backoff = RECONNECT_INITIAL_BACKOFF;
    let mut attempt = 0u32;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        attempt += 1;

        match publisher::Publisher::connect(&relay, &room, Arc::clone(&force_keyframe)).await {
            Ok((handle, mut pump)) => {
                tracing::debug!(attempt, "publisher connected");
                backoff = RECONNECT_INITIAL_BACKOFF;
                let _ = events_tx
                    .send(PipelineEvent::PublisherConnected { attempt })
                    .await;

                // Drop stale AUs buffered during the down period so we don't
                // burst-send a second of old frames on reconnect.
                let mut drained = 0u64;
                while au_rx.try_recv().is_ok() {
                    drained += 1;
                }
                if drained > 0 {
                    tracing::debug!(drained, "discarded stale AUs from buffer");
                }

                let track = Arc::clone(&handle.track);
                let mut aus_sent: u64 = 0;
                loop {
                    tokio::select! {
                        au = au_rx.recv() => {
                            let Some(au) = au else {
                                tracing::info!("AU channel closed, ending publisher loop");
                                return;
                            };
                            let sample = Sample {
                                data: Bytes::from(au),
                                duration: frame_interval,
                                ..Default::default()
                            };
                            if let Err(e) = track.write_sample(&sample).await {
                                tracing::warn!(error = ?e, "write_sample");
                            }
                            aus_sent += 1;
                            if aus_sent % PUBLISHING_EMIT_EVERY == 0 {
                                let _ = events_tx
                                    .send(PipelineEvent::Publishing { aus_sent })
                                    .await;
                            }
                        }
                        result = &mut pump => {
                            let ended = result.unwrap_or(PumpEnded::WsClosed);
                            if let PumpEnded::RelayError(detail) = &ended {
                                tracing::debug!(%detail, "relay error — not recoverable, exiting");
                                let _ = events_tx
                                    .send(PipelineEvent::PublisherDisconnected {
                                        reason: detail.clone(),
                                        will_retry: false,
                                    })
                                    .await;
                                let _ = events_tx
                                    .send(PipelineEvent::Error { detail: detail.clone() })
                                    .await;
                                return;
                            }
                            tracing::debug!(?ended, "publisher disconnected, will retry");
                            let _ = events_tx
                                .send(PipelineEvent::PublisherDisconnected {
                                    reason: format!("{:?}", ended),
                                    will_retry: true,
                                })
                                .await;
                            break;
                        }
                    }
                }
                drop(handle);
            }
            Err(e) => {
                tracing::warn!(attempt, error = ?e, ?backoff, "connect failed, retrying");
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, RECONNECT_MAX_BACKOFF);
    }
}

/// Resolve the Spout sender name the GPU path will open: explicit config
/// value, else the first broadcasting sender.
#[cfg(windows)]
fn resolve_spout_name(config: &Config) -> Option<String> {
    match config.sender_name.clone() {
        Some(n) => Some(n),
        None => list_senders().ok()?.into_iter().next(),
    }
}

/// Try to stand up the GPU zero-copy pipeline for a Spout source. Returns
/// `Some(Pipeline)` once the source is validated and the encode task is
/// spawned, or `None` to signal the caller to fall back to the CPU pipeline.
///
/// The validation probe opens the Spout handle on a shared D3D11 device to
/// confirm eligibility and read dimensions/adapter, then drops it — the
/// encode worker re-opens on its own thread because the D3D/MF COM objects
/// aren't `Send`.
#[cfg(windows)]
async fn try_start_gpu_spout(config: &Config, room: &str, receiver_url: &str) -> Option<Pipeline> {
    let name = resolve_spout_name(config)?;

    let (raw_w, raw_h, adapter) = {
        let (recv, _vendor) = match SpoutReceiver::open_shared(&name) {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!(error = ?e, "open_shared probe failed; GPU path skipped");
                return None;
            }
        };
        let (w, h) = recv.dimensions();
        (w, h, recv.adapter_name().to_string())
        // recv dropped here — re-opened on the worker thread.
    };

    // Eligibility depends on the capture GPU's vendor: NVIDIA goes through the
    // NVENC SDK (its MF MFT is broken), everyone else through a Media
    // Foundation hardware MFT. Probe the matching encoder's availability
    // cheaply before committing — otherwise fall back to the CPU pipeline.
    let viable = if adapter.to_lowercase().contains("nvidia") {
        crate::nvenc::NvencApi::load().is_ok()
    } else {
        mf_encoder::hardware_h264_available()
    };
    if !viable {
        tracing::info!(%adapter, "no usable GPU encoder for this adapter; using CPU pipeline");
        return None;
    }

    let (src_w, src_h) = resize::compute_effective_dims(raw_w, raw_h, config.max_packed_width);
    let (packed_w, packed_h) = packer::packed_dims(src_w, src_h);
    tracing::info!(
        source = %name,
        src = format!("{src_w}x{src_h}"),
        packed = format!("{packed_w}x{packed_h}"),
        %adapter,
        "GPU zero-copy pipeline opening"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let frame_interval = Duration::from_secs_f64(1.0 / config.fps as f64);
    let (events_tx, events_rx) = mpsc::channel::<PipelineEvent>(EVENT_CHANNEL_DEPTH);
    let (au_tx, au_rx) = mpsc::channel::<Vec<u8>>(AU_CHANNEL_DEPTH);
    // The worker reports whether it got all the way through init (device +
    // converter + encoder + a warmup frame) before we commit to the GPU path.
    let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

    let force_keyframe = Arc::new(AtomicBool::new(false));
    let ctx = GpuCtx {
        sender_name: name.clone(),
        adapter: adapter.clone(),
        packed_w,
        packed_h,
        fps: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        frame_interval,
        shutdown: Arc::clone(&shutdown),
        force_keyframe: Arc::clone(&force_keyframe),
    };
    let encode_task = tokio::task::spawn_blocking(move || {
        sysprio::lower_current_thread_priority("gpu-encode");
        gpu_encode_loop(ctx, au_tx, init_tx)
    });

    // Block on init before committing. Any failure (or timeout, or the worker
    // dying early) drops us back to the CPU pipeline instead of leaving a dead
    // GPU pipeline. The warmup encode inside the worker means a successful
    // signal has actually produced one frame end-to-end.
    match tokio::time::timeout(Duration::from_secs(10), init_rx).await {
        Ok(Ok(Ok(()))) => {}
        other => {
            shutdown.store(true, Ordering::Relaxed);
            let reason = match other {
                Ok(Ok(Err(e))) => e,
                Ok(Err(_)) => "GPU worker exited before signalling init".to_string(),
                Err(_) => "GPU worker init timed out".to_string(),
                Ok(Ok(Ok(()))) => unreachable!(),
            };
            tracing::info!(%reason, "GPU init failed; falling back to CPU pipeline");
            return None;
        }
    }

    // Committed: announce the source and wire up the publisher + supervisor.
    let source = SourceInfo {
        sender_name: name,
        width: src_w,
        height: src_h,
        adapter: format!("{adapter} (GPU)"),
    };
    let _ = events_tx
        .send(PipelineEvent::Started {
            room: room.to_string(),
            receiver_url: receiver_url.to_string(),
            source,
        })
        .await;

    let publisher_loop = tokio::spawn(publisher_worker(
        config.relay_url.clone(),
        room.to_string(),
        au_rx,
        frame_interval,
        Arc::clone(&shutdown),
        events_tx.clone(),
        force_keyframe,
    ));

    let sup_shutdown = Arc::clone(&shutdown);
    let orchestrator = tokio::spawn(async move {
        tokio::select! {
            r = encode_task => tracing::info!(result = ?r, "GPU encode task exited"),
            r = publisher_loop => tracing::info!(reason = ?r, "publisher loop exited (GPU)"),
        }
        sup_shutdown.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = events_tx.send(PipelineEvent::Stopped).await;
    });

    Some(Pipeline {
        shutdown,
        events_rx,
        _orchestrator: orchestrator,
    })
}

#[cfg(windows)]
struct GpuCtx {
    sender_name: String,
    /// Adapter description of the GPU the Spout texture lives on; drives the
    /// NVENC-vs-MF branch and the preferred encoder-MFT vendor order.
    adapter: String,
    packed_w: u32,
    packed_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    frame_interval: Duration,
    shutdown: Arc<AtomicBool>,
    /// Raised by the publisher when a subscriber reports picture loss (PLI);
    /// the NVENC loop swaps it to false and forces an immediate IDR.
    force_keyframe: Arc<AtomicBool>,
}

/// The fused GPU pipeline body, run on a blocking thread. Re-opens the Spout
/// receiver here (COM objects aren't `Send`), builds the NV12 converter +
/// encoder on the single shared device, then loops: snapshot+convert on the
/// GPU, hand the NV12 texture straight to the encoder, forward access units.
#[cfg(windows)]
fn gpu_encode_loop(
    ctx: GpuCtx,
    au_tx: mpsc::Sender<Vec<u8>>,
    init_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    use crate::gpu_convert::Nv12Converter;

    let GpuCtx {
        sender_name,
        adapter,
        packed_w,
        packed_h,
        fps,
        bitrate_kbps,
        frame_interval,
        shutdown,
        force_keyframe,
    } = ctx;

    let (recv, _vendor) = match SpoutReceiver::open_shared(&sender_name) {
        Ok(x) => x,
        Err(e) => {
            let _ = init_tx.send(Err(format!("re-open Spout on worker thread: {e:#}")));
            return;
        }
    };
    let device = recv.device().clone();
    let context = recv.context().clone();
    // Deprioritise our GPU submissions relative to the game / OBS / compositor
    // sharing this GPU, so a saturated card doesn't delay their work (which can
    // surface as audio-driver DPC latency / mic crackle for the broadcaster).
    sysprio::lower_gpu_priority(&device);
    let (src_w, src_h) = recv.dimensions();
    let src_format = recv.format();

    let converter = match Nv12Converter::new(
        &device,
        &context,
        recv.shared_texture(),
        src_w,
        src_h,
        src_format,
        packed_w,
        packed_h,
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("NV12 converter init: {e:#}")));
            return;
        }
    };

    // NVIDIA's Media Foundation MFT is broken on current drivers, so NVIDIA
    // takes the direct-NVENC path; Intel/AMD use their (working) MF MFT. Both
    // consume the converter's NV12 textures straight from VRAM, and both
    // signal `init_tx` once (Ok after a warmup / encoder open, Err on failure)
    // so the supervisor can fall back to the CPU pipeline if init fails.
    let la = adapter.to_lowercase();
    if la.contains("nvidia") {
        run_nvenc_encode(
            &device,
            &converter,
            packed_w,
            packed_h,
            fps,
            bitrate_kbps,
            frame_interval,
            &shutdown,
            &force_keyframe,
            &au_tx,
            init_tx,
        );
    } else {
        run_mf_gpu_encode(
            &device,
            &converter,
            packed_w,
            packed_h,
            fps,
            bitrate_kbps,
            frame_interval,
            &shutdown,
            &au_tx,
            init_tx,
        );
    }

    // Keep the receiver alive for the whole loop; its textures back the
    // converter's source clone.
    drop(recv);
    tracing::info!("GPU encode loop ended");
}

/// NVIDIA path: encode the converter's NV12 textures directly through the
/// NVENC SDK (zero-copy from VRAM). Synchronous — one access unit per frame.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_nvenc_encode(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    converter: &crate::gpu_convert::Nv12Converter,
    packed_w: u32,
    packed_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    frame_interval: Duration,
    shutdown: &Arc<AtomicBool>,
    force_keyframe: &Arc<AtomicBool>,
    au_tx: &mpsc::Sender<Vec<u8>>,
    init_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    // Init: load → open session → configure. Each failure reports via init_tx
    // so the supervisor falls back to the CPU pipeline.
    let api = match crate::nvenc::NvencApi::load() {
        Ok(a) => a,
        Err(e) => {
            let _ = init_tx.send(Err(format!("NVENC load: {e:#}")));
            return;
        }
    };
    let mut session = match api.open_session(device) {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(format!("NVENC open_session: {e:#}")));
            return;
        }
    };
    if let Err(e) = session.initialize(packed_w, packed_h, fps, bitrate_kbps) {
        let _ = init_tx.send(Err(format!("NVENC initialize: {e:#}")));
        return;
    }

    // Warmup: convert + encode one frame so registering the input texture and
    // the first encode are validated before we commit. The warmup AU is a
    // valid keyframe, so forward it.
    let mut produced: u64 = 0;
    let mut dropped: u64 = 0;
    match converter.convert() {
        Ok(tex) => match session.encode_texture(tex, false) {
            Ok(au_opt) => {
                let _ = init_tx.send(Ok(()));
                if let Some(au) = au_opt {
                    produced += 1;
                    let _ = au_tx.try_send(au);
                }
            }
            Err(e) => {
                let _ = init_tx.send(Err(format!("NVENC warmup encode: {e:#}")));
                return;
            }
        },
        Err(e) => {
            let _ = init_tx.send(Err(format!("NV12 convert warmup: {e:#}")));
            return;
        }
    }
    tracing::info!(
        size = format!("{packed_w}x{packed_h}"),
        "NVENC zero-copy encoder initialized"
    );

    let mut next = std::time::Instant::now();
    while !shutdown.load(Ordering::Relaxed) {
        // A subscriber asked for a keyframe (PLI after packet loss) — make
        // this frame an IDR so it can re-sync now, not at the next GOP.
        let force_idr = force_keyframe.swap(false, Ordering::Relaxed);
        match converter.convert() {
            Ok(tex) => match session.encode_texture(tex, force_idr) {
                Ok(Some(au)) => {
                    produced += 1;
                    if au_tx.try_send(au).is_err() {
                        dropped += 1;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = ?e, "NVENC encode_texture failed");
                    break;
                }
            },
            Err(e) => {
                tracing::warn!(error = ?e, "NVENC loop: convert failed");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        next += frame_interval;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    tracing::info!(produced, dropped, "NVENC encode loop ended");
}

/// Intel/AMD path: feed the converter's NV12 textures to the vendor's Media
/// Foundation hardware MFT (which, unlike NVIDIA's, works) on the same GPU.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_mf_gpu_encode(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    converter: &crate::gpu_convert::Nv12Converter,
    packed_w: u32,
    packed_h: u32,
    fps: u32,
    bitrate_kbps: u32,
    frame_interval: Duration,
    shutdown: &Arc<AtomicBool>,
    au_tx: &mpsc::Sender<Vec<u8>>,
    init_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    use crate::mf_encoder::AsyncMfEncoder;
    let mut encoder = None;
    for name in ["quick sync", "amf", "nvidia"] {
        match AsyncMfEncoder::open_with_device(name, device, packed_w, packed_h, fps, bitrate_kbps) {
            Ok(e) => {
                tracing::info!(mft = name, "GPU MF encoder opened on shared device");
                encoder = Some(e);
                break;
            }
            Err(e) => tracing::debug!(mft = name, error = ?e, "shared-device MFT not usable"),
        }
    }
    let Some(mut encoder) = encoder else {
        let _ = init_tx.send(Err(
            "no usable hardware MFT opened on the shared device".to_string()
        ));
        return;
    };
    // Opening the MFT validated the NV12-in/H.264-out params (SetInputType /
    // SetOutputType), which is the main failure mode — signal success.
    let _ = init_tx.send(Ok(()));

    let mut produced: u64 = 0;
    let mut dropped: u64 = 0;
    let mut next = std::time::Instant::now();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Cap convert-ahead at 2 frames so we never overwrite an NV12 ring
        // slot the encoder still references.
        if encoder.texture_backlog() < 2 {
            match converter.convert() {
                Ok(nv12) => {
                    if let Err(e) = encoder.enqueue_texture(nv12) {
                        tracing::error!(error = ?e, "GPU loop: enqueue_texture failed");
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "GPU loop: convert failed");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
        }
        match encoder.pump_textures(32) {
            Ok(aus) => {
                for au in aus {
                    produced += 1;
                    if au_tx.try_send(au).is_err() {
                        dropped += 1;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = ?e, "GPU loop: pump_textures failed");
                break;
            }
        }
        next += frame_interval;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    let tail = encoder.finish().unwrap_or_default();
    for au in tail {
        let _ = au_tx.try_send(au);
    }
    tracing::info!(produced, dropped, "MF GPU encode loop ended");
}

async fn mint_room(relay: &str) -> Result<String> {
    let url = format!("{}/api/new-room", relay.trim_end_matches('/'));
    #[derive(serde::Deserialize)]
    struct R {
        code: String,
    }
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {} — is the relay running?", url))?;
    let r: R = resp.json().await.context("decode /api/new-room response")?;
    Ok(r.code)
}
