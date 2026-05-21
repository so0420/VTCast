//! Media Foundation H.264 encoder (Windows).
//!
//! Phase 2C.2 work item. Status:
//!   * 2C.2.1: MF lifecycle + `enumerate_h264_encoders` ✓
//!   * 2C.2.2: open a sync MFT, configure NV12 input / H.264 output,
//!     encode one frame end-to-end and pull NAL units back out.
//!   * 2C.2.3: continuous encoding + async-MFT support (in progress).

use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::Duration;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup,
    MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_INPUT_STREAM_INFO, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    IMFDXGIDeviceManager, MFCreateDXGIDeviceManager, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, eAVEncH264VProfile_Base,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

static MF_INIT: OnceLock<()> = OnceLock::new();

pub fn ensure_mf_initialized() -> Result<()> {
    MF_INIT.get_or_init(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = MFStartup(MF_VERSION, 0) {
            tracing::error!(error = ?e, "MFStartup failed");
        }
    });
    Ok(())
}

#[derive(Debug, Clone)]
pub struct EncoderDescriptor {
    pub name: String,
    pub flags: u32,
    pub is_hardware: bool,
    pub is_sync: bool,
}

pub fn enumerate_h264_encoders(prefer_hardware: bool) -> Result<Vec<EncoderDescriptor>> {
    ensure_mf_initialized()?;
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut base_flags = MFT_ENUM_FLAG_ALL.0 as u32 | MFT_ENUM_FLAG_SORTANDFILTER.0 as u32;
    if prefer_hardware {
        base_flags |= MFT_ENUM_FLAG_HARDWARE.0 as u32;
    } else {
        base_flags |= MFT_ENUM_FLAG_SYNCMFT.0 as u32;
    }

    let activates: Vec<IMFActivate> = unsafe {
        let mut raw_array: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG(base_flags as i32),
            None,
            Some(&output_type),
            &mut raw_array,
            &mut count,
        )
        .context("MFTEnumEx")?;

        let slice = if raw_array.is_null() || count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(raw_array, count as usize)
        };
        let out: Vec<IMFActivate> = slice.iter().filter_map(|o| o.clone()).collect();
        if !raw_array.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some(raw_array as *const _));
        }
        out
    };

    let mut descriptors = Vec::with_capacity(activates.len());
    for activate in activates {
        let name = unsafe { friendly_name(&activate) }.unwrap_or_else(|| "<unknown>".into());
        let lname = name.to_lowercase();
        let is_hardware = lname.contains("nvidia")
            || lname.contains("nvenc")
            || lname.contains("intel")
            || lname.contains("quick sync")
            || lname.contains("amd")
            || lname.contains("amf")
            || lname.contains("hardware");
        descriptors.push(EncoderDescriptor {
            name,
            flags: base_flags,
            is_hardware,
            is_sync: !is_hardware,
        });
    }

    Ok(descriptors)
}

unsafe fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut buf: [u16; 256] = [0; 256];
    let mut len: u32 = 0;
    let res = unsafe {
        activate.GetString(
            &MFT_FRIENDLY_NAME_Attribute as *const GUID,
            &mut buf,
            Some(&mut len),
        )
    };
    if res.is_err() || len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

// =====================================================================
// 2C.2.2: open a sync MFT, configure types, encode a frame.
// =====================================================================

/// Synchronous H.264 encoder wrapping a Media Foundation Transform.
///
/// Currently targets sync MFTs (typically the Microsoft software encoder
/// or `Microsoft AVC DX12 Encoder`). Async hardware MFTs need the
/// event-generator path; that's the next step.
pub struct MfEncoder {
    transform: IMFTransform,
    /// Pre-allocated input buffer reused per frame. NV12 frame size is
    /// `width * height * 3 / 2`.
    input_buffer_size: usize,
    /// Output buffer size required by the encoder.
    output_buffer_size: usize,
    /// True if the MFT provides its own output sample (we won't allocate one).
    output_provides_samples: bool,
    /// Frame duration in 100-ns units (`10_000_000 / fps`). Used as both
    /// the per-sample duration and the timestamp increment.
    frame_duration_hns: i64,
    next_pts_hns: i64,
}

impl MfEncoder {
    /// Open the encoder MFT whose friendly name contains `name_match`
    /// (case-insensitive substring). Pass `"h264 encoder mft"` to pick the
    /// Microsoft software encoder.
    pub fn open(
        name_match: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        ensure_mf_initialized()?;

        let want = name_match.to_lowercase();
        let candidates = enumerate_all_h264_activates()?;
        let activate = candidates
            .into_iter()
            .find(|(name, _)| name.to_lowercase().contains(&want))
            .ok_or_else(|| anyhow!("no H.264 MFT name matches '{}'", name_match))?
            .1;

        let transform: IMFTransform = unsafe { activate.ActivateObject() }
            .context("IMFActivate::ActivateObject")?;

        let output_type = build_output_type(width, height, fps, bitrate_kbps * 1000)?;
        unsafe { transform.SetOutputType(0, &output_type, 0) }
            .context("SetOutputType — encoder rejected H.264 params")?;

        let input_type = build_input_type_nv12(width, height, fps)?;
        unsafe { transform.SetInputType(0, &input_type, 0) }
            .context("SetInputType — encoder rejected NV12 params")?;

        let mut input_info = MFT_INPUT_STREAM_INFO::default();
        unsafe { transform.GetInputStreamInfo(0, &mut input_info) }
            .context("GetInputStreamInfo")?;
        let output_info: MFT_OUTPUT_STREAM_INFO = unsafe { transform.GetOutputStreamInfo(0) }
            .context("GetOutputStreamInfo")?;

        let nv12_size = (width as usize) * (height as usize) * 3 / 2;
        let input_buffer_size = std::cmp::max(input_info.cbSize as usize, nv12_size);

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }

        Ok(Self {
            transform,
            input_buffer_size,
            output_buffer_size: output_info.cbSize as usize,
            output_provides_samples: (output_info.dwFlags
                & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32)
                != 0,
            frame_duration_hns: (10_000_000i64) / fps as i64,
            next_pts_hns: 0,
        })
    }

    /// Encode one NV12 frame; returns zero or more Annex-B access units.
    /// The MFT may emit no output for the first few frames (it buffers for
    /// rate control). Subsequent calls drain.
    pub fn encode_nv12(&mut self, nv12: &[u8]) -> Result<Vec<Vec<u8>>> {
        let nv12_size = self.input_buffer_size;
        let buf = unsafe { MFCreateMemoryBuffer(nv12_size as u32) }
            .context("MFCreateMemoryBuffer (input)")?;
        unsafe {
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))
                .context("input buffer Lock")?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buf.SetCurrentLength(nv12.len() as u32)
                .context("input buffer SetCurrentLength")?;
            buf.Unlock().context("input buffer Unlock")?;
        }

        let sample: IMFSample = unsafe { MFCreateSample() }.context("MFCreateSample (input)")?;
        unsafe {
            sample.AddBuffer(&buf).context("AddBuffer")?;
            sample
                .SetSampleTime(self.next_pts_hns)
                .context("SetSampleTime")?;
            sample
                .SetSampleDuration(self.frame_duration_hns)
                .context("SetSampleDuration")?;
        }
        self.next_pts_hns += self.frame_duration_hns;

        unsafe {
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")?;
        }

        self.drain_output()
    }

    /// Flush remaining outputs at end of stream.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_END_OF_STREAM")?;
        }
        let mut all = self.drain_output()?;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                .ok();
        }
        // After END_OF_STREAM we may still have pending output samples.
        let tail = self.drain_output().unwrap_or_default();
        all.extend(tail);
        Ok(all)
    }

    fn drain_output(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut aus = Vec::new();
        loop {
            // Allocate an output sample if the MFT doesn't supply one.
            let owned_sample: Option<IMFSample> = if self.output_provides_samples {
                None
            } else {
                let buf = unsafe { MFCreateMemoryBuffer(self.output_buffer_size as u32) }
                    .context("MFCreateMemoryBuffer (output)")?;
                let s = unsafe { MFCreateSample() }.context("MFCreateSample (output)")?;
                unsafe { s.AddBuffer(&buf) }.context("AddBuffer(output)")?;
                Some(s)
            };

            let mut data_buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(owned_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };

            let mut status: u32 = 0;
            let buffers = std::slice::from_mut(&mut data_buffer);
            let hr = unsafe {
                self.transform
                    .ProcessOutput(0, buffers, &mut status)
            };

            match hr {
                Ok(()) => {
                    if let Some(sample) = data_buffer.pSample.as_ref() {
                        let bytes = unsafe { read_sample_bytes(sample) }?;
                        if !bytes.is_empty() {
                            // MF H.264 encoder MFTs emit Annex-B framing
                            // by default — start codes are already present
                            // so we pass the bytes through verbatim.
                            aus.push(bytes);
                        }
                    }
                    // Drop ManuallyDrop contents so refcounts release.
                    unsafe {
                        ManuallyDrop::drop(&mut data_buffer.pSample);
                        ManuallyDrop::drop(&mut data_buffer.pEvents);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    unsafe {
                        ManuallyDrop::drop(&mut data_buffer.pSample);
                        ManuallyDrop::drop(&mut data_buffer.pEvents);
                    }
                    break;
                }
                Err(e) => {
                    unsafe {
                        ManuallyDrop::drop(&mut data_buffer.pSample);
                        ManuallyDrop::drop(&mut data_buffer.pEvents);
                    }
                    return Err(anyhow!("ProcessOutput: {}", e));
                }
            }
        }
        Ok(aus)
    }
}

fn enumerate_all_h264_activates() -> Result<Vec<(String, IMFActivate)>> {
    ensure_mf_initialized()?;
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags =
        (MFT_ENUM_FLAG_ALL.0 | MFT_ENUM_FLAG_SORTANDFILTER.0 | MFT_ENUM_FLAG_SYNCMFT.0) as u32;
    let activates: Vec<IMFActivate> = unsafe {
        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG(flags as i32),
            None,
            Some(&output_type),
            &mut raw,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let slice = if raw.is_null() || count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(raw, count as usize)
        };
        let out: Vec<IMFActivate> = slice.iter().filter_map(|o| o.clone()).collect();
        if !raw.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some(raw as *const _));
        }
        out
    };
    let mut out = Vec::with_capacity(activates.len());
    for a in activates {
        let name = unsafe { friendly_name(&a) }.unwrap_or_else(|| "<unknown>".into());
        out.push((name, a));
    }
    Ok(out)
}

fn build_output_type(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<IMFMediaType> {
    let mt: IMFMediaType = unsafe { MFCreateMediaType() }.context("MFCreateMediaType(out)")?;
    unsafe {
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        mt.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
        mt.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
        set_size(&mt, &MF_MT_FRAME_SIZE, width, height)?;
        set_ratio(&mt, &MF_MT_FRAME_RATE, fps, 1)?;
        set_ratio(&mt, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    }
    Ok(mt)
}

fn build_input_type_nv12(width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    let mt: IMFMediaType = unsafe { MFCreateMediaType() }.context("MFCreateMediaType(in)")?;
    unsafe {
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        set_size(&mt, &MF_MT_FRAME_SIZE, width, height)?;
        set_ratio(&mt, &MF_MT_FRAME_RATE, fps, 1)?;
        set_ratio(&mt, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    }
    Ok(mt)
}

unsafe fn set_size(mt: &IMFMediaType, key: &GUID, w: u32, h: u32) -> windows::core::Result<()> {
    let packed = (w as u64) << 32 | (h as u64);
    unsafe { mt.SetUINT64(key, packed) }
}
unsafe fn set_ratio(mt: &IMFMediaType, key: &GUID, num: u32, den: u32) -> windows::core::Result<()> {
    let packed = (num as u64) << 32 | (den as u64);
    unsafe { mt.SetUINT64(key, packed) }
}

unsafe fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    let buf = unsafe { sample.ConvertToContiguousBuffer() }.context("ConvertToContiguousBuffer")?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut cur_len: u32 = 0;
    unsafe {
        buf.Lock(&mut ptr, None, Some(&mut cur_len))
            .context("output buffer Lock")?;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize).to_vec() };
    unsafe { buf.Unlock().ok() };
    Ok(data)
}

/// Convert a side-by-side packed RGBA buffer (already in our wire format,
/// 2W × H) into NV12 (BT.601 limited range). The output buffer must be
/// `2W * H * 3 / 2` bytes long.
pub fn rgba_to_nv12(rgba: &[u8], width: u32, height: u32, nv12: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    debug_assert_eq!(rgba.len(), w * h * 4);
    debug_assert_eq!(nv12.len(), w * h * 3 / 2);

    let (y_plane, uv_plane) = nv12.split_at_mut(w * h);

    for yy in 0..h {
        for xx in 0..w {
            let i = (yy * w + xx) * 4;
            let r = rgba[i] as f32;
            let g = rgba[i + 1] as f32;
            let b = rgba[i + 2] as f32;
            let yv = 0.257 * r + 0.504 * g + 0.098 * b + 16.0;
            y_plane[yy * w + xx] = yv.clamp(0.0, 255.0) as u8;
        }
    }

    // 2×2 subsample for UV
    for yy in 0..(h / 2) {
        for xx in 0..(w / 2) {
            let mut r = 0f32;
            let mut g = 0f32;
            let mut b = 0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((yy * 2 + dy) * w + (xx * 2 + dx)) * 4;
                    r += rgba[i] as f32;
                    g += rgba[i + 1] as f32;
                    b += rgba[i + 2] as f32;
                }
            }
            r /= 4.0;
            g /= 4.0;
            b /= 4.0;
            let u = -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
            let v = 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
            let idx = (yy * (w / 2) + xx) * 2;
            uv_plane[idx] = u.clamp(0.0, 255.0) as u8;
            uv_plane[idx + 1] = v.clamp(0.0, 255.0) as u8;
        }
    }
}

// =====================================================================
// 2C.2.3 (async-hardware): Asynchronous MFT support.
//
// Hardware encoders (NVIDIA NVENC, Intel Quick Sync, AMD AMF) ship as
// async MFTs: the caller must set MF_TRANSFORM_ASYNC_UNLOCK on the
// transform and then only call ProcessInput / ProcessOutput in response
// to METransformNeedInput / METransformHaveOutput events on the
// transform's IMFMediaEventGenerator.
// =====================================================================

pub struct AsyncMfEncoder {
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    input_buffer_size: usize,
    output_buffer_size: usize,
    output_provides_samples: bool,
    frame_duration_hns: i64,
    next_pts_hns: i64,
    pending_need_input: u32,
    finished: bool,
    /// Held to keep the D3D11 device + DXGI manager alive while the MFT
    /// references them. Some hardware MFTs refuse to run without a
    /// device manager (NVIDIA NVENC specifically returns E_UNEXPECTED on
    /// ActivateObject otherwise).
    #[allow(dead_code)]
    d3d_device: Option<ID3D11Device>,
    #[allow(dead_code)]
    d3d_manager: Option<IMFDXGIDeviceManager>,
}

impl AsyncMfEncoder {
    pub fn open(
        name_match: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        ensure_mf_initialized()?;

        let want = name_match.to_lowercase();
        let candidates = enumerate_all_h264_activates_async()?;
        let (picked_name, activate) = candidates
            .into_iter()
            .find(|(name, _)| name.to_lowercase().contains(&want))
            .ok_or_else(|| anyhow!("no async H.264 MFT name matches '{}'", name_match))?;
        tracing::info!(mft = %picked_name, "opening async MFT");

        // Pick a D3D11 adapter matching the MFT's vendor. NVIDIA's MFT
        // and Intel QSV each want a device on their own GPU; mixing
        // them (NVIDIA D3D into QSV, or vice versa) causes
        // SetOutputType to reject the input format.
        let adapter_hint = if want.contains("nvidia") {
            Some("nvidia")
        } else if want.contains("quick sync") || want.contains("intel") {
            Some("intel")
        } else if want.contains("amf") || want.contains("amd") || want.contains("radeon") {
            Some("amd")
        } else {
            None
        };
        let (d3d_device, d3d_manager) = match create_d3d_manager_for(adapter_hint) {
            Ok((dev, mgr)) => (Some(dev), Some(mgr)),
            Err(e) => {
                tracing::debug!(error = ?e, "no D3D manager; some hardware MFTs may refuse to activate");
                (None, None)
            }
        };

        // NVIDIA's MFT will return E_UNEXPECTED from ActivateObject unless
        // the activate object itself advertises async-unlock support. The
        // attribute is harmless on other activates (Intel QSV ignores it),
        // so we set it unconditionally.
        unsafe {
            let activate_attrs: IMFAttributes = activate.cast()
                .context("IMFActivate -> IMFAttributes cast")?;
            let _ = activate_attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK as *const _, 1);
        }

        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.context("IMFActivate::ActivateObject")?;

        // MF_TRANSFORM_ASYNC_UNLOCK has to be set *before* SET_D3D_MANAGER —
        // Intel QSV otherwise returns 0xC00D6D77 ("caller doesn't support
        // async features of this transform") from the manager message.
        unsafe {
            let attrs: IMFAttributes = transform.GetAttributes().context("GetAttributes")?;
            let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK as *const _, 1);
        }

        // Hand the D3D manager to the MFT before SetInputType / SetOutputType,
        // as MS docs require for hardware MFTs.
        if let Some(mgr) = &d3d_manager {
            let mgr_ptr = mgr.as_raw() as usize;
            if let Err(e) = unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_ptr) } {
                tracing::debug!(error = ?e, "SET_D3D_MANAGER not supported by this MFT");
            } else {
                tracing::debug!("SET_D3D_MANAGER applied");
            }
        }

        let output_type = build_output_type(width, height, fps, bitrate_kbps * 1000)?;
        unsafe { transform.SetOutputType(0, &output_type, 0) }
            .context("SetOutputType (async) — encoder rejected H.264 params")?;

        let input_type = build_input_type_nv12(width, height, fps)?;
        unsafe { transform.SetInputType(0, &input_type, 0) }
            .context("SetInputType (async) — encoder rejected NV12 params")?;

        let mut input_info = MFT_INPUT_STREAM_INFO::default();
        unsafe { transform.GetInputStreamInfo(0, &mut input_info) }
            .context("GetInputStreamInfo")?;
        let output_info: MFT_OUTPUT_STREAM_INFO =
            unsafe { transform.GetOutputStreamInfo(0) }.context("GetOutputStreamInfo")?;

        let nv12_size = (width as usize) * (height as usize) * 3 / 2;
        let input_buffer_size = std::cmp::max(input_info.cbSize as usize, nv12_size);

        let event_gen: IMFMediaEventGenerator = transform
            .cast()
            .context("IMFTransform does not implement IMFMediaEventGenerator")?;

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING (async)")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM (async)")?;
        }

        Ok(Self {
            transform,
            event_gen,
            input_buffer_size,
            output_buffer_size: output_info.cbSize as usize,
            output_provides_samples: (output_info.dwFlags
                & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32)
                != 0,
            frame_duration_hns: (10_000_000i64) / fps as i64,
            next_pts_hns: 0,
            pending_need_input: 0,
            finished: false,
            d3d_device,
            d3d_manager,
        })
    }

    /// Drain any pending MF events for at most `max_iters` iterations.
    /// Returns access units produced. Caller should refill `input_queue`
    /// between calls; on each NeedInput event we'll consume one queued NV12
    /// frame and call ProcessInput.
    pub fn has_pending_requests(&self) -> bool {
        self.pending_need_input > 0
    }

    pub fn pump(
        &mut self,
        input_queue: &mut VecDeque<Vec<u8>>,
        max_iters: u32,
    ) -> Result<Vec<Vec<u8>>> {
        let mut aus = Vec::new();
        for _ in 0..max_iters {
            // First, satisfy any deferred NeedInput requests from the queue.
            while self.pending_need_input > 0 {
                let Some(frame) = input_queue.pop_front() else {
                    break;
                };
                self.submit_input(&frame)?;
                self.pending_need_input -= 1;
            }

            let ev = unsafe { self.event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            match ev {
                Ok(ev) => {
                    let ev_type = unsafe { ev.GetType() }.context("event GetType")?;
                    if ev_type == METransformNeedInput.0 as u32 {
                        if let Some(frame) = input_queue.pop_front() {
                            self.submit_input(&frame)?;
                        } else {
                            self.pending_need_input += 1;
                        }
                    } else if ev_type == METransformHaveOutput.0 as u32 {
                        if let Some(au) = self.drain_one_output()? {
                            aus.push(au);
                        }
                    } else if ev_type == METransformDrainComplete.0 as u32 {
                        self.finished = true;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    return Ok(aus); // no more pending events; caller can refill
                }
                Err(e) => return Err(anyhow!("GetEvent: {}", e)),
            }
        }
        Ok(aus)
    }

    pub fn block_for_event(&mut self, timeout: Duration) -> Result<()> {
        // GetEvent with flag=0 blocks until an event arrives, but there's
        // no per-call timeout. Approximate via a short sleep when polling.
        std::thread::sleep(timeout);
        Ok(())
    }

    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .ok();
        }
        // Drain remaining outputs by pumping events until DrainComplete or
        // a reasonable cap.
        let mut all = Vec::new();
        let mut empty_queue: VecDeque<Vec<u8>> = VecDeque::new();
        for _ in 0..100 {
            let aus = self.pump(&mut empty_queue, 64)?;
            all.extend(aus);
            if self.finished {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                .ok();
        }
        Ok(all)
    }

    fn submit_input(&mut self, nv12: &[u8]) -> Result<()> {
        let buf = unsafe { MFCreateMemoryBuffer(self.input_buffer_size as u32) }
            .context("MFCreateMemoryBuffer (input)")?;
        unsafe {
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buf.SetCurrentLength(nv12.len() as u32)?;
            buf.Unlock()?;
        }
        let sample: IMFSample = unsafe { MFCreateSample() }.context("MFCreateSample (input)")?;
        unsafe {
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(self.next_pts_hns)?;
            sample.SetSampleDuration(self.frame_duration_hns)?;
        }
        self.next_pts_hns += self.frame_duration_hns;
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .context("ProcessInput (async)")?;
        Ok(())
    }

    fn drain_one_output(&mut self) -> Result<Option<Vec<u8>>> {
        let owned_sample = if self.output_provides_samples {
            None
        } else {
            let buf = unsafe { MFCreateMemoryBuffer(self.output_buffer_size as u32) }
                .context("MFCreateMemoryBuffer (output)")?;
            let s = unsafe { MFCreateSample() }.context("MFCreateSample (output)")?;
            unsafe { s.AddBuffer(&buf)?; }
            Some(s)
        };
        let mut data_buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(owned_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status: u32 = 0;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut data_buffer), &mut status)
        };
        let out = match result {
            Ok(()) => {
                let mut au = None;
                if let Some(sample) = data_buffer.pSample.as_ref() {
                    let bytes = unsafe { read_sample_bytes(sample) }?;
                    if !bytes.is_empty() {
                        au = Some(bytes);
                    }
                }
                au
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => None,
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The MFT wants to renegotiate output. Drop whatever's in
                // data_buffer, fetch the type the MFT now offers, apply
                // it, and return None — the next HaveOutput will deliver
                // real bytes.
                unsafe {
                    ManuallyDrop::drop(&mut data_buffer.pSample);
                    ManuallyDrop::drop(&mut data_buffer.pEvents);
                }
                tracing::info!("async MFT signaled stream change, renegotiating output type");
                let new_type: IMFMediaType = unsafe { self.transform.GetOutputAvailableType(0, 0) }
                    .context("GetOutputAvailableType after stream change")?;
                unsafe { self.transform.SetOutputType(0, &new_type, 0) }
                    .context("SetOutputType after stream change")?;
                return Ok(None);
            }
            Err(e) => {
                unsafe {
                    ManuallyDrop::drop(&mut data_buffer.pSample);
                    ManuallyDrop::drop(&mut data_buffer.pEvents);
                }
                return Err(anyhow!("ProcessOutput (async): {}", e));
            }
        };
        unsafe {
            ManuallyDrop::drop(&mut data_buffer.pSample);
            ManuallyDrop::drop(&mut data_buffer.pEvents);
        }
        Ok(out)
    }
}

/// Create a D3D11 device with video support and wrap it in an
/// IMFDXGIDeviceManager for hardware MFTs. If `vendor_hint` is set
/// (e.g. "nvidia" / "intel"), we pin to a matching DXGI adapter so
/// hardware MFTs that route through D3D end up on their own GPU.
fn create_d3d_manager_for(
    vendor_hint: Option<&str>,
) -> Result<(ID3D11Device, IMFDXGIDeviceManager)> {
    let adapter = pick_adapter_for_vendor(vendor_hint);
    if let Some((name, _)) = &adapter {
        tracing::debug!(adapter = %name, ?vendor_hint, "creating D3D11 device on adapter");
    }

    let mut device: Option<ID3D11Device> = None;
    let mut feature_level = D3D_FEATURE_LEVEL_11_0;
    let flags = windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG(
        D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
    );
    let (driver_type, adapter_ref): (_, Option<&IDXGIAdapter>) = match &adapter {
        Some((_, a)) => (D3D_DRIVER_TYPE_UNKNOWN, Some(a)),
        None => (D3D_DRIVER_TYPE_HARDWARE, None),
    };
    unsafe {
        D3D11CreateDevice(
            adapter_ref,
            driver_type,
            HMODULE::default(),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            None,
        )
        .context("D3D11CreateDevice for MF manager")?;
    }
    let device = device.ok_or_else(|| anyhow!("D3D11 device null"))?;

    // MF accesses the D3D device from worker threads, so multithread
    // protection is mandatory.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }

    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe {
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
            .context("MFCreateDXGIDeviceManager")?;
    }
    let manager = manager.ok_or_else(|| anyhow!("DXGI manager null"))?;
    unsafe {
        manager
            .ResetDevice(&device, reset_token)
            .context("IMFDXGIDeviceManager::ResetDevice")?;
    }
    Ok((device, manager))
}

/// Walk DXGI adapters and find one matching `vendor_hint`. With None,
/// return the first non-software adapter. The Microsoft Basic Render
/// Driver always sorts last.
fn pick_adapter_for_vendor(vendor_hint: Option<&str>) -> Option<(String, IDXGIAdapter)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut matched: Option<(String, IDXGIAdapter)> = None;
        let mut first_any: Option<(String, IDXGIAdapter)> = None;
        let mut i = 0u32;
        loop {
            let Ok(a) = factory.EnumAdapters(i) else { break };
            i += 1;
            let Ok(desc) = a.GetDesc() else { continue };
            let len = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            let lname = name.to_lowercase();
            if lname.contains("microsoft basic") {
                continue;
            }
            if first_any.is_none() {
                first_any = Some((name.clone(), a.clone()));
            }
            if let Some(hint) = vendor_hint {
                let h = hint.to_lowercase();
                let vendor_match = match h.as_str() {
                    "nvidia" => lname.contains("nvidia"),
                    "intel" => lname.contains("intel"),
                    "amd" => lname.contains("amd") || lname.contains("radeon"),
                    _ => false,
                };
                if vendor_match && matched.is_none() {
                    matched = Some((name, a));
                }
            }
        }
        matched.or(first_any)
    }
}

fn enumerate_all_h264_activates_async() -> Result<Vec<(String, IMFActivate)>> {
    ensure_mf_initialized()?;
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = (MFT_ENUM_FLAG_HARDWARE.0
        | MFT_ENUM_FLAG_ASYNCMFT.0
        | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
    let activates: Vec<IMFActivate> = unsafe {
        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG(flags as i32),
            None,
            Some(&output_type),
            &mut raw,
            &mut count,
        )
        .context("MFTEnumEx (async hw)")?;
        let slice = if raw.is_null() || count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(raw, count as usize)
        };
        let out: Vec<IMFActivate> = slice.iter().filter_map(|o| o.clone()).collect();
        if !raw.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some(raw as *const _));
        }
        out
    };
    let mut out = Vec::with_capacity(activates.len());
    for a in activates {
        let name = unsafe { friendly_name(&a) }.unwrap_or_else(|| "<unknown>".into());
        out.push((name, a));
    }
    Ok(out)
}

#[allow(dead_code)]
fn _suppress(_p: PCWSTR) {}
