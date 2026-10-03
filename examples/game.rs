//! A stand-in for a game: a borderless full-screen window on the primary monitor that renders as
//! fast as it can through a flip-model swap chain with tearing allowed (the presentation mode
//! games use to get independent flip). Each frame clears to a new colour. After the given number
//! of seconds it prints its frame rate, so the cost of a capture method shows up as lost frames.
//!
//! ```text
//! cargo run --release --example game -- 10
//! ```

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use windows::core::w;
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, wp, lp) }
    }

    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10.0);
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
            BufferCount: 3,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32,
            ..Default::default()
        };
        let swap = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None).unwrap();
        let back: ID3D11Texture2D = swap.GetBuffer(0).unwrap();
        let mut rtv = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv)).unwrap();
        let rtv = rtv.unwrap();
        drop(back);

        let start = std::time::Instant::now();
        let mut frames = 0u64;
        let mut msg = MSG::default();
        while start.elapsed().as_secs_f64() < secs {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let t = (frames % 256) as f32 / 255.0;
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            context.ClearRenderTargetView(&rtv, &[t, 0.3, 1.0 - t, 1.0]);
            let _ = swap.Present(0, DXGI_PRESENT_ALLOW_TEARING);
            frames += 1;
        }
        println!("{:.0} fps", frames as f64 / start.elapsed().as_secs_f64());
        let _ = DestroyWindow(hwnd);
    }
}
