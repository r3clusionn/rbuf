//! BGRA to NV12 on the GPU with a compute shader: BT.709 coefficients, limited range (16-235 luma,
//! 16-240 chroma), chroma averaged over each 2x2 block, and bilinear scaling to the output size
//! (a window that changes size keeps the size the recording started with). The NV12 planes are
//! written through unordered-access views, so frames never leave video memory.

use windows::core::{s, Interface, Result};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device3, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11UnorderedAccessView, ID3D11UnorderedAccessView1, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_FEATURE_DATA_FORMAT_SUPPORT2,
    D3D11_FEATURE_FORMAT_SUPPORT2, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_SRV,
    D3D11_TEX2D_UAV1, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC1,
    D3D11_UNORDERED_ACCESS_VIEW_DESC1_0, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
};

use super::d3d::{texture_size, Gpu};

pub const SHADER: &str = r#"
Texture2D<float4> src : register(t0);
SamplerState samp : register(s0);
RWTexture2D<float> dstY : register(u0);
RWTexture2D<float2> dstUV : register(u1);
cbuffer Params : register(b0) {
    float2 outSize;   // output texture width, height
    float2 uvScale;   // content size / source texture size
    float2 active;    // the picture's size inside the output; the rest is black padding
    float2 unused;
};

float3 rgbAt(float2 px) {
    if (px.x >= active.x || px.y >= active.y) return float3(0, 0, 0);
    return src.SampleLevel(samp, (px + 0.5) / active * uvScale, 0).rgb;
}

float luma(float3 c) {
    return (16.0 + 219.0 * dot(c, float3(0.2126, 0.7152, 0.0722))) / 255.0;
}

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint2 p = id.xy * 2;
    if (p.x >= (uint)outSize.x || p.y >= (uint)outSize.y) return;
    float3 a = rgbAt(p);
    float3 b = rgbAt(p + uint2(1, 0));
    float3 c = rgbAt(p + uint2(0, 1));
    float3 d = rgbAt(p + uint2(1, 1));
    dstY[p] = luma(a);
    dstY[p + uint2(1, 0)] = luma(b);
    dstY[p + uint2(0, 1)] = luma(c);
    dstY[p + uint2(1, 1)] = luma(d);
    float3 m = (a + b + c + d) * 0.25;
    float cb = (128.0 + 224.0 * dot(m, float3(-0.1146, -0.3854, 0.5))) / 255.0;
    float cr = (128.0 + 224.0 * dot(m, float3(0.5, -0.4542, -0.0458))) / 255.0;
    dstUV[id.xy] = float2(cb, cr);
}
"#;

/// An NV12 texture with views on both planes.
pub struct Nv12 {
    pub texture: ID3D11Texture2D,
    y: ID3D11UnorderedAccessView,
    uv: ID3D11UnorderedAccessView,
}

pub struct Converter {
    gpu: Gpu,
    shader: ID3D11ComputeShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    pub width: u32,
    pub height: u32,
    /// The picture's size; smaller than the texture when the encoder needs padded frames.
    pub active: (u32, u32),
    srv: Option<(usize, ID3D11ShaderResourceView)>,
}

pub fn nv12_uav_supported(gpu: &Gpu) -> bool {
    let mut s = D3D11_FEATURE_DATA_FORMAT_SUPPORT2 { InFormat: DXGI_FORMAT_NV12, OutFormatSupport2: 0 };
    let ok = unsafe {
        gpu.device.CheckFeatureSupport(
            D3D11_FEATURE_FORMAT_SUPPORT2,
            &mut s as *mut _ as *mut _,
            std::mem::size_of_val(&s) as u32,
        )
    };
    ok.is_ok() && s.OutFormatSupport2 & D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE.0 as u32 != 0
}

fn compile(src: &str) -> Result<Vec<u8>> {
    let mut blob: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            s!("nv12.hlsl"),
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    if let Err(e) = r {
        let msg = err
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()))
                    .into_owned()
            })
            .unwrap_or_default();
        return Err(windows::core::Error::new(e.code(), msg));
    }
    let b = blob.unwrap();
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() })
}

impl Converter {
    /// Output size must be even (NV12 has one chroma sample per 2x2 block).
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Converter> {
        let code = compile(SHADER)?;
        let mut shader = None;
        unsafe { gpu.device.CreateComputeShader(&code, None, Some(&mut shader))? };
        let sd = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { gpu.device.CreateSamplerState(&sd, Some(&mut sampler))? };
        let bd = D3D11_BUFFER_DESC {
            ByteWidth: 32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let init = [width as f32, height as f32, 1.0f32, 1.0, width as f32, height as f32, 0.0, 0.0];
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: init.as_ptr() as *const _, ..Default::default() };
        let mut params = None;
        unsafe { gpu.device.CreateBuffer(&bd, Some(&data), Some(&mut params))? };
        Ok(Converter {
            gpu: gpu.clone(),
            shader: shader.unwrap(),
            sampler: sampler.unwrap(),
            params: params.unwrap(),
            width,
            height,
            active: (width, height),
            srv: None,
        })
    }

    /// An NV12 texture this converter can write, which the encoder can read.
    pub fn target(&self) -> Result<Nv12> {
        let texture = self.gpu.texture(
            self.width,
            self.height,
            DXGI_FORMAT_NV12,
            D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE,
        )?;
        let dev3: ID3D11Device3 = self.gpu.device.cast()?;
        let view = |plane: u32, fmt| -> Result<ID3D11UnorderedAccessView> {
            let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC1 {
                Format: fmt,
                ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC1_0 { Texture2D: D3D11_TEX2D_UAV1 { MipSlice: 0, PlaneSlice: plane } },
            };
            let mut v: Option<ID3D11UnorderedAccessView1> = None;
            unsafe { dev3.CreateUnorderedAccessView1(&texture, Some(&desc), Some(&mut v))? };
            v.unwrap().cast()
        };
        Ok(Nv12 { y: view(0, DXGI_FORMAT_R8_UNORM)?, uv: view(1, DXGI_FORMAT_R8G8_UNORM)?, texture })
    }

    /// Converts the `content` part of a BGRA texture into `out`.
    pub fn convert(&mut self, src: &ID3D11Texture2D, content: (u32, u32), out: &Nv12) -> Result<()> {
        let key = src.as_raw() as usize;
        if self.srv.as_ref().map(|s| s.0) != Some(key) {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
            };
            let mut v = None;
            unsafe { self.gpu.device.CreateShaderResourceView(src, Some(&desc), Some(&mut v))? };
            self.srv = Some((key, v.unwrap()));
        }
        let (tw, th) = texture_size(src);
        let p = [
            self.width as f32,
            self.height as f32,
            content.0 as f32 / tw as f32,
            content.1 as f32 / th as f32,
            self.active.0 as f32,
            self.active.1 as f32,
            0.0,
            0.0,
        ];
        let ctx = &self.gpu.context;
        unsafe {
            ctx.UpdateSubresource(&self.params, 0, None, p.as_ptr() as *const _, 0, 0);
            ctx.CSSetShader(&self.shader, None);
            ctx.CSSetShaderResources(0, Some(&[Some(self.srv.as_ref().unwrap().1.clone())]));
            ctx.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.CSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            let uavs = [Some(out.y.clone()), Some(out.uv.clone())];
            ctx.CSSetUnorderedAccessViews(0, 2, Some(uavs.as_ptr()), None);
            ctx.Dispatch(self.width.div_ceil(16), self.height.div_ceil(16), 1);
            // Unbind so the encoder may read the texture.
            let none: [Option<ID3D11UnorderedAccessView>; 2] = [None, None];
            ctx.CSSetUnorderedAccessViews(0, 2, Some(none.as_ptr()), None);
            ctx.CSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

// The converter and its targets belong to the multithread-protected device (see `d3d`); the pacer
// thread owns them.
unsafe impl Send for Converter {}
unsafe impl Send for Nv12 {}
