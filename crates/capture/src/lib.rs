//! vtcast-capture — Spout/WGC/DDA receiver for VTCast, pure Rust (no Spout SDK).
//!
//! Public API:
//!   * [`list_senders`] / [`read_sender_info`] — enumerate active Spout senders
//!     and inspect them without opening a D3D11 device.
//!   * [`SpoutReceiver`] — open a sender, probe DXGI adapters until one can
//!     open its shared texture, then grab RGBA frames on demand.
//!
//! Production decisions baked in (see project memory `phase0-findings`):
//!   * DXGI multi-adapter probing is required — Optimus laptops route Warudo
//!     to the dGPU, the iGPU device cannot open its shared handle.
//!   * Falls back from `OpenSharedResource` (KMT) to `OpenSharedResource1`
//!     (NT) automatically. Warudo uses the KMT path in practice.
//!   * RGBA output is straight (not premultiplied) for B8G8R8A8/R8G8B8A8
//!     formats. Warudo emits essentially binary alpha in practice.

mod d3d11;
mod protocol;
#[cfg(windows)]
pub mod dda;
#[cfg(windows)]
pub mod wgc;

pub use protocol::{list_senders, read_sender_info, SenderInfo};

/// Whether the bytes a capture returns are R,G,B,A or B,G,R,A. The packer
/// normalises before encoding, so callers don't need to swap themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorOrder {
    Rgba,
    Bgra,
}

/// A pull-based frame source. The pipeline drives this from a blocking
/// thread, calling `grab_into` once per frame interval.
pub trait FrameCapture: Send {
    fn dimensions(&self) -> (u32, u32);
    /// Display name of the source — used in logs + the UI status line.
    fn source_name(&self) -> &str;
    /// Whether the captured pixels carry meaningful alpha. WGC / DDA
    /// return `false` so the packer can decide whether to insert a
    /// chroma-key step or leave the alpha plane as a solid 0xFF.
    fn has_alpha(&self) -> bool;
    fn color_order(&self) -> ColorOrder;
    /// `out.len()` must equal `width * height * 4`.
    fn grab_into(&mut self, out: &mut [u8]) -> Result<()>;
}

impl FrameCapture for SpoutReceiver {
    fn dimensions(&self) -> (u32, u32) {
        SpoutReceiver::dimensions(self)
    }
    fn source_name(&self) -> &str {
        SpoutReceiver::sender_name(self)
    }
    fn has_alpha(&self) -> bool {
        true
    }
    fn color_order(&self) -> ColorOrder {
        // grab_into already swaps BGRA->RGBA when the source format is
        // B8G8R8A8 — see SpoutReceiver::grab_into.
        ColorOrder::Rgba
    }
    fn grab_into(&mut self, out: &mut [u8]) -> Result<()> {
        SpoutReceiver::grab_into(self, out)
    }
}

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN,
};

/// An open receiver pinned to a specific sender. Owns the D3D11 device that
/// could open the shared handle, plus the immediate context used for copy +
/// map.
pub struct SpoutReceiver {
    sender_name: String,
    info: SenderInfo,
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    shared_tex: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    adapter_name: String,
}

impl SpoutReceiver {
    /// Open a receiver for the named sender. Returns an error if the sender
    /// is not currently broadcasting or no DXGI adapter can open its handle.
    pub fn open(sender_name: &str) -> Result<Self> {
        let info = read_sender_info(sender_name)?;
        Self::open_with_info(sender_name, info)
    }

    fn open_with_info(sender_name: &str, info: SenderInfo) -> Result<Self> {
        let raw_handle = info.share_handle as usize as *mut std::ffi::c_void;
        let handle = HANDLE(raw_handle);

        let adapters = d3d11::enumerate_adapters()?;
        let mut last_err: Option<anyhow::Error> = None;
        for (adapter_name, adapter) in adapters {
            let (device, context) = match d3d11::create_d3d11_on(Some(&adapter)) {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match Self::try_open_shared(&device, handle) {
                Ok(shared_tex) => {
                    let mut resolved = info.clone();
                    let staging =
                        match Self::staging_from_shared(&device, &shared_tex, &mut resolved) {
                            Ok(s) => s,
                            Err(e) => {
                                last_err = Some(e);
                                continue;
                            }
                        };
                    return Ok(Self {
                        sender_name: sender_name.to_string(),
                        info: resolved,
                        _device: device,
                        context,
                        shared_tex,
                        staging,
                        adapter_name,
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow!("no DXGI adapters available"))
            .context(format!(
                "no adapter could open shared handle 0x{:08x} for sender '{}'",
                info.share_handle, sender_name
            )))
    }

    /// Open the sender on a Media-Foundation-capable *shared* device — the
    /// device the GPU zero-copy pipeline reuses for the NV12 conversion
    /// shader and the encoder MFT.
    ///
    /// Like [`open`] this probes DXGI adapters until one can open the shared
    /// handle, but each candidate device is created via
    /// [`d3d11::create_shared_device`] (BGRA + video support + multithread
    /// protected). Returns the receiver plus the lowercased adapter name so
    /// the caller can pick the matching hardware encoder MFT (the encoder has
    /// to live on the same GPU as the texture).
    pub fn open_shared(sender_name: &str) -> Result<(Self, String)> {
        let info = read_sender_info(sender_name)?;
        let raw_handle = info.share_handle as usize as *mut std::ffi::c_void;
        let handle = HANDLE(raw_handle);

        let adapters = d3d11::enumerate_adapters()?;
        let mut last_err: Option<anyhow::Error> = None;
        for (adapter_name, adapter) in adapters {
            let (device, context) = match d3d11::create_shared_device(Some(&adapter)) {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match Self::try_open_shared(&device, handle) {
                Ok(shared_tex) => {
                    let mut resolved = info.clone();
                    let staging =
                        match Self::staging_from_shared(&device, &shared_tex, &mut resolved) {
                            Ok(s) => s,
                            Err(e) => {
                                last_err = Some(e);
                                continue;
                            }
                        };
                    let lname = adapter_name.to_lowercase();
                    return Ok((
                        Self {
                            sender_name: sender_name.to_string(),
                            info: resolved,
                            _device: device,
                            context,
                            shared_tex,
                            staging,
                            adapter_name,
                        },
                        lname,
                    ));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow!("no DXGI adapters available"))
            .context(format!(
                "no shared-capable adapter could open handle 0x{:08x} for sender '{}'",
                info.share_handle, sender_name
            )))
    }

    /// The D3D11 device this receiver's textures live on. The GPU pipeline
    /// shares this with the conversion shader and the encoder.
    pub fn device(&self) -> &ID3D11Device {
        &self._device
    }

    /// The immediate context paired with [`device`]. Single-threaded use
    /// only; the device itself is multithread-protected for MF.
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// The opened Spout shared texture (source RGBA/BGRA). Sample this from a
    /// shader via an SRV, or snapshot it with `CopyResource` first to avoid
    /// tearing against the sender's writes.
    pub fn shared_texture(&self) -> &ID3D11Texture2D {
        &self.shared_tex
    }

    fn try_open_shared(
        device: &ID3D11Device,
        handle: HANDLE,
    ) -> Result<ID3D11Texture2D> {
        unsafe {
            let mut t: Option<ID3D11Texture2D> = None;
            match device.OpenSharedResource::<ID3D11Texture2D>(handle, &mut t) {
                Ok(()) => t.ok_or_else(|| anyhow!("KMT shared texture null")),
                Err(_) => {
                    let device1: ID3D11Device1 = device
                        .cast()
                        .context("device does not implement ID3D11Device1")?;
                    let t1: ID3D11Texture2D = device1
                        .OpenSharedResource1(handle)
                        .context("OpenSharedResource1 (NT) failed")?;
                    Ok(t1)
                }
            }
        }
    }

    /// Build the CPU-readback staging texture from the *opened* shared
    /// texture's authoritative desc, updating `info` to match.
    ///
    /// Spout senders frequently write a bogus or zero (`DXGI_FORMAT_UNKNOWN`)
    /// format — and sometimes stale dims — into their shared-memory metadata.
    /// Trusting that makes `CreateTexture2D(staging)` fail with E_INVALIDARG
    /// (0x80070057), which takes down the whole pipeline (the GPU zero-copy
    /// path doesn't even use this texture, but it was created eagerly). The
    /// opened texture's own `GetDesc` is ground truth, so we resolve dims +
    /// format from it and normalise to a staging-safe format.
    fn staging_from_shared(
        device: &ID3D11Device,
        shared_tex: &ID3D11Texture2D,
        info: &mut SenderInfo,
    ) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { shared_tex.GetDesc(&mut desc) };
        // Prefer the real texture's dims/format; keep the reported metadata
        // only if the desc came back empty (shouldn't happen for a texture we
        // just opened, but stay defensive).
        if desc.Width != 0 && desc.Height != 0 {
            info.width = desc.Width;
            info.height = desc.Height;
        }
        let staging_fmt = staging_safe_format(desc.Format);
        // Report the concrete, known format downstream so the BGRA->RGBA swap
        // and the GPU converter's format matching pick the right path.
        info.format = staging_fmt.0 as u32;
        d3d11::create_staging_texture(device, info.width, info.height, staging_fmt)
    }

    pub fn sender_name(&self) -> &str {
        &self.sender_name
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }

    pub fn format(&self) -> u32 {
        self.info.format
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Required buffer size for a frame in bytes (width * height * 4).
    pub fn frame_bytes(&self) -> usize {
        (self.info.width as usize) * (self.info.height as usize) * 4
    }

    /// Copy the current sender frame into the staging texture and read it
    /// into `out`. `out` must be exactly `frame_bytes()` long. Output is
    /// RGBA bytes regardless of whether the source format was BGRA-typed.
    pub fn grab_into(&mut self, out: &mut [u8]) -> Result<()> {
        if out.len() != self.frame_bytes() {
            return Err(anyhow!(
                "buffer size mismatch: got {} bytes, need {}",
                out.len(),
                self.frame_bytes()
            ));
        }
        unsafe {
            let src: ID3D11Resource = self.shared_tex.cast()?;
            let dst: ID3D11Resource = self.staging.cast()?;
            self.context.CopyResource(&dst, &src);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map staging")?;

            let w = self.info.width as usize;
            let h = self.info.height as usize;
            let row_bytes = w * 4;
            let src_pitch = mapped.RowPitch as usize;
            let src_ptr = mapped.pData as *const u8;
            for y in 0..h {
                let src_row = src_ptr.add(y * src_pitch);
                let dst_row = out.as_mut_ptr().add(y * row_bytes);
                std::ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
            }
            self.context.Unmap(&dst, 0);
        }

        // BGRA → RGBA swap if the source format is BGRA-typed.
        let fmt = DXGI_FORMAT(self.info.format as i32);
        if fmt == DXGI_FORMAT_B8G8R8A8_UNORM || self.info.format == 91 {
            for px in out.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        } else if fmt != DXGI_FORMAT_R8G8B8A8_UNORM && self.info.format != 29 {
            tracing_unexpected_format(self.info.format);
        }
        Ok(())
    }

    /// Convenience wrapper around [`grab_into`] that allocates the buffer.
    pub fn grab(&mut self) -> Result<Vec<u8>> {
        let mut out = vec![0u8; self.frame_bytes()];
        self.grab_into(&mut out)?;
        Ok(out)
    }
}

// Silently swallow unexpected formats here — log helpers depend on tracing
// which we don't pull in for the library to keep deps minimal. Callers can
// inspect format() upfront and decide.
fn tracing_unexpected_format(_f: u32) {}

/// Map a shared texture's real format to one valid for a CPU-readable staging
/// copy. Typeless formats become their UNORM sibling (so `Map` returns sane
/// bytes and `CopyResource` stays in-family); `UNKNOWN` — which many Spout
/// senders report when they leave the field unset — becomes BGRA8_UNORM, the
/// Spout default. Everything else passes through unchanged.
fn staging_safe_format(fmt: DXGI_FORMAT) -> DXGI_FORMAT {
    match fmt {
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_UNKNOWN => DXGI_FORMAT_B8G8R8A8_UNORM,
        other => other,
    }
}

/// Name of a DXGI_FORMAT value for diagnostics.
pub fn format_name(f: u32) -> &'static str {
    match f {
        0 => "UNKNOWN",
        10 => "R16G16B16A16_FLOAT",
        24 => "R10G10B10A2_UNORM",
        28 => "R8G8B8A8_UNORM",
        29 => "R8G8B8A8_UNORM_SRGB",
        87 => "B8G8R8A8_UNORM",
        91 => "B8G8R8A8_UNORM_SRGB",
        _ => "(other)",
    }
}

