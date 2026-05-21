//! Diagnostic for the async hardware MFT path. Tries each vendor in
//! turn, generates 60 test NV12 frames, and reports what worked.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::collections::VecDeque;
    use std::time::Duration;
    use vtcast_sender::mf_encoder::{rgba_to_nv12, AsyncMfEncoder};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let width = 1280u32;
    let height = 720u32;
    let fps = 30u32;
    let bitrate_kbps = 4000u32;

    for name in ["nvidia", "quick sync", "amf"] {
        println!("\n=== trying '{}' ===", name);
        let mut enc = match AsyncMfEncoder::open(name, width, height, fps, bitrate_kbps) {
            Ok(e) => {
                println!("  opened: {}", name);
                e
            }
            Err(e) => {
                println!("  FAILED: {:#}", e);
                continue;
            }
        };

        let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
        let mut nv12 = vec![0u8; (width as usize) * (height as usize) * 3 / 2];
        let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
        let mut aus_total = 0usize;
        let mut first_au_size: Option<usize> = None;
        let mut stream_changes = 0u32;

        let start = std::time::Instant::now();
        let mut frames_pushed = 0u32;
        while start.elapsed() < Duration::from_secs(4) {
            // Generate up to 4 frames in queue
            while queue.len() < 4 && frames_pushed < 60 {
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let i = (y * width as usize + x) * 4;
                        let phase = ((frames_pushed as usize + x / 4) % 256) as u8;
                        rgba[i] = phase;
                        rgba[i + 1] = (255 - phase) as u8;
                        rgba[i + 2] = 128;
                        rgba[i + 3] = 255;
                    }
                }
                rgba_to_nv12(&rgba, width, height, &mut nv12);
                queue.push_back(nv12.clone());
                frames_pushed += 1;
            }

            match enc.pump(&mut queue, 32) {
                Ok(aus) => {
                    if !aus.is_empty() {
                        if first_au_size.is_none() {
                            first_au_size = Some(aus[0].len());
                            let head_n = aus[0].len().min(16);
                            println!(
                                "  first AU: {} bytes (first 16 = {:02x?})",
                                aus[0].len(),
                                &aus[0][..head_n]
                            );
                        }
                        aus_total += aus.len();
                    }
                }
                Err(e) => {
                    let detail = format!("{:#}", e);
                    if detail.contains("stream change") || detail.contains("STREAM_CHANGE") {
                        stream_changes += 1;
                    }
                    println!("  pump error: {}", detail);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        match enc.finish() {
            Ok(tail) => {
                aus_total += tail.len();
            }
            Err(e) => println!("  finish error: {:#}", e),
        }

        println!(
            "  result: frames_pushed={} aus_total={} stream_changes={}",
            frames_pushed, aus_total, stream_changes
        );
        if aus_total > 0 {
            println!("  ✓ {} works", name);
            return Ok(());
        }
    }

    anyhow::bail!("no async MFT produced any AUs")
}

#[cfg(not(windows))]
fn main() {
    eprintln!("encode_test_async is Windows-only");
    std::process::exit(1);
}
