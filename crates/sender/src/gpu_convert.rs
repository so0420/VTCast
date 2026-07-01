//! GPU-resident side-by-side pack + RGBA→NV12 conversion (Windows/D3D11).
//!
//! Replaces the CPU readback → `pack_rgba_side_by_side` → `rgba_to_nv12`
//! chain for the Spout capture path. The Spout shared texture never leaves
//! VRAM: we snapshot it with a GPU→GPU `CopyResource`, then a pixel shader
//! samples it and writes an NV12 texture's luma + chroma planes directly.
//! The resulting NV12 texture is fed straight into the hardware encoder MFT
//! (see [`crate::mf_encoder::AsyncMfEncoder::submit_input_texture`]), so a
//! frame is produced and consumed entirely on the GPU.
//!
//! The colour maths mirror [`crate::mf_encoder::rgba_to_nv12`] exactly
//! (BT.601 limited range) so the receiver's WebGL reconstruction shader sees
//! the same values whichever pipeline produced the frame.
//!
//! Vendor-neutral: this is plain D3D11 + HLSL, so it runs on any GPU. The
//! encoder it feeds is selected through Media Foundation, which abstracts
//! NVENC / Quick Sync / AMF behind one interface.

use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11Resource, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_COMPARISON_NEVER, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV, D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_BIND_CONSTANT_BUFFER,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_FORMAT_R10G10B10A2_TYPELESS, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_TYPELESS,
    DXGI_FORMAT_R16G16B16A16_UNORM, DXGI_FORMAT_R32G32B32A32_FLOAT,
    DXGI_FORMAT_R32G32B32A32_TYPELESS, DXGI_FORMAT_R8G8B8A8_TYPELESS,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
    DXGI_SAMPLE_DESC,
};

// DXGI_FORMAT numeric values we match against the Spout sender's reported
// format. windows-rs exposes the named constants we render *to*; the source
// format arrives as a raw u32 from the Spout registry.
const FMT_B8G8R8A8_UNORM: u32 = 87;
const FMT_B8G8R8A8_UNORM_SRGB: u32 = 91;
const FMT_B8G8R8A8_TYPELESS: u32 = 90;
const FMT_R8G8B8A8_UNORM: u32 = 28;
const FMT_R8G8B8A8_UNORM_SRGB: u32 = 29;
const FMT_R8G8B8A8_TYPELESS: u32 = 27;
// Wide-gamut / HDR families some avatar apps emit (Spout can carry any DXGI
// format). The shader samples everything as float4, so once the snapshot +
// SRV formats match the source's copy family, the NV12 output is identical
// 8-bit regardless of source bit depth.
const FMT_R10G10B10A2_TYPELESS: u32 = 23;
const FMT_R10G10B10A2_UNORM: u32 = 24;
const FMT_R16G16B16A16_TYPELESS: u32 = 9;
const FMT_R16G16B16A16_FLOAT: u32 = 10;
const FMT_R16G16B16A16_UNORM: u32 = 11;
const FMT_R32G32B32A32_TYPELESS: u32 = 1;
const FMT_R32G32B32A32_FLOAT: u32 = 2;

/// HLSL for the pack + NV12 conversion. One fullscreen-triangle VS, plus a
/// luma PS (writes the R8 plane at full size) and a chroma PS (writes the
/// R8G8 plane at half size). The left half of the output carries the source
/// RGB; the right half carries the source alpha replicated as grey (neutral
/// chroma), exactly as the CPU packer does.
const SHADER_HLSL: &str = r#"
cbuffer Params : register(b0) {
    float2 outSize;   // bound render target size, in pixels
    float2 _pad;
};
Texture2D<float4> src : register(t0);
SamplerState samp : register(s0);

struct VSOut { float4 pos : SV_Position; };

VSOut vs_main(uint vid : SV_VertexID) {
    // Fullscreen triangle: (0,0) (2,0) (0,2) in UV, mapped to clip space.
    float2 p = float2((vid << 1) & 2, vid & 2);
    VSOut o;
    o.pos = float4(p * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

// Luma plane (R8). Left half: BT.601 Y of source RGB. Right half: source
// alpha as grey luma (matches CPU packer's (A,A,A) -> Y).
float ps_luma(VSOut i) : SV_Target {
    float2 px = i.pos.xy;                 // pixel-centre coords [0,outSize)
    float halfw = outSize.x * 0.5;
    if (px.x < halfw) {
        float2 uv = float2(px.x / halfw, px.y / outSize.y);
        float4 c = src.SampleLevel(samp, uv, 0);
        return 0.257 * c.r + 0.504 * c.g + 0.098 * c.b + 16.0 / 255.0;
    } else {
        float2 uv = float2((px.x - halfw) / halfw, px.y / outSize.y);
        float4 c = src.SampleLevel(samp, uv, 0);
        return 0.859 * c.a + 16.0 / 255.0;
    }
}

// Chroma plane (R8G8). Bound at half luma size. Left half: BT.601 U,V of
// source RGB (linear sampler averages the 2x2 footprint). Right half: the
// alpha region is grey, so neutral chroma.
float2 ps_chroma(VSOut i) : SV_Target {
    float2 px = i.pos.xy;
    float halfw = outSize.x * 0.5;
    if (px.x < halfw) {
        float2 uv = float2(px.x / halfw, px.y / outSize.y);
        float4 c = src.SampleLevel(samp, uv, 0);
        float u = -0.148 * c.r - 0.291 * c.g + 0.439 * c.b + 128.0 / 255.0;
        float v =  0.439 * c.r - 0.368 * c.g - 0.071 * c.b + 128.0 / 255.0;
        return float2(u, v);
    } else {
        return float2(128.0 / 255.0, 128.0 / 255.0);
    }
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    out_w: f32,
    out_h: f32,
    _pad: [f32; 2],
}

/// Number of NV12 textures cycled through. An async hardware MFT can hold
/// several input surfaces in flight; rendering into a fresh ring slot each
/// frame keeps us from overwriting one the encoder is still reading. With the
/// encode loop bounding its convert-ahead to 2 frames, 4 slots leaves comfor-
/// table margin before a slot is reused.
const RING: usize = 4;

/// GPU NV12 converter bound to one shared device + one source texture.
pub struct Nv12Converter {
    context: ID3D11DeviceContext,
    src_tex: ID3D11Texture2D,
    /// GPU→GPU snapshot of the source, so we sample a stable frame instead of
    /// racing the Spout sender's writes.
    snapshot: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    /// Ring of NV12 outputs handed to the encoder; `convert` advances through
    /// them so an in-flight surface is never overwritten.
    nv12: Vec<ID3D11Texture2D>,
    rtv_luma: Vec<ID3D11RenderTargetView>,
    rtv_chroma: Vec<ID3D11RenderTargetView>,
    ring_index: std::cell::Cell<usize>,
    vs: ID3D11VertexShader,
    ps_luma: ID3D11PixelShader,
    ps_chroma: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cb_luma: ID3D11Buffer,
    cb_chroma: ID3D11Buffer,
    packed_w: u32,
    packed_h: u32,
}

impl Nv12Converter {
    /// Build the conversion pipeline. `src_tex` is the Spout shared texture
    /// (on `device`); `src_w`/`src_h` are its dimensions and `src_format` the
    /// raw DXGI_FORMAT value from the Spout registry. `packed_w`/`packed_h`
    /// are the NV12 output dimensions (already side-by-side-doubled and
    /// even-clamped); the shader scales the source to fit via the sampler, so
    /// this also subsumes the old CPU box-resize.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_tex: &ID3D11Texture2D,
        src_w: u32,
        src_h: u32,
        src_format: u32,
        packed_w: u32,
        packed_h: u32,
    ) -> Result<Self> {
        if packed_w % 2 != 0 || packed_h % 2 != 0 {
            return Err(anyhow!(
                "NV12 requires even dims, got {}x{}",
                packed_w,
                packed_h
            ));
        }
        let (snap_fmt, srv_fmt) = snapshot_formats(src_format);

        // Snapshot texture: same typeless family as the source so CopyResource
        // is legal, sampled through a non-sRGB UNORM SRV so we read raw bytes
        // (matching the CPU path, which never linearises).
        let snapshot = create_texture2d(
            device,
            src_w,
            src_h,
            snap_fmt,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
        )
        .context("create snapshot texture")?;

        let srv = {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: srv_fmt,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                    },
                },
            };
            let res: ID3D11Resource = snapshot.cast().context("snapshot as resource")?;
            let mut out: Option<ID3D11ShaderResourceView> = None;
            unsafe {
                device
                    .CreateShaderResourceView(&res, Some(&desc), Some(&mut out))
                    .context("CreateShaderResourceView")?;
            }
            out.ok_or_else(|| anyhow!("SRV null"))?
        };

        // NV12 output ring + a render-target view per plane. In D3D11 the
        // plane is selected by the RTV format: R8_UNORM == luma, R8G8_UNORM
        // == chroma (half res).
        let mut nv12 = Vec::with_capacity(RING);
        let mut rtv_luma = Vec::with_capacity(RING);
        let mut rtv_chroma = Vec::with_capacity(RING);
        for _ in 0..RING {
            let tex = create_texture2d(
                device,
                packed_w,
                packed_h,
                DXGI_FORMAT_NV12,
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            )
            .context("create NV12 texture")?;
            let res: ID3D11Resource = tex.cast().context("nv12 as resource")?;
            rtv_luma.push(
                create_rtv(device, &res, DXGI_FORMAT_R8_UNORM).context("create luma RTV")?,
            );
            rtv_chroma.push(
                create_rtv(device, &res, DXGI_FORMAT_R8G8_UNORM).context("create chroma RTV")?,
            );
            nv12.push(tex);
        }

        // Shaders.
        let vs_blob = compile_shader(SHADER_HLSL, s!("vs_main"), s!("vs_5_0"))?;
        let ps_luma_blob = compile_shader(SHADER_HLSL, s!("ps_luma"), s!("ps_5_0"))?;
        let ps_chroma_blob = compile_shader(SHADER_HLSL, s!("ps_chroma"), s!("ps_5_0"))?;
        let vs = {
            let mut out: Option<ID3D11VertexShader> = None;
            unsafe {
                device
                    .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut out))
                    .context("CreateVertexShader")?;
            }
            out.ok_or_else(|| anyhow!("VS null"))?
        };
        let ps_luma = create_ps(device, &ps_luma_blob).context("create luma PS")?;
        let ps_chroma = create_ps(device, &ps_chroma_blob).context("create chroma PS")?;

        // Linear-clamp sampler (averages on downscale).
        let sampler = {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut out: Option<ID3D11SamplerState> = None;
            unsafe {
                device
                    .CreateSamplerState(&desc, Some(&mut out))
                    .context("CreateSamplerState")?;
            }
            out.ok_or_else(|| anyhow!("sampler null"))?
        };

        // Per-plane constant buffers (immutable).
        let cb_luma = create_const_buffer(
            device,
            Params {
                out_w: packed_w as f32,
                out_h: packed_h as f32,
                _pad: [0.0; 2],
            },
        )
        .context("luma cbuffer")?;
        let cb_chroma = create_const_buffer(
            device,
            Params {
                out_w: (packed_w / 2) as f32,
                out_h: (packed_h / 2) as f32,
                _pad: [0.0; 2],
            },
        )
        .context("chroma cbuffer")?;

        Ok(Self {
            context: context.clone(),
            src_tex: src_tex.clone(),
            snapshot,
            srv,
            nv12,
            rtv_luma,
            rtv_chroma,
            ring_index: std::cell::Cell::new(0),
            vs,
            ps_luma,
            ps_chroma,
            sampler,
            cb_luma,
            cb_chroma,
            packed_w,
            packed_h,
        })
    }

    /// Snapshot the source and render both NV12 planes into the next ring
    /// slot. Returns that NV12 texture; it stays valid until `convert` cycles
    /// back to the same slot (`RING` calls later).
    pub fn convert(&self) -> Result<&ID3D11Texture2D> {
        let idx = (self.ring_index.get() + 1) % self.nv12.len();
        self.ring_index.set(idx);
        let ctx = &self.context;
        unsafe {
            // Stable snapshot of the live Spout texture (GPU→GPU, no readback).
            let src: ID3D11Resource = self.src_tex.cast()?;
            let dst: ID3D11Resource = self.snapshot.cast()?;
            ctx.CopyResource(&dst, &src);

            // Shared pipeline state.
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(self.srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));

            // Luma pass — full size.
            ctx.PSSetShader(&self.ps_luma, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb_luma.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv_luma[idx].clone())]), None);
            set_viewport(ctx, self.packed_w, self.packed_h);
            ctx.Draw(3, 0);

            // Chroma pass — half size.
            ctx.PSSetShader(&self.ps_chroma, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb_chroma.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv_chroma[idx].clone())]), None);
            set_viewport(ctx, self.packed_w / 2, self.packed_h / 2);
            ctx.Draw(3, 0);

            // Unbind the render targets so the encoder MFT can read the NV12
            // texture without a read/write hazard, then submit the queued GPU
            // work (cheap — submission, not a CPU stall).
            ctx.OMSetRenderTargets(None, None);
            ctx.Flush();
        }
        Ok(&self.nv12[idx])
    }
}

/// Map a source DXGI_FORMAT to (snapshot texture format, SRV sample format).
/// The snapshot uses the typeless family member so CopyResource accepts the
/// typed source; the SRV uses the plain UNORM variant so sampling returns raw
/// bytes (no sRGB linearisation), matching the CPU packer.
fn snapshot_formats(src_format: u32) -> (DXGI_FORMAT, DXGI_FORMAT) {
    match src_format {
        FMT_B8G8R8A8_UNORM | FMT_B8G8R8A8_UNORM_SRGB | FMT_B8G8R8A8_TYPELESS => {
            (DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM)
        }
        FMT_R8G8B8A8_UNORM | FMT_R8G8B8A8_UNORM_SRGB | FMT_R8G8B8A8_TYPELESS => {
            (DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM)
        }
        // Wide-gamut / HDR sources. Snapshot in the typeless parent (so
        // CopyResource stays in-family with the typed source) and sample
        // through the concrete typed SRV; the shader's float4 samples read
        // 10-bit UNORM / 16-bit + 32-bit FLOAT values transparently and the
        // BT.601 maths clamp them into the 8-bit NV12 output.
        FMT_R10G10B10A2_UNORM | FMT_R10G10B10A2_TYPELESS => {
            (DXGI_FORMAT_R10G10B10A2_TYPELESS, DXGI_FORMAT_R10G10B10A2_UNORM)
        }
        FMT_R16G16B16A16_FLOAT | FMT_R16G16B16A16_TYPELESS => {
            (DXGI_FORMAT_R16G16B16A16_TYPELESS, DXGI_FORMAT_R16G16B16A16_FLOAT)
        }
        FMT_R16G16B16A16_UNORM => {
            (DXGI_FORMAT_R16G16B16A16_TYPELESS, DXGI_FORMAT_R16G16B16A16_UNORM)
        }
        FMT_R32G32B32A32_FLOAT | FMT_R32G32B32A32_TYPELESS => {
            (DXGI_FORMAT_R32G32B32A32_TYPELESS, DXGI_FORMAT_R32G32B32A32_FLOAT)
        }
        // Unknown: best-effort RGBA. If the family doesn't match, CopyResource
        // fails at convert() and the caller falls back to the CPU path.
        _ => (DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM),
    }
}

fn create_texture2d(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bind_flags: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut out: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut out))
            .context("CreateTexture2D")?;
    }
    out.ok_or_else(|| anyhow!("texture null"))
}

fn create_rtv(
    device: &ID3D11Device,
    resource: &ID3D11Resource,
    format: DXGI_FORMAT,
) -> Result<ID3D11RenderTargetView> {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    };
    let mut out: Option<ID3D11RenderTargetView> = None;
    unsafe {
        device
            .CreateRenderTargetView(resource, Some(&desc), Some(&mut out))
            .context("CreateRenderTargetView")?;
    }
    out.ok_or_else(|| anyhow!("RTV null"))
}

fn create_ps(device: &ID3D11Device, blob: &ID3DBlob) -> Result<ID3D11PixelShader> {
    let mut out: Option<ID3D11PixelShader> = None;
    unsafe {
        device
            .CreatePixelShader(blob_bytes(blob), None, Some(&mut out))
            .context("CreatePixelShader")?;
    }
    out.ok_or_else(|| anyhow!("PS null"))
}

fn create_const_buffer(device: &ID3D11Device, params: Params) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<Params>() as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: &params as *const Params as *const c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut out: Option<ID3D11Buffer> = None;
    unsafe {
        device
            .CreateBuffer(&desc, Some(&init), Some(&mut out))
            .context("CreateBuffer")?;
    }
    out.ok_or_else(|| anyhow!("const buffer null"))
}

fn set_viewport(ctx: &ID3D11DeviceContext, w: u32, h: u32) {
    let vp = D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: w as f32,
        Height: h as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    unsafe { ctx.RSSetViewports(Some(&[vp])) };
}

fn compile_shader(src: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            src.as_ptr() as *const c_void,
            src.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = hr {
        let msg = errors
            .as_ref()
            .map(|b| unsafe {
                let p = b.GetBufferPointer() as *const u8;
                let n = b.GetBufferSize();
                String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).to_string()
            })
            .unwrap_or_default();
        return Err(anyhow!("D3DCompile failed: {e} — {msg}"));
    }
    code.ok_or_else(|| anyhow!("shader blob null"))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        let p = blob.GetBufferPointer() as *const u8;
        let n = blob.GetBufferSize();
        std::slice::from_raw_parts(p, n)
    }
}
