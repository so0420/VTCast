//! Spout sender-registry protocol over Windows named shared memory.
//!
//! Senders maintain a `SpoutSenderNames` shared mapping (default 10 slots,
//! 256 bytes each, null-terminated names) and per-sender mappings keyed by
//! the sender's name that carry the [`SharedTextureInfo`] struct (DXGI
//! shared-texture handle + dims + format).

use anyhow::{anyhow, Context, Result};
use std::ffi::CString;

use windows::core::PCSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingA, UnmapViewOfFile, FILE_MAP_READ,
};

const SPOUT_SENDER_NAMES: &str = "SpoutSenderNames";
const SPOUT_MAX_NAME_LEN: usize = 256;
const DEFAULT_MAX_SENDERS: usize = 10;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SharedTextureInfoRaw {
    share_handle: u32,
    width: u32,
    height: u32,
    format: u32,
    usage: u32,
    description: [u8; 256],
    partner_id: u32,
}

/// Public view of a sender's texture metadata.
#[derive(Debug, Clone)]
pub struct SenderInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// DXGI_FORMAT value (e.g. 28 = R8G8B8A8_UNORM, 87 = B8G8R8A8_UNORM).
    pub format: u32,
    /// DXGI shared handle as the low 32 bits (Spout stores it as u32 for
    /// 32/64-bit interop). Pass through to `OpenSharedResource`.
    pub share_handle: u32,
}

pub fn list_senders() -> Result<Vec<String>> {
    let cname = CString::new(SPOUT_SENDER_NAMES)?;
    unsafe {
        let map = OpenFileMappingA(
            FILE_MAP_READ.0,
            false,
            PCSTR(cname.as_ptr() as *const u8),
        )
        .context(
            "OpenFileMappingA(\"SpoutSenderNames\") failed — no Spout senders \
             broadcasting?",
        )?;
        let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
        if view.Value.is_null() {
            let _ = CloseHandle(map);
            return Err(anyhow!("MapViewOfFile(SpoutSenderNames) returned null"));
        }
        let total = DEFAULT_MAX_SENDERS * SPOUT_MAX_NAME_LEN;
        let bytes = std::slice::from_raw_parts(view.Value as *const u8, total);
        let mut out = Vec::new();
        for slot in bytes.chunks(SPOUT_MAX_NAME_LEN) {
            let len = slot.iter().position(|&b| b == 0).unwrap_or(0);
            if len > 0 {
                if let Ok(s) = std::str::from_utf8(&slot[..len]) {
                    out.push(s.to_string());
                }
            }
        }
        let _ = UnmapViewOfFile(view);
        let _ = CloseHandle(map);
        Ok(out)
    }
}

pub fn read_sender_info(name: &str) -> Result<SenderInfo> {
    let cname = CString::new(name)?;
    unsafe {
        let map = OpenFileMappingA(
            FILE_MAP_READ.0,
            false,
            PCSTR(cname.as_ptr() as *const u8),
        )
        .with_context(|| format!("OpenFileMappingA(\"{}\")", name))?;
        let size = std::mem::size_of::<SharedTextureInfoRaw>();
        let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, size);
        if view.Value.is_null() {
            let _ = CloseHandle(map);
            return Err(anyhow!("MapViewOfFile({}) returned null", name));
        }
        let raw = *(view.Value as *const SharedTextureInfoRaw);
        let _ = UnmapViewOfFile(view);
        let _ = CloseHandle(map);
        Ok(SenderInfo {
            name: name.to_string(),
            width: raw.width,
            height: raw.height,
            format: raw.format,
            share_handle: raw.share_handle,
        })
    }
}
