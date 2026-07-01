//! Windows Graphics Capture (WGC) wrapper.
//!
//! Bridges the windows-capture v2 callback model into our pull-based
//! [`FrameCapture`] trait: the WGC handler thread pushes incoming frames
//! into a shared buffer, and `grab_into` copies from that buffer when the
//! pipeline asks for a frame.

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::sync::OnceLock;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IsIconic,
    IsWindowVisible,
};

static DPI_INIT: OnceLock<()> = OnceLock::new();

/// Make the calling process per-monitor DPI-aware so that
/// `GetClientRect` / `GetWindowRect` return physical pixels (matching
/// the WGC frame buffer). Called from any path that uses Win32 rect
/// queries; safe to invoke multiple times — `SetProcessDpiAwarenessContext`
/// fails after the first call but we ignore the error.
fn ensure_dpi_aware() {
    DPI_INIT.get_or_init(|| unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}
use windows_capture::capture::{Context as WgcContext, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::{ColorOrder, FrameCapture};

#[derive(Default)]
struct Shared {
    latest: Mutex<Option<Vec<u8>>>,
    dimensions: Mutex<Option<(u32, u32)>>,
}

struct WgcHandler {
    shared: Arc<Shared>,
}

impl GraphicsCaptureApiHandler for WgcHandler {
    type Flags = Arc<Shared>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: WgcContext<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { shared: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _ctl: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let w = frame.width();
        let h = frame.height();
        let mut buffer = frame.buffer()?;
        let raw = buffer.as_raw_buffer();
        // Some WGC drivers add stride padding when the actual width isn't
        // 4-byte aligned; in practice for 32-bit RGBA this is rare, but
        // copy carefully just in case.
        let row_bytes = (w as usize) * 4;
        let needed = row_bytes * (h as usize);
        let buf_to_store = if raw.len() == needed {
            raw.to_vec()
        } else {
            // Stride > row_bytes — repack tightly.
            let stride = raw.len() / h as usize;
            let mut out = Vec::with_capacity(needed);
            for y in 0..h as usize {
                let off = y * stride;
                out.extend_from_slice(&raw[off..off + row_bytes]);
            }
            out
        };
        *self.shared.latest.lock() = Some(buf_to_store);
        *self.shared.dimensions.lock() = Some((w, h));
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // The window vanished. Leave latest as-is; grab_into will keep
        // serving the last frame the captor saw.
        Ok(())
    }
}

/// Capture from a specific top-level window.
pub struct WgcCapture {
    shared: Arc<Shared>,
    title: String,
    /// Effective dimensions (cropped to client area, even-clamped) that
    /// `dimensions()` reports to the pipeline.
    width: u32,
    height: u32,
    /// Pixel offset from the captured window's top-left to the start of
    /// the client area. Used to skip the title bar / frame in grab_into.
    crop_x: u32,
    crop_y: u32,
}

impl WgcCapture {
    /// Open a capture session on the window whose title matches. Pass
    /// `contains = true` for substring matching, false for exact.
    pub fn open_by_title(title: &str, contains: bool) -> Result<Self> {
        ensure_dpi_aware();
        let window = if contains {
            Window::from_contains_name(title)
                .map_err(|e| anyhow!("window lookup '{}': {:?}", title, e))?
        } else {
            Window::from_name(title)
                .map_err(|e| anyhow!("window lookup '{}': {:?}", title, e))?
        };
        let actual_title = window
            .title()
            .map_err(|e| anyhow!("Window::title: {:?}", e))?;

        // Look up the same window's HWND via Win32 so we can read the
        // GetClientRect / GetWindowRect pair and crop out the title bar
        // + border. windows-capture's Window doesn't expose its HWND, so
        // we re-resolve from the resolved title.
        let hwnd_for_rect = find_hwnd_by_substring(&actual_title);

        // Refuse to wait on a window WGC can't capture — minimized or
        // hidden windows never deliver frames, so detect those before we
        // hit the timeout below.
        if let Some(hwnd) = hwnd_for_rect {
            let minimized = unsafe { IsIconic(hwnd) }.as_bool();
            let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
            if minimized {
                return Err(anyhow!(
                    "'{}' is minimized — restore the window and try again",
                    actual_title
                ));
            }
            if !visible {
                return Err(anyhow!(
                    "'{}' is hidden — bring the window to the foreground",
                    actual_title
                ));
            }
        }

        let crop = hwnd_for_rect.and_then(|hwnd| unsafe { client_crop(hwnd) });

        let shared = Arc::new(Shared::default());

        // Requesting `WithoutBorder` (hide the yellow capture highlight) needs
        // a recent Windows build; on older ones `start_free_threaded` fails
        // with `BorderConfigUnsupported`. Try the clean setting first, then
        // fall back to `Default` (all settings at system default — no newer
        // WGC config APIs touched), which captures fine everywhere, just
        // possibly with the highlight border. Re-resolve the window each
        // attempt because `Settings::new` consumes it.
        let start_with = |border: DrawBorderSettings| -> Result<()> {
            let window = if contains {
                Window::from_contains_name(title)
            } else {
                Window::from_name(title)
            }
            .map_err(|e| anyhow!("window lookup '{}': {:?}", title, e))?;
            let settings = Settings::new(
                window,
                CursorCaptureSettings::Default,
                border,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Rgba8,
                shared.clone(),
            );
            WgcHandler::start_free_threaded(settings)
                .map(|_| ())
                .map_err(|e| anyhow!("WgcHandler::start_free_threaded: {:?}", e))
        };

        // `window` was only needed for the pre-flight rect/visibility checks
        // above; the capture itself re-resolves it inside `start_with`.
        if let Err(e) = start_with(DrawBorderSettings::WithoutBorder) {
            if format!("{e:?}").contains("Unsupported") {
                eprintln!(
                    "[wgc] '{}': border/config setting unsupported on this Windows \
                     build; retrying with system defaults",
                    actual_title
                );
                start_with(DrawBorderSettings::Default)?;
            } else {
                return Err(e);
            }
        }

        // Wait briefly for the first frame so we know the actual dimensions.
        // WGC reports dimensions on the first delivered frame; without that
        // we'd have to query GetClientRect ourselves.
        // WGC sessions for heavy windows (Chrome, OBS, electron apps)
        // sometimes take a beat to produce the first frame; 8 s is the
        // upper bound we've seen from real captures. Anything past that
        // is almost certainly a permissions / visibility issue, not a
        // slow start.
        let started = Instant::now();
        let timeout = Duration::from_secs(8);
        let (raw_w, raw_h) = loop {
            if let Some(dims) = *shared.dimensions.lock() {
                if dims.0 > 0 && dims.1 > 0 {
                    break dims;
                }
            }
            if started.elapsed() > timeout {
                return Err(anyhow!(
                    "WGC did not produce a frame within {:?} for '{}'. \
                     The window may be on another virtual desktop, occluded, \
                     or doesn't support GraphicsCapture (some games / DRM windows refuse).",
                    timeout,
                    actual_title
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // Decide effective dims. Without a crop we keep the whole window,
        // even-clamped. With a crop we clamp the client rect to the
        // capture-buffer bounds and even-clamp from there.
        let (crop_x, crop_y, width, height) = if let Some((cx, cy, cw, ch)) = crop {
            // Bound the crop rect inside the captured frame so we never
            // index past the buffer if the window has shrunk since open.
            let cx = cx.min(raw_w);
            let cy = cy.min(raw_h);
            let cw = cw.min(raw_w.saturating_sub(cx));
            let ch = ch.min(raw_h.saturating_sub(cy));
            (cx & !1, cy & !1, cw & !1, ch & !1)
        } else {
            eprintln!(
                "[wgc] could not resolve client rect for '{}'; capturing full window",
                actual_title
            );
            (0, 0, raw_w & !1, raw_h & !1)
        };

        if (width, height) != (raw_w, raw_h) || (crop_x, crop_y) != (0, 0) {
            eprintln!(
                "[wgc] '{}' raw {}x{} -> cropped {}x{}+{}+{}",
                actual_title, raw_w, raw_h, width, height, crop_x, crop_y
            );
        }

        Ok(Self {
            shared,
            title: actual_title,
            width,
            height,
            crop_x,
            crop_y,
        })
    }
}

impl FrameCapture for WgcCapture {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn source_name(&self) -> &str {
        &self.title
    }
    fn has_alpha(&self) -> bool {
        // Windows window framebuffers are opaque in practice. The packer
        // sees no_alpha and inserts the chroma-key step when the user has
        // configured one; otherwise the alpha plane is solid 0xFF and the
        // receiver shows the whole rectangle.
        false
    }
    fn color_order(&self) -> ColorOrder {
        ColorOrder::Rgba
    }
    fn grab_into(&mut self, out: &mut [u8]) -> Result<()> {
        let want_row = (self.width as usize) * 4;
        let want_rows = self.height as usize;
        let want = want_row * want_rows;
        if out.len() != want {
            return Err(anyhow!(
                "WGC grab_into: out len {} != expected {}",
                out.len(),
                want
            ));
        }

        let guard = self.shared.latest.lock();
        let Some(buf) = guard.as_ref() else {
            // No frame yet — opaque black so the encoder has data.
            for px in out.chunks_exact_mut(4) {
                px[0] = 0;
                px[1] = 0;
                px[2] = 0;
                px[3] = 255;
            }
            return Ok(());
        };
        let raw_dims = self.shared.dimensions.lock();
        let (raw_w, raw_h) = raw_dims.unwrap_or((self.width, self.height));
        let raw_row = (raw_w as usize) * 4;
        let crop_x = self.crop_x as usize;
        let crop_y = self.crop_y as usize;

        // Each output row is a strip of `want_row` bytes starting at
        // `(crop_y + y, crop_x)` of the raw frame.
        for y in 0..want_rows {
            let src_y = crop_y + y;
            let dst_off = y * want_row;
            if src_y >= raw_h as usize {
                out[dst_off..dst_off + want_row].fill(0);
                continue;
            }
            let row_start = src_y * raw_row;
            let strip_start = row_start + crop_x * 4;
            let strip_end = strip_start + want_row;
            if strip_end <= buf.len() {
                out[dst_off..dst_off + want_row]
                    .copy_from_slice(&buf[strip_start..strip_end]);
            } else if strip_start < buf.len() {
                let n = buf.len() - strip_start;
                out[dst_off..dst_off + n].copy_from_slice(&buf[strip_start..]);
                out[dst_off + n..dst_off + want_row].fill(0);
            } else {
                out[dst_off..dst_off + want_row].fill(0);
            }
        }
        Ok(())
    }
}

// Win32 helpers --------------------------------------------------------

struct EnumState {
    needle: String,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut EnumState) };
    if state.found.is_some() {
        return BOOL(0); // stop enumeration
    }
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len > 0 {
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if copied > 0 {
            let title = String::from_utf16_lossy(&buf[..copied as usize]);
            if title.to_lowercase().contains(&state.needle) {
                state.found = Some(hwnd);
                return BOOL(0);
            }
        }
    }
    BOOL(1)
}

fn find_hwnd_by_substring(needle: &str) -> Option<HWND> {
    let mut state = EnumState {
        needle: needle.to_lowercase(),
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut state as *mut _ as isize));
    }
    state.found
}

/// Return (crop_x, crop_y, client_w, client_h) in *window-relative*
/// pixel coordinates, suitable for slicing the WGC frame buffer.
unsafe fn client_crop(hwnd: HWND) -> Option<(u32, u32, u32, u32)> {
    let mut window_rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut window_rect).ok()? };
    let mut client_rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client_rect).ok()? };
    let mut p = POINT { x: 0, y: 0 };
    unsafe {
        if !ClientToScreen(hwnd, &mut p).as_bool() {
            return None;
        }
    }
    let crop_x = (p.x - window_rect.left).max(0) as u32;
    let crop_y = (p.y - window_rect.top).max(0) as u32;
    let crop_w = (client_rect.right - client_rect.left).max(0) as u32;
    let crop_h = (client_rect.bottom - client_rect.top).max(0) as u32;
    if crop_w == 0 || crop_h == 0 {
        return None;
    }
    Some((crop_x, crop_y, crop_w, crop_h))
}

/// Return a list of (title) pairs for top-level windows that have a
/// non-empty title. Used by the Tauri UI to populate the picker.
pub fn list_windows() -> Result<Vec<String>> {
    ensure_dpi_aware();
    let wnds = Window::enumerate().map_err(|e| anyhow!("Window::enumerate: {:?}", e))?;
    let mut out = Vec::new();
    for w in wnds {
        if let Ok(title) = w.title() {
            if !title.trim().is_empty() {
                out.push(title);
            }
        }
    }
    Ok(out)
}
