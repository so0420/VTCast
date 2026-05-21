//! Desktop Duplication API (DDA) capture for full-monitor frames.
//!
//! Synchronous unlike WGC — we call `acquire_next_frame` on each
//! `grab_into`, with a short timeout that matches our frame interval.

use anyhow::{anyhow, Result};
use windows_capture::dxgi_duplication_api::DxgiDuplicationApi;
use windows_capture::monitor::Monitor;

use crate::{ColorOrder, FrameCapture};

pub struct DdaCapture {
    dup: DxgiDuplicationApi,
    width: u32,
    height: u32,
    name: String,
    scratch: Vec<u8>,
}

impl DdaCapture {
    /// Open the monitor at `index` (0 = primary, 1 = secondary, …).
    /// Pass `None` for the primary.
    pub fn open(index: Option<usize>) -> Result<Self> {
        let monitor = match index {
            Some(i) => Monitor::from_index(i)
                .map_err(|e| anyhow!("Monitor::from_index({}): {:?}", i, e))?,
            None => Monitor::primary().map_err(|e| anyhow!("Monitor::primary: {:?}", e))?,
        };

        let raw_w = monitor
            .width()
            .map_err(|e| anyhow!("Monitor::width: {:?}", e))?;
        let raw_h = monitor
            .height()
            .map_err(|e| anyhow!("Monitor::height: {:?}", e))?;
        let name = monitor.name().unwrap_or_else(|_| "Display".to_string());

        let dup = DxgiDuplicationApi::new(monitor)
            .map_err(|e| anyhow!("DxgiDuplicationApi::new: {:?}", e))?;

        // Trim to even for encoder compatibility (DDA monitors are usually
        // already even, but be defensive).
        let width = raw_w & !1;
        let height = raw_h & !1;

        Ok(Self {
            dup,
            width,
            height,
            name,
            scratch: Vec::with_capacity((width as usize) * (height as usize) * 4),
        })
    }
}

impl FrameCapture for DdaCapture {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn source_name(&self) -> &str {
        &self.name
    }
    fn has_alpha(&self) -> bool {
        false
    }
    fn color_order(&self) -> ColorOrder {
        // We swap BGRA -> RGBA internally before returning so the rest of
        // the pipeline can assume RGBA regardless of source.
        ColorOrder::Rgba
    }
    fn grab_into(&mut self, out: &mut [u8]) -> Result<()> {
        let want_row = (self.width as usize) * 4;
        let rows = self.height as usize;
        if out.len() != want_row * rows {
            return Err(anyhow!(
                "DDA grab_into: out len {} != expected {}",
                out.len(),
                want_row * rows
            ));
        }

        // Short timeout — if the desktop hasn't changed, DDA returns
        // a stale or empty frame, but acquire_next_frame still succeeds.
        let mut frame = match self.dup.acquire_next_frame(33) {
            Ok(f) => f,
            Err(e) => {
                // Most common path here is "no new frame in time" — fill the
                // output with black rather than failing the whole pipeline.
                tracing_disabled_dbg(&format!("acquire_next_frame: {:?}", e));
                for px in out.chunks_exact_mut(4) {
                    px[0] = 0;
                    px[1] = 0;
                    px[2] = 0;
                    px[3] = 255;
                }
                return Ok(());
            }
        };

        let buffer = frame
            .buffer()
            .map_err(|e| anyhow!("DxgiDuplicationFrame::buffer: {:?}", e))?;
        self.scratch.clear();
        let bytes = buffer.as_nopadding_buffer(&mut self.scratch);

        // Copy rows accounting for possible width mismatch from trimming.
        let src_row = bytes.len() / rows.max(1);
        let copy_row = src_row.min(want_row);
        for y in 0..rows {
            let src_off = y * src_row;
            let dst_off = y * want_row;
            if src_off + copy_row <= bytes.len() {
                out[dst_off..dst_off + copy_row]
                    .copy_from_slice(&bytes[src_off..src_off + copy_row]);
            }
            if copy_row < want_row {
                out[dst_off + copy_row..dst_off + want_row].fill(0);
            }
        }

        // DDA returns BGRA; swap R/B in-place so the rest of the pipeline
        // can treat all captures as RGBA.
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Ok(())
    }
}

// We don't pull tracing into vtcast-capture; use eprintln + a no-op when
// running under tests to keep stderr tidy.
fn tracing_disabled_dbg(_msg: &str) {
    // intentionally empty in release; flip to eprintln! for verbose runs
}

/// List monitor indices + names for the UI picker.
pub fn list_displays() -> Result<Vec<(usize, String)>> {
    let monitors = Monitor::enumerate().map_err(|e| anyhow!("Monitor::enumerate: {:?}", e))?;
    let mut out = Vec::with_capacity(monitors.len());
    for m in monitors {
        let idx = m.index().unwrap_or(usize::MAX);
        let name = m.name().unwrap_or_else(|_| format!("Display {}", idx));
        out.push((idx, name));
    }
    Ok(out)
}
