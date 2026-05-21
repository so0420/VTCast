//! Diagnostic: open the Microsoft software H.264 MFT, feed it 60 frames
//! of a moving test pattern, and print how many access units came back
//! plus the size of the first one.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use vtcast_sender::mf_encoder::{rgba_to_nv12, MfEncoder};

    let width = 1280u32;
    let height = 720u32;
    let fps = 30u32;
    let bitrate_kbps = 4000u32;

    // Try the Microsoft software MFT first (synchronous).
    let mut enc = MfEncoder::open("h264 encoder mft", width, height, fps, bitrate_kbps)
        .or_else(|e| {
            eprintln!("software MFT failed: {e:#}");
            MfEncoder::open("microsoft avc dx12", width, height, fps, bitrate_kbps)
        })?;
    println!("encoder opened: {}x{} @ {} fps, {} kbps", width, height, fps, bitrate_kbps);

    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut nv12 = vec![0u8; (width as usize) * (height as usize) * 3 / 2];

    let mut total_aus = 0usize;
    let mut first_au_size: Option<usize> = None;
    for frame in 0..60u32 {
        // Draw a moving vertical band so successive frames differ
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                let phase = ((frame as usize + x / 4) % 256) as u8;
                rgba[i] = phase;
                rgba[i + 1] = (255 - phase) as u8;
                rgba[i + 2] = 128;
                rgba[i + 3] = 255;
            }
        }
        rgba_to_nv12(&rgba, width, height, &mut nv12);
        let aus = enc.encode_nv12(&nv12)?;
        for au in &aus {
            if first_au_size.is_none() {
                first_au_size = Some(au.len());
                println!("first AU bytes: {} (first 16 = {:02x?})", au.len(), &au[..au.len().min(16)]);
            }
            total_aus += 1;
        }
    }

    let tail = enc.finish()?;
    total_aus += tail.len();
    println!("total AUs produced: {}", total_aus);
    if first_au_size.is_none() {
        eprintln!("WARNING: no AUs emitted — encoder may be silently buffering");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("encode_test is Windows-only");
    std::process::exit(1);
}
