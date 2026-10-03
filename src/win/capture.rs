//! Screen and window capture into a GPU texture: Windows Graphics Capture (monitors and windows,
//! the default) or DXGI Desktop Duplication (monitors). Either way the newest frame is copied into
//! one texture owned here, with its timestamp; the pacer reads it at the output frame rate. No
//! hooking into the captured program, so nothing an anti-cheat would see.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{factory, Interface, Result, BOOL};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput1, IDXGIOutputDuplication, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetForegroundWindow, GetWindowTextW, IsWindowVisible};

use super::clock;
use super::d3d::{texture_size, Gpu};

#[derive(Clone, Debug)]
pub struct Monitor {
    pub handle: isize,
    pub name: String,
    pub rect: (i32, i32, i32, i32),
    pub primary: bool,
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<Monitor>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let n = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
            list.push(Monitor {
                handle: m.0 as isize,
                name: String::from_utf16_lossy(&info.szDevice[..n]),
                rect: (r.left, r.top, r.right - r.left, r.bottom - r.top),
                primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
        true.into()
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// Visible top-level windows with a title: (handle, title).
pub fn windows() -> Vec<(isize, String)> {
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<(isize, String)>);
        if IsWindowVisible(h).as_bool() {
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(h, &mut buf);
            if n > 0 {
                list.push((h.0 as isize, String::from_utf16_lossy(&buf[..n as usize])));
            }
        }
        true.into()
    }
    let mut list: Vec<(isize, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Monitor by index in `monitors()`; `None` is the primary monitor.
    Monitor(Option<usize>),
    /// A window by handle.
    Window(isize),
    /// Whatever window is in the foreground when capture starts.
    Focused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Wgc,
    Dxgi,
}

/// The newest captured frame.
// The texture belongs to the multithread-protected device (see `d3d`).
unsafe impl Send for Latest {}

pub struct Latest {
    pub texture: Option<ID3D11Texture2D>,
    /// Size of the picture inside `texture` (a window can be smaller than the texture).
    pub content: (u32, u32),
    /// Capture time, 100 ns ticks of the performance counter.
    pub time: i64,
    /// Increases with every new frame.
    pub seq: u64,
}

pub struct Capture {
    pub latest: Arc<Mutex<Latest>>,
    pub size: (u32, u32),
    _wgc: Option<(GraphicsCaptureSession, Direct3D11CaptureFramePool, GraphicsCaptureItem)>,
    dxgi: Option<(Arc<std::sync::atomic::AtomicBool>, JoinHandle<()>)>,
}

fn copy_into(gpu: &Gpu, latest: &Mutex<Latest>, src: &ID3D11Texture2D, content: (u32, u32), time: i64) {
    let (w, h) = texture_size(src);
    let content = (content.0.min(w).max(1), content.1.min(h).max(1));
    let mut l = latest.lock().unwrap();
    let need_new = match &l.texture {
        Some(t) => texture_size(t) != (w, h),
        None => true,
    };
    if need_new {
        l.texture = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE).ok();
    }
    if let Some(dst) = &l.texture {
        let b = D3D11_BOX { left: 0, top: 0, front: 0, right: content.0, bottom: content.1, back: 1 };
        unsafe { gpu.context.CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&b)) };
        l.content = content;
        l.time = time;
        l.seq += 1;
    }
}

impl Capture {
    pub fn start(gpu: &Gpu, target: &Target, method: Method, cursor: bool) -> Result<Capture> {
        let latest = Arc::new(Mutex::new(Latest { texture: None, content: (0, 0), time: 0, seq: 0 }));
        let mons = monitors();
        let monitor = |i: Option<usize>| -> Result<HMONITOR> {
            let m = match i {
                Some(i) => mons.get(i),
                None => mons.iter().find(|m| m.primary).or(mons.first()),
            };
            m.map(|m| HMONITOR(m.handle as *mut _))
                .ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "no such monitor"))
        };
        if method == Method::Dxgi {
            let Target::Monitor(i) = target else {
                return Err(windows::core::Error::new(windows::core::HRESULT(-1), "desktop duplication captures monitors only"));
            };
            return Self::start_dxgi(gpu, monitor(*i)?, latest);
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe {
            match target {
                Target::Monitor(i) => interop.CreateForMonitor(monitor(*i)?)?,
                Target::Window(h) => interop.CreateForWindow(HWND(*h as *mut _))?,
                Target::Focused => interop.CreateForWindow(GetForegroundWindow())?,
            }
        };
        let size = item.Size()?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&gpu.dxgi_device()?)? };
        let d3d: IDirect3DDevice = inspectable.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
        let g = gpu.clone();
        let l2 = latest.clone();
        let device = AgileDevice(d3d.clone());
        let pool_size = Arc::new(Mutex::new(size));
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(move |p, _| {
            let Some(p) = p.as_ref() else { return Ok(()) };
            let d3d = device.get();
            let frame = p.TryGetNextFrame()?;
            let content = frame.ContentSize()?;
            let time = frame.SystemRelativeTime()?.Duration;
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            copy_into(&g, &l2, &tex, (content.Width as u32, content.Height as u32), time);
            // A window that changed size: give the pool buffers of the new size.
            let mut ps = pool_size.lock().unwrap();
            if content.Width != ps.Width || content.Height != ps.Height {
                *ps = content;
                p.Recreate(d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
            }
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        // Windows 11 can leave out the yellow border.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        Ok(Capture { latest, size: (size.Width as u32, size.Height as u32), _wgc: Some((session, pool, item)), dxgi: None })
    }

    fn start_dxgi(gpu: &Gpu, mon: HMONITOR, latest: Arc<Mutex<Latest>>) -> Result<Capture> {
        unsafe {
            let adapter = gpu.dxgi_device()?.GetAdapter()?;
            let mut i = 0;
            let output = loop {
                let o = adapter.EnumOutputs(i)?;
                if o.GetDesc()?.Monitor == mon {
                    break o;
                }
                i += 1;
            };
            let out1: IDXGIOutput1 = output.cast()?;
            let dup: IDXGIOutputDuplication = out1.DuplicateOutput(&gpu.device)?;
            let d = dup.GetDesc();
            let size = (d.ModeDesc.Width, d.ModeDesc.Height);
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (g, l2, s2) = (gpu.clone(), latest.clone(), stop.clone());
            let dup = SendDup(dup);
            let handle = std::thread::spawn(move || {
                let dup = dup;
                while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                    let mut res = None;
                    // A short wait: AcquireNextFrame holds the device lock while it waits, which
                    // would stall the converter on the same device.
                    match dup.0.AcquireNextFrame(1, &mut info, &mut res) {
                        Ok(()) => {
                            // A present time of 0 means only the mouse moved, except for the very first
                            // frame, which is the whole desktop and must be kept.
                            let first = l2.lock().unwrap().seq == 0;
                            if info.LastPresentTime != 0 || first {
                                if let Some(tex) = res.and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                                    let t = if info.LastPresentTime != 0 {
                                        clock::qpc_to_ticks(info.LastPresentTime)
                                    } else {
                                        clock::now()
                                    };
                                    copy_into(&g, &l2, &tex, size, t);
                                }
                            }
                            let _ = dup.0.ReleaseFrame();
                        }
                        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => std::thread::sleep(std::time::Duration::from_millis(1)),
                        Err(e) => {
                            eprintln!("rbuf: desktop duplication stopped: {e}");
                            break;
                        }
                    }
                }
            });
            Ok(Capture { latest, size, _wgc: None, dxgi: Some((stop, handle)) })
        }
    }
}

/// The WinRT Direct3D device wrapper is agile, so the free-threaded frame pool may use it from its
/// callback thread.
struct AgileDevice(IDirect3DDevice);
unsafe impl Send for AgileDevice {}
unsafe impl Sync for AgileDevice {}

impl AgileDevice {
    /// A method, so closures capture the whole (Send) wrapper rather than the field.
    fn get(&self) -> &IDirect3DDevice {
        &self.0
    }
}

struct SendDup(IDXGIOutputDuplication);
// The duplication interface is free-threaded; it is used by one thread at a time here.
unsafe impl Send for SendDup {}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some((session, pool, _)) = self._wgc.take() {
            let _ = session.Close();
            let _ = pool.Close();
        }
        if let Some((stop, h)) = self.dxgi.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = h.join();
        }
    }
}
