//! CLI utility: enumerate Spout senders, then capture one frame from the
//! first sender as a sanity check. Replaces the standalone poc2_spout PoC.

use anyhow::{anyhow, Result};
use vtcast_capture::{format_name, list_senders, read_sender_info, SpoutReceiver};

fn main() -> Result<()> {
    let senders = list_senders()?;
    println!("Spout senders: {}", senders.len());
    for name in &senders {
        match read_sender_info(name) {
            Ok(info) => println!(
                "  {:<24}  {:>5}x{:<5}  fmt={:<3} ({})  handle=0x{:08x}",
                info.name,
                info.width,
                info.height,
                info.format,
                format_name(info.format),
                info.share_handle,
            ),
            Err(e) => println!("  {:<24}  <info read failed: {:#}>", name, e),
        }
    }
    let Some(first) = senders.first() else {
        return Err(anyhow!("no active senders"));
    };
    println!("\nopening receiver on '{}'…", first);
    let mut rx = SpoutReceiver::open(first)?;
    let (w, h) = rx.dimensions();
    println!(
        "opened: {}x{}  fmt={} ({})  adapter='{}'",
        w,
        h,
        rx.format(),
        format_name(rx.format()),
        rx.adapter_name()
    );

    let frame = rx.grab()?;
    println!("captured {} bytes ({} px)", frame.len(), frame.len() / 4);

    let mut min_a = 255u8;
    let mut max_a = 0u8;
    let mut sum_a: u64 = 0;
    for px in frame.chunks_exact(4) {
        let a = px[3];
        sum_a += a as u64;
        if a < min_a { min_a = a; }
        if a > max_a { max_a = a; }
    }
    let avg = sum_a as f64 / (frame.len() / 4) as f64;
    println!("alpha: min={}  max={}  mean={:.1}", min_a, max_a, avg);
    Ok(())
}
