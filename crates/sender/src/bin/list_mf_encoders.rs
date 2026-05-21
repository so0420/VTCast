//! Diagnostic: enumerate Media Foundation H.264 encoder MFTs available on
//! this system. Useful for confirming hardware-accelerated paths before
//! the encoder pipeline tries to open one.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use vtcast_sender::mf_encoder;

    let hw = mf_encoder::enumerate_h264_encoders(true)?;
    println!("Hardware MFTs ({}):", hw.len());
    if hw.is_empty() {
        println!("  (none)");
    }
    for d in &hw {
        println!(
            "  - {:<60}  is_hardware={}  is_sync={}",
            d.name, d.is_hardware, d.is_sync
        );
    }

    let sw = mf_encoder::enumerate_h264_encoders(false)?;
    println!("\nSync (software-leaning) MFTs ({}):", sw.len());
    if sw.is_empty() {
        println!("  (none)");
    }
    for d in &sw {
        println!(
            "  - {:<60}  is_hardware={}  is_sync={}",
            d.name, d.is_hardware, d.is_sync
        );
    }

    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("list_mf_encoders is Windows-only");
    std::process::exit(1);
}
