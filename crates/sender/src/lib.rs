//! vtcast-sender pipeline library.
//!
//! The CLI in `src/main.rs` is a thin driver over this crate; the Tauri
//! desktop app (`vtcast-sender-app`) drives the same [`Pipeline`] via Tauri
//! commands. All orchestration lives here so both UIs see identical
//! behaviour.

pub mod encoder;
#[cfg(windows)]
pub mod mf_encoder;
mod packer;
mod publisher;
mod resize;

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

    let publisher_loop = tokio::spawn(publisher_worker(
        relay,
        room,
        au_rx,
        frame_interval,
        Arc::clone(&shutdown),
        events_tx.clone(),
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
) {
    let mut backoff = RECONNECT_INITIAL_BACKOFF;
    let mut attempt = 0u32;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        attempt += 1;

        match publisher::Publisher::connect(&relay, &room).await {
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
