//! A stand-in for a game: a borderless full-screen window on the primary monitor that renders as
//! fast as it can through a flip-model swap chain with tearing allowed (the presentation mode
//! games use to get independent flip). After the given number of seconds it prints its frame rate
//! and its 1% low, so the cost of a capture method shows up as lost frames.
//!
//! Two loads: by default each frame only clears to a new colour, which makes per-frame overhead
//! (hooks, composition) visible; `heavy:N` draws a full-screen pixel shader with an N-step loop
//! per pixel instead, a GPU-bound game whose rate depends on how much GPU time capture takes.
//!
//! ```text
//! cargo run --release --example game -- 20                       # light load for 20 s
//! cargo run --release --example game -- 20 heavy:400 skip:8 trace
//! ```
//!
//! `exclusive` and `legacy` switch to exclusive full screen (flip model and blit model).
//! `skip:S` leaves the first S seconds out of the printed figures; `trace` also prints each
//! second's rate to stderr. `vsync` presents once per refresh, so every frame the display shows
//! is one colour whose red channel steps by one (mod 256): a test pattern for capture timing.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use windows::core::{s, w};
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
    use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_DRIVER_TYPE_HARDWARE};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, wp, lp) }
    }

    fn compile(src: &str, entry: windows::core::PCSTR, target: windows::core::PCSTR) -> ID3DBlob {
        let mut blob = None;
        let mut err: Option<ID3DBlob> = None;
        let r = unsafe {
            D3DCompile(
                src.as_ptr() as *const _,
                src.len(),
                None,
                None,
                None,
                entry,
                target,
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
            panic!("shader: {e} {msg}");
        }
        blob.unwrap()
    }

    const SHADER: &str = r#"
cbuffer C : register(b0) { float t; uint steps; float2 pad; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float2 uv = pos.xy / 1080.0;
    float3 c = float3(uv, 0.5);
    [loop] for (uint i = 0; i < steps; i++) {
        c = frac(sin(c.yzx * 12.9898 + c * 78.233 + t) * 0.5 + c * 1.0001);
    }
    return float4(c, 1);
}
"#;

    let mut secs = 10.0;
    let (mut trace, mut heavy, mut skip) = (false, 0u32, 0.0f64);
    // `exclusive`: DXGI exclusive full screen on a flip-model swap chain; `legacy`: the old
    // blit-model exclusive full screen (Legacy Flip once Windows' full-screen optimisations are
    // off for the program).
    let (mut exclusive, mut legacy, mut vsync) = (false, false, false);
    for (i, a) in std::env::args().skip(1).enumerate() {
        if i == 0 {
            secs = a.parse().unwrap_or(10.0);
        } else if a == "exclusive" {
            exclusive = true;
        } else if a == "legacy" {
            (exclusive, legacy) = (true, true);
        } else if a == "trace" {
            trace = true;
        } else if a == "vsync" {
            vsync = true;
        } else if let Some(n) = a.strip_prefix("heavy:") {
            heavy = n.parse().unwrap_or(400);
        } else if let Some(n) = a.strip_prefix("skip:") {
            skip = n.parse().unwrap_or(0.0);
        }
    }
    unsafe {
        let inst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: w!("rbuf-game"), ..Default::default() };
        RegisterClassW(&wc);
        let (w, h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            w!("rbuf-game"),
            w!("rbuf game"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            w,
            h,
            None,
            None,
            Some(inst.into()),
            None,
        )
        .unwrap();
        // In front with focus, as a game started from a launcher would be.
        let _ = SetForegroundWindow(hwnd);

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .unwrap();
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
        let factory: IDXGIFactory2 = CreateDXGIFactory1().unwrap();
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w as u32,
            Height: h as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: if legacy { 2 } else { 3 },
            SwapEffect: if legacy { DXGI_SWAP_EFFECT_DISCARD } else { DXGI_SWAP_EFFECT_FLIP_DISCARD },
            Flags: if exclusive { 0 } else { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 },
            ..Default::default()
        };
        let fs = DXGI_SWAP_CHAIN_FULLSCREEN_DESC { Windowed: false.into(), ..Default::default() };
        let swap = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, exclusive.then_some(&fs as *const _), None).unwrap();
        let back: ID3D11Texture2D = swap.GetBuffer(0).unwrap();
        let mut rtv = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv)).unwrap();
        let rtv = rtv.unwrap();
        drop(back);

        // The heavy load's pipeline.
        let draw = (heavy > 0).then(|| {
            let vsb = compile(SHADER, s!("vs"), s!("vs_5_0"));
            let psb = compile(SHADER, s!("ps"), s!("ps_5_0"));
            let bytes = |b: &ID3DBlob| std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
            let (mut vs, mut ps, mut cb) = (None, None, None);
            device.CreateVertexShader(bytes(&vsb), None, Some(&mut vs)).unwrap();
            device.CreatePixelShader(bytes(&psb), None, Some(&mut ps)).unwrap();
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            device.CreateBuffer(&bd, None, Some(&mut cb)).unwrap();
            (vs.unwrap(), ps.unwrap(), cb.unwrap())
        });
        let vp = D3D11_VIEWPORT { Width: w as f32, Height: h as f32, MaxDepth: 1.0, ..Default::default() };

        let start = std::time::Instant::now();
        let mut frames = 0u64;
        let (mut tick, mut tick_frames) = (std::time::Instant::now(), 0u64);
        let mut times: Vec<f32> = Vec::with_capacity(1 << 20);
        let mut last = std::time::Instant::now();
        let mut msg = MSG::default();
        let mut bad = std::collections::BTreeMap::new();
        while start.elapsed().as_secs_f64() < secs {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let t = (frames % 256) as f32 / 255.0;
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            match &draw {
                None => context.ClearRenderTargetView(&rtv, &[t, 0.3, 1.0 - t, 1.0]),
                Some((vs, ps, cb)) => {
                    let data: [u32; 4] = [(frames as f32 * 0.01).to_bits(), heavy, 0, 0];
                    context.UpdateSubresource(cb, 0, None, data.as_ptr() as *const _, 0, 0);
                    context.RSSetViewports(Some(&[vp]));
                    context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    context.VSSetShader(vs, None);
                    context.PSSetShader(ps, None);
                    context.PSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
                    context.Draw(3, 0);
                }
            }
            let hr = if vsync {
                swap.Present(1, DXGI_PRESENT(0))
            } else {
                swap.Present(0, if exclusive { DXGI_PRESENT(0) } else { DXGI_PRESENT_ALLOW_TEARING })
            };
            if hr.is_err() || hr.0 != 0 {
                *bad.entry(hr.0).or_insert(0u64) += 1;
            }
            frames += 1;
            let now = std::time::Instant::now();
            if start.elapsed().as_secs_f64() >= skip {
                times.push((now - last).as_secs_f32());
            }
            last = now;
            if trace && tick.elapsed().as_secs_f64() >= 1.0 {
                eprintln!("{:.0}", (frames - tick_frames) as f64 / tick.elapsed().as_secs_f64());
                (tick, tick_frames) = (std::time::Instant::now(), frames);
            }
        }
        // The average over the measured part, and the 1% low: the rate of the slowest 1% of frames.
        let total: f64 = times.iter().map(|&t| t as f64).sum();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let worst = &sorted[..(sorted.len() / 100).max(1)];
        let low = worst.len() as f64 / worst.iter().map(|&t| t as f64).sum::<f64>();
        println!("{:.0} fps, 1% low {:.0} fps", times.len() as f64 / total, low);
        if exclusive {
            let _ = swap.SetFullscreenState(false, None);
        }
        if !bad.is_empty() {
            eprintln!("Present results other than S_OK: {:?}", bad.iter().map(|(k, v)| (format!("0x{:08x}", *k as u32), v)).collect::<Vec<_>>());
        }
        let _ = DestroyWindow(hwnd);
    }
}
