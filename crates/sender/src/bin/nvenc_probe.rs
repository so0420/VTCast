//! Stage-1 diagnostic for the direct-NVENC path: load nvEncodeAPI64.dll,
//! check the driver version handshake, and open an encode session bound to
//! the NVIDIA D3D11 device that owns the running Spout sender's texture.
//!
//! This validates exactly what the Media Foundation NVIDIA MFT failed at
//! (version + DirectX device-type session open) before we build the full
//! encode path on top.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use vtcast_capture::SpoutReceiver;
    use vtcast_sender::nvenc::NvencApi;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let name = vtcast_sender::list_spout_senders()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no Spout sender broadcasting — start Warudo first"))?;
    println!("Spout sender: {name}");

    let (recv, _vendor) = SpoutReceiver::open_shared(&name)?;
    println!("shared device adapter: {}", recv.adapter_name());

    let api = NvencApi::load()?;
    println!("NvencApi loaded; opening session on the capture device…");

    let mut session = api.open_session(recv.device())?;
    println!("✓ NVENC session opened on '{}'", recv.adapter_name());

    // Full path: build the NV12 converter on the same device, configure the
    // encoder, then convert + encode a handful of live frames.
    let (src_w, src_h) = recv.dimensions();
    let src_format = recv.format();
    // Mirror the pipeline's resize cap: packed width must fit NVENC's 4096
    // ceiling, so the content width is capped at 2048 (the shader scales).
    let max_src_w = 2048u32;
    let (eff_w, eff_h) = if src_w <= max_src_w {
        (src_w & !1, src_h & !1)
    } else {
        let s = max_src_w as f64 / src_w as f64;
        (max_src_w & !1, (((src_h as f64 * s) as u32) & !1).max(2))
    };
    let packed_w = eff_w * 2;
    let packed_h = eff_h;
    println!("source {src_w}x{src_h} → packed NV12 dims: {packed_w}x{packed_h}");

    let converter = vtcast_sender::gpu_convert::Nv12Converter::new(
        recv.device(),
        recv.context(),
        recv.shared_texture(),
        src_w,
        src_h,
        src_format,
        packed_w,
        packed_h,
    )?;

    session
        .initialize(packed_w, packed_h, 30, 8000)
        .map_err(|e| anyhow::anyhow!("encoder initialize: {e}"))?;
    println!("✓ encoder initialized; encoding 10 live frames…");

    let mut produced = 0;
    let mut first_au: Option<usize> = None;
    for i in 0..10 {
        let tex = converter.convert()?;
        match session.encode_texture(tex, false)? {
            Some(au) => {
                produced += 1;
                if first_au.is_none() {
                    let n = au.len().min(16);
                    println!(
                        "  frame {i}: AU {} bytes, first {n} = {:02x?}",
                        au.len(),
                        &au[..n]
                    );
                    first_au = Some(au.len());
                }
            }
            None => println!("  frame {i}: buffered (no output)"),
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
    println!("✓ Stage-2 OK: produced {produced}/10 access units via zero-copy NVENC.");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("nvenc_probe is Windows-only");
    std::process::exit(1);
}
