//! D3D11 / DXGI helpers — adapter enumeration and staging-texture creation.
//! Adapter probing exists because Spout's shared-texture handles only open
//! on the device whose adapter owns them; on multi-GPU laptops that's
//! usually the dGPU, not whatever a default `D3D11CreateDevice` picks.

use anyhow::{anyhow, Context, Result};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

pub fn enumerate_adapters() -> Result<Vec<(String, IDXGIAdapter)>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            match factory.EnumAdapters(i) {
                Ok(adapter) => {
                    let desc = adapter.GetDesc().context("adapter GetDesc")?;
                    let name = String::from_utf16_lossy(
                        &desc.Description[..desc
                            .Description
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.Description.len())],
                    );
                    out.push((name, adapter));
                    i += 1;
                }
                Err(_) => break,
            }
        }
        Ok(out)
    }
}

pub fn create_d3d11_on(
    adapter: Option<&IDXGIAdapter>,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device = None;
        let mut context = None;
        let mut feature_level = D3D_FEATURE_LEVEL_11_0;
        let driver_type = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };
        D3D11CreateDevice(
            adapter,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        Ok((
            device.ok_or_else(|| anyhow!("device null"))?,
            context.ok_or_else(|| anyhow!("context null"))?,
        ))
    }
}

/// Create a D3D11 device suitable for *both* Spout capture and Media
/// Foundation hardware encoding on the given adapter. Compared to
/// [`create_d3d11_on`] this adds:
///   * `BGRA_SUPPORT` + `VIDEO_SUPPORT` — required for MF video MFTs that
///     route through D3D, and for sampling BGRA shared textures in shaders.
///   * `ID3D11Multithread` protection — MF drives the device from its own
///     worker threads, so this is mandatory once the device is handed to a
///     DXGI device manager.
///
/// The returned device is the one the GPU zero-copy pipeline shares between
/// the Spout receiver, the NV12 conversion shader, and the encoder MFT.
pub fn create_shared_device(
    adapter: Option<&IDXGIAdapter>,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device = None;
        let mut context = None;
        let mut feature_level = D3D_FEATURE_LEVEL_11_0;
        let driver_type = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };
        let flags = D3D11_CREATE_DEVICE_FLAG(
            D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
        );
        D3D11CreateDevice(
            adapter,
            driver_type,
            HMODULE::default(),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
        .context("D3D11CreateDevice (shared capture+encode)")?;
        let device = device.ok_or_else(|| anyhow!("device null"))?;
        let context = context.ok_or_else(|| anyhow!("context null"))?;

        // MF accesses the device from worker threads — protect it now, before
        // it is ever handed to a DXGI device manager.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok((device, context))
    }
}

pub fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&mut desc, None, Some(&mut staging))
            .with_context(|| {
                format!(
                    "CreateTexture2D(staging) {}x{} fmt={}",
                    width, height, format.0
                )
            })?;
    }
    staging.ok_or_else(|| anyhow!("staging texture null"))
}
