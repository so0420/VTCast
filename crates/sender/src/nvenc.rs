//! Direct NVENC (NVIDIA Video Codec SDK) encoder with D3D11 texture input.
//!
//! NVIDIA's Media Foundation H.264 MFT is deprecated and fails to activate on
//! current drivers (E_UNEXPECTED), so the MF zero-copy path can't reach NVENC.
//! ffmpeg's `h264_nvenc` works because it calls the NVENC SDK
//! (`nvEncodeAPI`) directly. This module does the same, in-process, accepting
//! a D3D11 NV12 texture as input so a frame is encoded straight from VRAM —
//! true zero-copy on NVIDIA.
//!
//! Bindings are hand-written against `nvEncodeAPI.h` (NVENCAPI v13.0). The DLL
//! (`nvEncodeAPI64.dll`) ships with the NVIDIA driver and is loaded at runtime
//! via `LoadLibrary` + the two exported entry points, so there's no import-lib
//! or SDK-install build dependency.
//!
//! Only the H.264 + DirectX subset is bound. Structs that the driver reads or
//! writes are declared with their exact reserved arrays so the byte layout
//! matches the header regardless of which fields we set.
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;
use std::os::raw::c_char;

// ── Version macros (nvEncodeAPI.h) ──────────────────────────────────────
// NVENCAPI_VERSION = MAJOR | (MINOR<<24); STRUCT_VERSION(v) =
// NVENCAPI_VERSION | (v<<16) | (0x7<<28).
const NVENCAPI_MAJOR_VERSION: u32 = 13;
const NVENCAPI_MINOR_VERSION: u32 = 0;
const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
const fn struct_version(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}

const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);

// ── Basic types ─────────────────────────────────────────────────────────
pub type NVENCSTATUS = i32;
pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    Guid {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    }
}

pub const NV_ENC_CODEC_H264_GUID: Guid = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub const NV_ENC_H264_PROFILE_BASELINE_GUID: Guid = guid(
    0x727bcaa,
    0x78c4,
    0x4c83,
    [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
);
pub const NV_ENC_H264_PROFILE_HIGH_GUID: Guid = guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
// P1 = fastest preset (lowest latency, highest perf) — matches our ffmpeg p1.
pub const NV_ENC_PRESET_P1_GUID: Guid = guid(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0xe, 0xbf],
);
// P4 = "medium" preset — markedly better quality than P1, still trivially
// real-time for our frame size on any NVENC-capable GPU.
pub const NV_ENC_PRESET_P4_GUID: Guid = guid(
    0x90a7b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);

// NV_ENC_DEVICE_TYPE
pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0x0;
// NV_ENC_INPUT_RESOURCE_TYPE
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0x0;
// NV_ENC_BUFFER_FORMAT
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x00000001;
// NV_ENC_TUNING_INFO — low latency (allows a real VBV buffer; still no
// B-frames). ULTRA(3) clamps the VBV to ~1 frame and hurts quality.
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;

// ── Session open params ─────────────────────────────────────────────────
#[repr(C)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

impl Default for NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    fn default() -> Self {
        // SAFETY: all-zero is a valid representation; we set the fields that matter.
        unsafe { std::mem::zeroed() }
    }
}

// ── Stage-2: config / init / encode structs ────────────────────────────
const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);
const NV_ENC_CONFIG_VER: u32 = struct_version(9) | (1u32 << 31);
const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | (1u32 << 31);
const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1u32 << 31);
const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | (1u32 << 31);
const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1u32 << 31);

const NV_ENC_PARAMS_RC_CBR: u32 = 0x2;
const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x01;
const NV_ENC_INPUT_IMAGE: u32 = 0x0;
const NV_ENC_ERR_NEED_MORE_INPUT: NVENCSTATUS = 15;
// NV_ENC_PIC_FLAGS: force the current frame to be an IDR + emit SPS/PPS with
// it, so a receiver whose decoder is stuck (packet loss) can re-sync on this
// exact frame instead of waiting out the GOP.
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
// NV_ENC_CONFIG_H264 bitfield positions: outputAUD = bit 6, repeatSPSPPS = bit 12.
const H264_FLAG_OUTPUT_AUD: u32 = 1 << 6;
const H264_FLAG_REPEAT_SPSPPS: u32 = 1 << 12;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvEncQp {
    qpInterP: u32,
    qpInterB: u32,
    qpIntra: u32,
}

#[repr(C)]
struct NvEncRcParams {
    version: u32,
    rateControlMode: u32,
    constQP: NvEncQp,
    averageBitRate: u32,
    maxBitRate: u32,
    vbvBufferSize: u32,
    vbvInitialDelay: u32,
    flags: u32, // packed bitfields (enableMinQP..reservedBitFields); 32 bits total
    minQP: NvEncQp,
    maxQP: NvEncQp,
    initialRCQP: NvEncQp,
    temporallayerIdxMask: u32,
    temporalLayerQP: [u8; 8],
    targetQuality: u8,
    targetQualityLSB: u8,
    lookaheadDepth: u16,
    lowDelayKeyFrameScale: u8,
    yDcQPIndexOffset: i8,
    uDcQPIndexOffset: i8,
    vDcQPIndexOffset: i8,
    qpMapMode: u32,
    multiPass: u32,
    alphaLayerBitrateRatio: u32,
    cbQPIndexOffset: i8,
    crQPIndexOffset: i8,
    reserved2: u16,
    lookaheadLevel: u32,
    viewBitrateRatios: [u8; 7],
    reserved3: u8,
    reserved1: u32,
}

#[repr(C)]
struct NvEncConfigH264 {
    flags: u32, // packed bitfields (outputAUD = bit6, repeatSPSPPS = bit12, ...)
    level: u32,
    idrPeriod: u32,
    separateColourPlaneFlag: u32,
    disableDeblockingFilterIDC: u32,
    numTemporalLayers: u32,
    spsId: u32,
    ppsId: u32,
    adaptiveTransformMode: u32,
    fmoMode: u32,
    bdirectMode: u32,
    entropyCodingMode: u32,
    stereoMode: u32,
    intraRefreshPeriod: u32,
    intraRefreshCnt: u32,
    maxNumRefFrames: u32,
    sliceMode: u32,
    sliceModeData: u32,
    h264VUIParameters: [u32; 28], // NV_ENC_CONFIG_H264_VUI_PARAMETERS (all u32, 112 bytes)
    ltrNumFrames: u32,
    ltrTrustMode: u32,
    chromaFormatIDC: u32,
    maxTemporalLayers: u32,
    useBFramesAsRef: u32,
    numRefL0: u32,
    numRefL1: u32,
    outputBitDepth: u32,
    inputBitDepth: u32,
    tfLevel: u32,
    reserved1: [u32; 264],
    reserved2: [*mut c_void; 64],
}

// The codec-config union is sized by its largest member; H264 (with its
// reserved1[264]) is the largest, so the union == NvEncConfigH264.
#[repr(C)]
struct NvEncConfig {
    version: u32,
    profileGUID: Guid,
    gopLength: u32,
    frameIntervalP: i32,
    monoChromeEncoding: u32,
    frameFieldMode: u32,
    mvPrecision: u32,
    rcParams: NvEncRcParams,
    encodeCodecConfig: NvEncConfigH264,
    reserved: [u32; 278],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
struct NvEncPresetConfig {
    version: u32,
    reserved: u32,
    presetCfg: NvEncConfig,
    reserved1: [u32; 256],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
struct NvEncInitializeParams {
    version: u32,
    encodeGUID: Guid,
    presetGUID: Guid,
    encodeWidth: u32,
    encodeHeight: u32,
    darWidth: u32,
    darHeight: u32,
    frameRateNum: u32,
    frameRateDen: u32,
    enableEncodeAsync: u32,
    enablePTD: u32,
    flags: u32, // packed bitfields (reportSliceOffsets..reservedBitFields)
    privDataSize: u32,
    reserved: u32,
    privData: *mut c_void,
    encodeConfig: *mut NvEncConfig,
    maxEncodeWidth: u32,
    maxEncodeHeight: u32,
    maxMEHintCountsPerBlock: [u32; 8], // 2 × hint-counts struct (4 u32 each)
    tuningInfo: u32,
    bufferFormat: u32,
    numStateBuffers: u32,
    outputStatsLevel: u32,
    reserved1: [u32; 284],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
struct NvEncRegisterResource {
    version: u32,
    resourceType: u32,
    width: u32,
    height: u32,
    pitch: u32,
    subResourceIndex: u32,
    resourceToRegister: *mut c_void,
    registeredResource: *mut c_void, // out
    bufferFormat: u32,
    bufferUsage: u32,
    pInputFencePoint: *mut c_void,
    chromaOffset: [u32; 2],
    chromaOffsetIn: [u32; 2],
    reserved1: [u32; 244],
    reserved2: [*mut c_void; 61],
}

#[repr(C)]
struct NvEncMapInputResource {
    version: u32,
    subResourceIndex: u32,
    inputResource: *mut c_void,
    registeredResource: *mut c_void,
    mappedResource: *mut c_void, // out
    mappedBufferFmt: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 63],
}

#[repr(C)]
struct NvEncCreateBitstreamBuffer {
    version: u32,
    size: u32,
    memoryHeap: u32,
    reserved: u32,
    bitstreamBuffer: *mut c_void, // out
    bitstreamBufferPtr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}

// Exact prefix up to (but not including) the codecPicParams union; the union
// and all trailing fields are a zeroed tail. We only set pre-union fields and
// rely on enablePTD for picture-type decisions, so the rest stays zero. The
// driver reads only its version's size (~3.5 KB), so an over-large tail is
// safe.
#[repr(C)]
struct NvEncPicParams {
    version: u32,
    inputWidth: u32,
    inputHeight: u32,
    inputPitch: u32,
    encodePicFlags: u32,
    frameIdx: u32,
    inputTimeStamp: u64,
    inputDuration: u64,
    inputBuffer: *mut c_void,
    outputBitstream: *mut c_void,
    completionEvent: *mut c_void,
    bufferFmt: u32,
    pictureStruct: u32,
    pictureType: u32,
    tail: [u8; 4096],
}

// Exact prefix through bitstreamBufferPtr (the two fields we read are
// bitstreamSizeInBytes + bitstreamBufferPtr); the rest is a zeroed tail.
#[repr(C)]
struct NvEncLockBitstream {
    version: u32,
    flags: u32, // doNotWait..reservedBitFields
    outputBitstream: *mut c_void,
    sliceOffsets: *mut u32,
    frameIdx: u32,
    hwEncodeStatus: u32,
    numSlices: u32,
    bitstreamSizeInBytes: u32, // out
    outputTimeStamp: u64,
    outputDuration: u64,
    bitstreamBufferPtr: *mut c_void, // out
    tail: [u8; 2048],
}

macro_rules! zeroed_default {
    ($($t:ty),+ $(,)?) => {$(
        impl Default for $t {
            fn default() -> Self { unsafe { std::mem::zeroed() } }
        }
    )+};
}
zeroed_default!(
    NvEncRcParams,
    NvEncConfigH264,
    NvEncConfig,
    NvEncPresetConfig,
    NvEncInitializeParams,
    NvEncRegisterResource,
    NvEncMapInputResource,
    NvEncCreateBitstreamBuffer,
    NvEncPicParams,
    NvEncLockBitstream,
);

// Typed signatures for the Stage-2 functions we call.
type PFnGetPresetConfigEx =
    unsafe extern "system" fn(*mut c_void, Guid, Guid, u32, *mut NvEncPresetConfig) -> NVENCSTATUS;
type PFnInitializeEncoder =
    unsafe extern "system" fn(*mut c_void, *mut NvEncInitializeParams) -> NVENCSTATUS;
type PFnCreateBitstreamBuffer =
    unsafe extern "system" fn(*mut c_void, *mut NvEncCreateBitstreamBuffer) -> NVENCSTATUS;
type PFnRegisterResource =
    unsafe extern "system" fn(*mut c_void, *mut NvEncRegisterResource) -> NVENCSTATUS;
type PFnMapInputResource =
    unsafe extern "system" fn(*mut c_void, *mut NvEncMapInputResource) -> NVENCSTATUS;
type PFnEncodePicture = unsafe extern "system" fn(*mut c_void, *mut NvEncPicParams) -> NVENCSTATUS;
type PFnLockBitstream =
    unsafe extern "system" fn(*mut c_void, *mut NvEncLockBitstream) -> NVENCSTATUS;
type PFnPtrArg = unsafe extern "system" fn(*mut c_void, *mut c_void) -> NVENCSTATUS;

// ── Function list ───────────────────────────────────────────────────────
// All entries are pointer-sized; we type only the ones we call and leave the
// rest as raw pointers. Field order matches nvEncodeAPI.h exactly.
#[repr(C)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: *mut c_void,
    pub nvEncGetEncodeGUIDCount: *mut c_void,
    pub nvEncGetEncodeProfileGUIDCount: *mut c_void,
    pub nvEncGetEncodeProfileGUIDs: *mut c_void,
    pub nvEncGetEncodeGUIDs: *mut c_void,
    pub nvEncGetInputFormatCount: *mut c_void,
    pub nvEncGetInputFormats: *mut c_void,
    pub nvEncGetEncodeCaps: *mut c_void,
    pub nvEncGetEncodePresetCount: *mut c_void,
    pub nvEncGetEncodePresetGUIDs: *mut c_void,
    pub nvEncGetEncodePresetConfig: *mut c_void,
    pub nvEncInitializeEncoder: *mut c_void,
    pub nvEncCreateInputBuffer: *mut c_void,
    pub nvEncDestroyInputBuffer: *mut c_void,
    pub nvEncCreateBitstreamBuffer: *mut c_void,
    pub nvEncDestroyBitstreamBuffer: *mut c_void,
    pub nvEncEncodePicture: *mut c_void,
    pub nvEncLockBitstream: *mut c_void,
    pub nvEncUnlockBitstream: *mut c_void,
    pub nvEncLockInputBuffer: *mut c_void,
    pub nvEncUnlockInputBuffer: *mut c_void,
    pub nvEncGetEncodeStats: *mut c_void,
    pub nvEncGetSequenceParams: *mut c_void,
    pub nvEncRegisterAsyncEvent: *mut c_void,
    pub nvEncUnregisterAsyncEvent: *mut c_void,
    pub nvEncMapInputResource: *mut c_void,
    pub nvEncUnmapInputResource: *mut c_void,
    pub nvEncDestroyEncoder: *mut c_void,
    pub nvEncInvalidateRefFrames: *mut c_void,
    pub nvEncOpenEncodeSessionEx: *mut c_void,
    pub nvEncRegisterResource: *mut c_void,
    pub nvEncUnregisterResource: *mut c_void,
    pub nvEncReconfigureEncoder: *mut c_void,
    pub reserved1: *mut c_void,
    pub nvEncCreateMVBuffer: *mut c_void,
    pub nvEncDestroyMVBuffer: *mut c_void,
    pub nvEncRunMotionEstimationOnly: *mut c_void,
    pub nvEncGetLastErrorString: *mut c_void,
    pub nvEncSetIOCudaStreams: *mut c_void,
    pub nvEncGetEncodePresetConfigEx: *mut c_void,
    pub nvEncGetSequenceParamEx: *mut c_void,
    pub nvEncRestoreEncoderState: *mut c_void,
    pub nvEncLookaheadPicture: *mut c_void,
    pub reserved2: [*mut c_void; 275],
}

impl Default for NV_ENCODE_API_FUNCTION_LIST {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ── Typed function-pointer signatures (NVENCAPI = __stdcall = extern system) ─
type PFnOpenEncodeSessionEx =
    unsafe extern "system" fn(*mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut *mut c_void) -> NVENCSTATUS;
type PFnDestroyEncoder = unsafe extern "system" fn(*mut c_void) -> NVENCSTATUS;
type PFnGetLastError = unsafe extern "system" fn(*mut c_void) -> *const c_char;

type PFnCreateInstance =
    unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type PFnGetMaxVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;

use anyhow::{anyhow, Context, Result};
use windows::core::{s, Interface};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

/// The loaded NVENC API: the DLL handle plus the filled function list.
pub struct NvencApi {
    _lib: HMODULE,
    funcs: NV_ENCODE_API_FUNCTION_LIST,
}

impl NvencApi {
    /// Load `nvEncodeAPI64.dll`, verify the driver supports our API version,
    /// and fetch the function pointer table.
    pub fn load() -> Result<Self> {
        unsafe {
            let lib = LoadLibraryA(s!("nvEncodeAPI64.dll"))
                .context("LoadLibrary nvEncodeAPI64.dll (NVIDIA driver not installed?)")?;

            let get_max: PFnGetMaxVersion = {
                let p = GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))
                    .ok_or_else(|| anyhow!("NvEncodeAPIGetMaxSupportedVersion not exported"))?;
                std::mem::transmute(p)
            };
            let create: PFnCreateInstance = {
                let p = GetProcAddress(lib, s!("NvEncodeAPICreateInstance"))
                    .ok_or_else(|| anyhow!("NvEncodeAPICreateInstance not exported"))?;
                std::mem::transmute(p)
            };

            // Driver's max supported version is packed (major | minor<<4).
            // Ours must be <= driver's, else CreateInstance returns
            // INVALID_VERSION.
            let mut driver_max: u32 = 0;
            let st = get_max(&mut driver_max);
            if st != NV_ENC_SUCCESS {
                return Err(anyhow!("NvEncodeAPIGetMaxSupportedVersion failed: {st}"));
            }
            let our_ver = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
            tracing::debug!(
                driver_max,
                our_ver,
                "NVENC version check (driver_max must be >= our_ver)"
            );
            if driver_max < our_ver {
                return Err(anyhow!(
                    "NVENC driver too old: supports {driver_max:#x}, we need {our_ver:#x} \
                     (update the NVIDIA driver, or pin an older NVENCAPI version)"
                ));
            }

            let mut funcs = NV_ENCODE_API_FUNCTION_LIST::default();
            funcs.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            let st = create(&mut funcs);
            if st != NV_ENC_SUCCESS {
                return Err(anyhow!("NvEncodeAPICreateInstance failed: {st}"));
            }
            if funcs.nvEncOpenEncodeSessionEx.is_null() {
                return Err(anyhow!("NVENC function list missing OpenEncodeSessionEx"));
            }

            Ok(Self { _lib: lib, funcs })
        }
    }

    /// Open an encode session bound to a D3D11 device. The encoder runs on the
    /// GPU that device belongs to — pass the device that owns the NV12 input
    /// textures (the Spout-capture GPU) for zero-copy.
    pub fn open_session(&self, device: &ID3D11Device) -> Result<NvencSession<'_>> {
        let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS::default();
        params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        params.device = device.as_raw() as *mut c_void;
        params.apiVersion = NVENCAPI_VERSION;

        let mut encoder: *mut c_void = std::ptr::null_mut();
        let open: PFnOpenEncodeSessionEx =
            unsafe { std::mem::transmute(self.funcs.nvEncOpenEncodeSessionEx) };
        let st = unsafe { open(&mut params, &mut encoder) };
        if st != NV_ENC_SUCCESS || encoder.is_null() {
            return Err(anyhow!("NvEncOpenEncodeSessionEx failed: {st}"));
        }
        Ok(NvencSession {
            funcs: &self.funcs,
            encoder,
            bitstream: std::ptr::null_mut(),
            registered: std::collections::HashMap::new(),
            width: 0,
            height: 0,
            pts: 0,
        })
    }
}

/// An open NVENC session. After [`initialize`], feed D3D11 NV12 textures to
/// [`encode_texture`] and receive H.264 Annex-B access units.
pub struct NvencSession<'a> {
    funcs: &'a NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    bitstream: *mut c_void,
    /// D3D11 texture raw pointer → NVENC registered-resource handle. Each ring
    /// texture is registered once and reused.
    registered: std::collections::HashMap<usize, *mut c_void>,
    width: u32,
    height: u32,
    pts: u64,
}

impl<'a> NvencSession<'a> {
    /// Configure the encoder for `width`×`height` NV12 input at `fps` and
    /// `bitrate_kbps` and allocate the output bitstream buffer. Tuned for a
    /// VTuber overlay (latency-tolerant, quality-biased): P4 preset, LOW_LATENCY
    /// tuning, CBR with a 1-second VBV, High profile, no B-frames, 2-second GOP,
    /// AUD + repeated SPS/PPS so the receiver's parser + late joiners work.
    pub fn initialize(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<()> {
        // Start from the preset's recommended config, then override.
        let mut preset = NvEncPresetConfig::default();
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.presetCfg.version = NV_ENC_CONFIG_VER;
        let get_preset: PFnGetPresetConfigEx =
            unsafe { std::mem::transmute(self.funcs.nvEncGetEncodePresetConfigEx) };
        let st = unsafe {
            get_preset(
                self.encoder,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO_LOW_LATENCY,
                &mut preset,
            )
        };
        if st != NV_ENC_SUCCESS {
            return Err(anyhow!("nvEncGetEncodePresetConfigEx failed: {st}"));
        }

        let cfg = &mut preset.presetCfg;
        cfg.version = NV_ENC_CONFIG_VER;
        cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        cfg.gopLength = fps * 2; // 2-second GOP
        cfg.frameIntervalP = 1; // no B-frames (keep latency low + simple PTS)
        cfg.rcParams.version = NV_ENC_RC_PARAMS_VER;
        cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        cfg.rcParams.averageBitRate = bitrate_kbps * 1000;
        cfg.rcParams.maxBitRate = bitrate_kbps * 1000;
        // 1-second VBV so the encoder can spend extra bits on complex frames
        // instead of hard-capping every frame (a ~1-frame VBV caused visible
        // blocking on detailed content). Still streaming-friendly latency.
        cfg.rcParams.vbvBufferSize = bitrate_kbps * 1000;
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        let h264 = &mut cfg.encodeCodecConfig;
        h264.idrPeriod = cfg.gopLength;
        h264.flags |= H264_FLAG_OUTPUT_AUD | H264_FLAG_REPEAT_SPSPPS;
        h264.chromaFormatIDC = 1; // 4:2:0 (NV12)

        let mut init = NvEncInitializeParams::default();
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = NV_ENC_CODEC_H264_GUID;
        init.presetGUID = NV_ENC_PRESET_P4_GUID;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.maxEncodeWidth = width;
        init.maxEncodeHeight = height;
        init.frameRateNum = fps;
        init.frameRateDen = 1;
        init.enablePTD = 1; // let NVENC decide picture types
        init.enableEncodeAsync = 0; // synchronous
        init.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
        init.encodeConfig = cfg as *mut NvEncConfig;

        let initialize: PFnInitializeEncoder =
            unsafe { std::mem::transmute(self.funcs.nvEncInitializeEncoder) };
        let st = unsafe { initialize(self.encoder, &mut init) };
        if st != NV_ENC_SUCCESS {
            return Err(anyhow!(
                "nvEncInitializeEncoder failed: {st} — {}",
                self.last_error()
            ));
        }

        // One reusable output bitstream buffer.
        let mut cbb = NvEncCreateBitstreamBuffer::default();
        cbb.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let create_bb: PFnCreateBitstreamBuffer =
            unsafe { std::mem::transmute(self.funcs.nvEncCreateBitstreamBuffer) };
        let st = unsafe { create_bb(self.encoder, &mut cbb) };
        if st != NV_ENC_SUCCESS {
            return Err(anyhow!("nvEncCreateBitstreamBuffer failed: {st}"));
        }
        self.bitstream = cbb.bitstreamBuffer;
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Register a D3D11 texture as an NVENC input resource (once per texture),
    /// returning the registered handle. Cached by the texture's raw pointer so
    /// the converter's small ring is registered lazily and reused.
    fn registered_for(&mut self, tex: &ID3D11Texture2D) -> Result<*mut c_void> {
        let key = tex.as_raw() as usize;
        if let Some(h) = self.registered.get(&key) {
            return Ok(*h);
        }
        let mut reg = NvEncRegisterResource::default();
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.width;
        reg.height = self.height;
        reg.pitch = 0; // DirectX: driver derives pitch
        reg.subResourceIndex = 0;
        reg.resourceToRegister = tex.as_raw() as *mut c_void;
        reg.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
        reg.bufferUsage = NV_ENC_INPUT_IMAGE;
        let f: PFnRegisterResource =
            unsafe { std::mem::transmute(self.funcs.nvEncRegisterResource) };
        let st = unsafe { f(self.encoder, &mut reg) };
        if st != NV_ENC_SUCCESS {
            return Err(anyhow!(
                "nvEncRegisterResource failed: {st} — {}",
                self.last_error()
            ));
        }
        self.registered.insert(key, reg.registeredResource);
        Ok(reg.registeredResource)
    }

    /// Encode one NV12 D3D11 texture (on the session's device). Returns the
    /// H.264 access unit, or `None` if the encoder buffered the frame and
    /// produced no output this call. `force_idr` makes this frame an IDR
    /// (with SPS/PPS) regardless of GOP position — used when a subscriber
    /// reports picture loss (PLI) so it can re-sync immediately.
    pub fn encode_texture(
        &mut self,
        tex: &ID3D11Texture2D,
        force_idr: bool,
    ) -> Result<Option<Vec<u8>>> {
        let registered = self.registered_for(tex)?;

        // Map for this submission.
        let mut map = NvEncMapInputResource::default();
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = registered;
        let map_fn: PFnMapInputResource =
            unsafe { std::mem::transmute(self.funcs.nvEncMapInputResource) };
        let st = unsafe { map_fn(self.encoder, &mut map) };
        if st != NV_ENC_SUCCESS {
            return Err(anyhow!("nvEncMapInputResource failed: {st}"));
        }
        let mapped = map.mappedResource;

        // Submit.
        let mut pic = NvEncPicParams::default();
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.width;
        pic.inputHeight = self.height;
        pic.inputPitch = self.width;
        pic.inputBuffer = mapped;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        if force_idr {
            pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        pic.inputTimeStamp = self.pts;
        pic.inputDuration = 1;
        self.pts += 1;

        let encode: PFnEncodePicture =
            unsafe { std::mem::transmute(self.funcs.nvEncEncodePicture) };
        let st = unsafe { encode(self.encoder, &mut pic) };

        let out = if st == NV_ENC_SUCCESS {
            Some(self.lock_and_copy()?)
        } else if st == NV_ENC_ERR_NEED_MORE_INPUT {
            None
        } else {
            // Still unmap before bailing.
            self.unmap(mapped);
            return Err(anyhow!(
                "nvEncEncodePicture failed: {st} — {}",
                self.last_error()
            ));
        };

        self.unmap(mapped);
        Ok(out)
    }

    /// Lock the output bitstream, copy the access unit out, unlock.
    fn lock_and_copy(&mut self) -> Result<Vec<u8>> {
        let mut lock = NvEncLockBitstream::default();
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        let lock_fn: PFnLockBitstream =
            unsafe { std::mem::transmute(self.funcs.nvEncLockBitstream) };
        let st = unsafe { lock_fn(self.encoder, &mut lock) };
        if st != NV_ENC_SUCCESS {
            return Err(anyhow!("nvEncLockBitstream failed: {st}"));
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            )
            .to_vec()
        };
        let unlock: PFnPtrArg = unsafe { std::mem::transmute(self.funcs.nvEncUnlockBitstream) };
        unsafe {
            let _ = unlock(self.encoder, self.bitstream);
        }
        Ok(bytes)
    }

    fn unmap(&self, mapped: *mut c_void) {
        let f: PFnPtrArg = unsafe { std::mem::transmute(self.funcs.nvEncUnmapInputResource) };
        unsafe {
            let _ = f(self.encoder, mapped);
        }
    }

    /// Last driver error string for diagnostics.
    pub fn last_error(&self) -> String {
        if self.funcs.nvEncGetLastErrorString.is_null() {
            return String::new();
        }
        let f: PFnGetLastError =
            unsafe { std::mem::transmute(self.funcs.nvEncGetLastErrorString) };
        let p = unsafe { f(self.encoder) };
        if p.is_null() {
            return String::new();
        }
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned()
    }
}

impl<'a> Drop for NvencSession<'a> {
    fn drop(&mut self) {
        unsafe {
            // Unregister all cached input resources.
            let unreg: PFnPtrArg = std::mem::transmute(self.funcs.nvEncUnregisterResource);
            for (_, h) in self.registered.drain() {
                let _ = unreg(self.encoder, h);
            }
            if !self.bitstream.is_null() {
                let destroy_bb: PFnPtrArg =
                    std::mem::transmute(self.funcs.nvEncDestroyBitstreamBuffer);
                let _ = destroy_bb(self.encoder, self.bitstream);
                self.bitstream = std::ptr::null_mut();
            }
            if !self.encoder.is_null() && !self.funcs.nvEncDestroyEncoder.is_null() {
                let f: PFnDestroyEncoder = std::mem::transmute(self.funcs.nvEncDestroyEncoder);
                let _ = f(self.encoder);
                self.encoder = std::ptr::null_mut();
            }
        }
    }
}
