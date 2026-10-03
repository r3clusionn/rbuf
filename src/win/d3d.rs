//! The Direct3D 11 device everything shares: capture writes into its textures, the compute shader
//! converts on it, and the encoder reads from it, so frames stay in GPU memory end to end.

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_FLAG,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_DESC1};

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter_name: String,
    pub luid: LUID,
}

// The device is free-threaded and the immediate context is multithread protected (see `new`), so
// the capture callback, the pacer and the encoder may share them.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

/// The adapters on this machine: (index, name).
pub fn adapters() -> Result<Vec<(u32, String)>> {
    let f: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    let mut i = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(i) } {
        let d = unsafe { a.GetDesc1()? };
        out.push((i, name_of(&d)));
        i += 1;
    }
    Ok(out)
}

fn name_of(d: &DXGI_ADAPTER_DESC1) -> String {
    let n = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
    String::from_utf16_lossy(&d.Description[..n])
}

impl Gpu {
    /// A device on the given adapter (default: the first, which owns the primary display).
    pub fn new(adapter: Option<u32>) -> Result<Gpu> {
        unsafe {
            let f: IDXGIFactory1 = CreateDXGIFactory1()?;
            let a: IDXGIAdapter1 = f.EnumAdapters1(adapter.unwrap_or(0))?;
            let desc = a.GetDesc1()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let context = context.unwrap();
            // Capture callbacks, the converter and the encoder use the device from different threads.
            let mt: ID3D11Multithread = context.cast()?;
            let _ = mt.SetMultithreadProtected(true);
            Ok(Gpu { device, context, adapter_name: name_of(&desc), luid: desc.AdapterLuid })
        }
    }

    pub fn dxgi_device(&self) -> Result<IDXGIDevice> {
        self.device.cast()
    }

    pub fn texture(&self, width: u32, height: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t))? };
        Ok(t.unwrap())
    }
}

pub fn texture_size(t: &ID3D11Texture2D) -> (u32, u32) {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { t.GetDesc(&mut d) };
    (d.Width, d.Height)
}
