//! Diagnostic: enumerate capturable windows and exercise the WGC
//! capture path. Run with no arg to just list windows; pass a title
//! substring to capture that window for ~1 second.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use vtcast_capture::wgc::{list_windows, WgcCapture};
    use vtcast_capture::FrameCapture;

    println!("=== capturable windows ===");
    let wnds = list_windows()?;
    for w in &wnds {
        println!("  - {}", w);
    }
    println!("({} total)\n", wnds.len());

    let Some(target) = std::env::args().nth(1) else {
        println!("(pass a window title substring as argv[1] to capture)");
        return Ok(());
    };

    println!("opening WGC capture for '{}'...", target);
    let mut cap = WgcCapture::open_by_title(&target, true)?;
    let (w, h) = cap.dimensions();
    println!("opened: '{}' at {}x{}", cap.source_name(), w, h);

    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut total_changes = 0u32;
    let mut prev_first: [u8; 4] = [0, 0, 0, 0];

    for i in 0..30u32 {
        cap.grab_into(&mut buf)?;
        if i == 0 {
            println!("first frame: TL pixel = {:?}", &buf[0..4]);
            let mid = (buf.len() / 2) & !3;
            println!("first frame: mid pixel = {:?}", &buf[mid..mid + 4]);
        }
        if buf[0..4] != prev_first {
            total_changes += 1;
            prev_first = [buf[0], buf[1], buf[2], buf[3]];
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
    println!("captured 30 frames; TL pixel changed {} times", total_changes);
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("test_wgc is Windows-only");
    std::process::exit(1);
}
