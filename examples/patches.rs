//! A window of eight large colour patches, for checking rbuf's colour conversion end to end:
//! record it, decode a frame, and compare the pixels with the colours drawn here.
//!
//! ```text
//! cargo run --release --example patches -- 10      # show it for 10 seconds
//! ```

/// The colours, left to right then top to bottom, as (r, g, b).
pub const PATCHES: [(u8, u8, u8); 8] =
    [(0, 0, 0), (255, 255, 255), (255, 0, 0), (0, 255, 0), (0, 0, 255), (128, 128, 128), (255, 255, 0), (40, 120, 200)];

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HGDIOBJ, PAINTSTRUCT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if m == WM_PAINT {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(h, &mut ps);
                for (i, (r, g, b)) in PATCHES.iter().enumerate() {
                    let (x, y) = ((i % 4) as i32 * 200, (i / 4) as i32 * 300);
                    let brush =
                        CreateSolidBrush(windows::Win32::Foundation::COLORREF(*r as u32 | (*g as u32) << 8 | (*b as u32) << 16));
                    FillRect(dc, &RECT { left: x, top: y, right: x + 200, bottom: y + 300 }, brush);
                    let _ = DeleteObject(HGDIOBJ(brush.0));
                }
                let _ = EndPaint(h, &ps);
                return LRESULT(0);
            }
            DefWindowProcW(h, m, wp, lp)
        }
    }

    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10.0);
    unsafe {
        let inst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: w!("rbuf-patches"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        // An 800x600 client area.
        let mut r = RECT { left: 0, top: 0, right: 800, bottom: 600 };
        let _ = AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("rbuf-patches"),
            w!("rbuf patches"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            60,
            60,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(inst.into()),
            None,
        )
        .unwrap();
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        // Where the client area sits inside what Windows Graphics Capture captures (the visible
        // frame, without the invisible resize borders).
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut vis = RECT::default();
        let _ = windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut vis as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        let mut origin = windows::Win32::Foundation::POINT::default();
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut origin);
        println!("hwnd 0x{:x} client {} {}", hwnd.0 as usize, origin.x - vis.left, origin.y - vis.top);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        let mut msg = MSG::default();
        while std::time::Instant::now() < end {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
