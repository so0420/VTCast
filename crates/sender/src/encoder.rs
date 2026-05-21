//! ffmpeg subprocess encoder.
//!
//! Phase 2A uses an out-of-process ffmpeg with libx264 because it works out
//! of the box, supports hardware backends behind a flag swap (`-c:v
//! h264_nvenc` etc.), and lets us iterate on the rest of the pipeline
//! without writing an encoder. Phase 2C will replace this with direct
//! Media Foundation calls for lower CPU and no external dependency.
//!
//! Output is H.264 Annex-B with AUD (Access Unit Delimiter, NAL type 9) at
//! each picture boundary. We split the byte stream on AUD prefixes and emit
//! each access unit's bytes (including the AUD) as one sample for the
//! webrtc-rs H.264 packetizer.

use anyhow::{anyhow, Result};
use std::io::ErrorKind;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Owning handle for the ffmpeg subprocess. `kill_on_drop(true)` is set on
/// the underlying `Command`, so dropping this struct kills the encoder
/// process — no zombie children if the main task exits early.
pub struct EncoderProcess {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum EncoderKind {
    /// Software libx264. Highest compatibility, highest CPU.
    Libx264,
    /// NVIDIA NVENC (h264_nvenc). Best on RTX GPUs; near-zero CPU.
    Nvenc,
    /// Intel Quick Sync (h264_qsv). Best on Intel iGPUs.
    Qsv,
    /// AMD AMF (h264_amf). Best on Radeon GPUs.
    Amf,
}

impl EncoderKind {
    fn codec_name(self) -> &'static str {
        match self {
            EncoderKind::Libx264 => "libx264",
            EncoderKind::Nvenc => "h264_nvenc",
            EncoderKind::Qsv => "h264_qsv",
            EncoderKind::Amf => "h264_amf",
        }
    }

    /// Extra args appended after -c:v. Tunes for low-latency real-time
    /// streaming on each encoder. AUD insertion + parameter-set repetition
    /// is handled outside this list via h264_metadata bsf so it's encoder-
    /// agnostic.
    fn tune_args(self) -> Vec<&'static str> {
        match self {
            EncoderKind::Libx264 => vec![
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-profile:v", "baseline",
            ],
            EncoderKind::Nvenc => vec![
                "-preset", "p1",            // fastest preset
                "-tune", "ull",             // ultra low latency, no B-frames
                "-rc", "cbr",
                "-zerolatency", "1",
                "-profile:v", "baseline",
            ],
            EncoderKind::Qsv => vec![
                "-preset", "veryfast",
                "-async_depth", "1",
                "-profile:v", "baseline",
            ],
            EncoderKind::Amf => vec![
                "-quality", "speed",
                "-usage", "ultralowlatency",
                "-profile:v", "main",      // AMF's baseline has quirks
            ],
        }
    }
}

pub fn start_encoder(
    kind: EncoderKind,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<EncoderProcess> {
    let size = format!("{}x{}", width, height);
    let fps_s = fps.to_string();
    let bv = format!("{}k", bitrate_kbps);
    let gop = (fps * 2).to_string(); // 2-second GOP

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel", "warning",
        "-f", "rawvideo",
        "-pix_fmt", "rgba",
        "-s", &size,
        "-framerate", &fps_s,
        "-i", "pipe:0",
        "-c:v", kind.codec_name(),
    ]);
    cmd.args(kind.tune_args());
    cmd.args([
        "-pix_fmt", "yuv420p",
        "-b:v", &bv,
        "-maxrate", &bv,
        "-bufsize", &bv,
        "-g", &gop,
        // Insert AUDs (NAL type 9) before every picture so AccessUnitParser
        // has stable boundaries, and repeat SPS/PPS on every keyframe so a
        // late-joining decoder can sync. Both via the same bitstream filter
        // so the command shape is encoder-agnostic.
        "-bsf:v", "h264_metadata=aud=insert,dump_extra=freq=k",
        "-f", "h264",
        "pipe:1",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    // Capture stderr (instead of inheriting) so ffmpeg's diagnostic
    // output goes to our tracing log — and doesn't pop a console
    // window of its own next to the Tauri app.
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    // Belt-and-suspenders: suppress the empty console window that pops
    // up next to a windowed Tauri app when ffmpeg spawns.
    // CREATE_NO_WINDOW = 0x08000000.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    tracing::info!(
        codec = kind.codec_name(),
        size = %size,
        fps,
        bitrate_kbps,
        "starting ffmpeg encoder"
    );

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            anyhow!(
                "ffmpeg not found on PATH. Install ffmpeg (https://ffmpeg.org/download.html) \
                 and ensure `ffmpeg --version` works, then re-run this command."
            )
        } else {
            anyhow!(e).context(format!("spawn ffmpeg (codec={})", kind.codec_name()))
        }
    })?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("ffmpeg stdin missing"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("ffmpeg stderr missing"))?;

    // Pump ffmpeg's stderr into tracing so configuration errors (unsupported
    // resolution, missing NVENC, etc.) show up in our log instead of
    // disappearing into a hidden console.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                tracing::warn!(target: "ffmpeg", "{}", line);
            }
        }
    });

    Ok(EncoderProcess { child, stdin, stdout })
}

/// Streaming parser that yields complete H.264 access units.
///
/// Identifies AU boundaries by AUD NAL units (type 9). The bytes between
/// consecutive AUDs (start code + AUD + following NAL units, up to the next
/// AUD's start code) are emitted as one AU.
pub struct AccessUnitParser {
    buf: Vec<u8>,
    /// Byte offset of the most recently seen AUD start code (start of the
    /// AU currently being accumulated). None if no AUD has been seen yet.
    current_au_start: Option<usize>,
}

impl AccessUnitParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(64 * 1024),
            current_au_start: None,
        }
    }

    /// Append more bytes from the encoder. Returns zero or more complete AUs
    /// found so far. The unfinished tail (the most recent AU still in
    /// progress) is retained for the next call.
    pub fn feed(&mut self, more: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(more);
        let mut aus = Vec::new();

        // Locate the first AUD if we don't have one yet. Anything before it
        // is preamble we discard.
        if self.current_au_start.is_none() {
            match find_aud(&self.buf, 0) {
                Some(first) => {
                    self.buf.drain(..first);
                    self.current_au_start = Some(0);
                }
                None => return aus,
            }
        }

        // Look for subsequent AUDs. Each one closes the current AU.
        loop {
            let current = self.current_au_start.expect("set above");
            // +4 safely skips both 3-byte (00 00 01) and 4-byte (00 00 00 01)
            // start codes without re-finding the same AUD.
            let search_from = current + 4;
            if search_from >= self.buf.len() {
                break;
            }
            match find_aud(&self.buf, search_from) {
                Some(found) => {
                    aus.push(self.buf[current..found].to_vec());
                    self.current_au_start = Some(found);
                }
                None => break,
            }
        }

        // Compact: drop bytes before the current AU so the buffer stays small.
        if let Some(start) = self.current_au_start {
            if start > 0 {
                self.buf.drain(..start);
                self.current_au_start = Some(0);
            }
        }
        aus
    }

    /// Emit any final AU still being held. Call this after the encoder
    /// stream ends. Returns None if no AU is in progress.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let start = self.current_au_start.take()?;
        if start < self.buf.len() {
            Some(self.buf[start..].to_vec())
        } else {
            None
        }
    }
}

/// Find the index of the next AUD start (start code + NAL header byte with
/// nal_unit_type == 9), beginning at `from`. Returns the offset of the
/// start code's first byte.
fn find_aud(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 4 < buf.len() {
        // 4-byte start code 00 00 00 01
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1 {
            let nal_byte = buf[i + 4];
            if nal_byte & 0x1f == 9 {
                return Some(i);
            }
            i += 4;
            continue;
        }
        // 3-byte start code 00 00 01
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let nal_byte = buf[i + 3];
            if nal_byte & 0x1f == 9 {
                return Some(i);
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_splits_on_aud() {
        // Two complete access units, each starting with AUD (00 00 00 01 09 ...)
        // and containing a slice (NAL type 1, byte 0x41).
        let stream: Vec<u8> = [
            0, 0, 0, 1, 9, 0x10, // AUD #1
            0, 0, 0, 1, 0x41, 0xab, 0xcd, // slice
            0, 0, 0, 1, 9, 0x10, // AUD #2 (boundary)
            0, 0, 0, 1, 0x41, 0xef, // slice
        ]
        .into_iter()
        .collect();
        let mut p = AccessUnitParser::new();
        let aus = p.feed(&stream);
        assert_eq!(aus.len(), 1, "first AU emitted, second still in progress");
        assert_eq!(aus[0], &stream[0..13]);
        let last = p.finish().expect("trailing AU");
        assert_eq!(last, &stream[13..]);
    }

    #[test]
    fn parser_handles_split_feeds() {
        let mut p = AccessUnitParser::new();
        // Feed bytes one at a time across two AUDs
        let stream: Vec<u8> = [
            0, 0, 0, 1, 9, 0x10, 0xaa,
            0, 0, 0, 1, 9, 0x10, 0xbb,
            0, 0, 0, 1, 9, 0x10, 0xcc,
        ]
        .into_iter()
        .collect();
        let mut total_aus = Vec::new();
        for b in &stream {
            total_aus.extend(p.feed(&[*b]));
        }
        assert_eq!(total_aus.len(), 2);
        assert_eq!(total_aus[0], &stream[0..7]);
        assert_eq!(total_aus[1], &stream[7..14]);
    }
}
