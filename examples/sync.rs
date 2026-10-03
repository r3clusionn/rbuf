//! Audio/video sync check: once a second the window turns white for 100 ms and a click plays,
//! both started in the same instant. Record it with rbuf, then compare when the flash and the
//! click appear in the file (`scripts/sync.py`). The difference includes Windows' audio output
//! latency and the display's, as a viewer of a real recording would see it.
//!
//! ```text
//! cargo run --release --example sync -- 12
//! ```

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    static WHITE: AtomicBool = AtomicBool::new(false);

    extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if m == WM_PAINT {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(h, &mut ps);
                let brush = GetStockObject(if WHITE.load(Ordering::Relaxed) { WHITE_BRUSH } else { BLACK_BRUSH });
                FillRect(dc, &RECT { left: 0, top: 0, right: 800, bottom: 600 }, HBRUSH(brush.0));
                let _ = EndPaint(h, &ps);
                return LRESULT(0);
            }
            DefWindowProcW(h, m, wp, lp)
        }
    }

    // A 10 ms 1 kHz click as a WAV file in memory.
    let rate = 48000u32;
    let n = (rate / 100) as usize;
    let mut wav = Vec::new();
    let data: Vec<u8> = (0..n)
        .flat_map(|i| (((i as f64 * 1000.0 * 2.0 * std::f64::consts::PI / rate as f64).sin() * 20000.0) as i16).to_le_bytes())
        .collect();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);

    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(12.0);
    unsafe {
        let inst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: w!("rbuf-sync"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let mut r = RECT { left: 0, top: 0, right: 800, bottom: 600 };
        let _ = AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("rbuf-sync"),
            w!("rbuf sync"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            80,
            80,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(inst.into()),
            None,
        )
        .unwrap();
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        println!("hwnd 0x{:x}", hwnd.0 as usize);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let start = std::time::Instant::now();
        let mut next_flash = 1.5f64;
        let mut msg = MSG::default();
        while start.elapsed().as_secs_f64() < secs {
            let t = start.elapsed().as_secs_f64();
            if t >= next_flash {
                WHITE.store(true, Ordering::Relaxed);
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
                let _ = PlaySoundW(windows::core::PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC);
                next_flash += 1.0;
            } else if WHITE.load(Ordering::Relaxed) && t >= next_flash - 0.9 {
                WHITE.store(false, Ordering::Relaxed);
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
