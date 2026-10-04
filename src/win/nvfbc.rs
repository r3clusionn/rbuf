//! NVIDIA Frame Buffer Capture (NvFBC), the capture path ShadowPlay is built on: the driver copies
//! the display's frame buffer, or the frames one process presents, into surfaces of the caller's
//! Direct3D 9 device, with no Desktop Window Manager or DXGI involvement. rbuf's surfaces are
//! Direct3D 11 textures shared with that device, so a grabbed frame is already where the rest of
//! rbuf (the NV12 conversion and the encoder) reads it. The picture never leaves GPU memory.
//!
//! The driver ships NvFBC as `NvFBC64.dll` (the Windows API of NVIDIA Capture SDK 7, structure
//! version 0x70). On GeForce cards `NvFBC_CreateEx` refuses unless the caller passes a
//! private-data key; the key below is the one NVIDIA's own GeForce software sends, as documented
//! by the open source nvidia-patch project. Nothing is patched or injected: rbuf calls the
//! driver's DLL like any NvFBC application, it just includes the key.
//!
//! Two things here are not in the public SDK and were read out of NvFBC64.dll (its log strings
//! name the fields; the checks in its code give the offsets): the per-process ("PID") capture
//! mode, and the v3 layout of the Direct3D 9 interface's setup parameters, the interface
//! ShadowPlay uses. The CUDA interface rbuf used before took twice the CPU time (6 to 7.5% of
//! one logical CPU against 2 to 3.5% for this one, measured with the same 60 Hz grab loop).
//!
//! NvFBC's own "wait for the next frame" grab spins a CPU core inside the driver. So rbuf paces
//! the grabs itself: it sleeps in `IDXGIOutput::WaitForVBlank`, a kernel wait, and then takes the
//! frame the driver already has with a non-blocking grab, exactly once per output frame.
//!
//! `NvFBC64.dll` is loaded at run time, so rbuf still starts on machines without it and falls
//! back to Windows Graphics Capture.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{s, Interface, PCSTR};
use windows::Win32::Foundation::{HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Direct3D9::{
    Direct3DCreate9Ex, IDirect3DDevice9Ex, IDirect3DQuery9, IDirect3DSurface9, IDirect3DTexture9,
    D3DCREATE_DISABLE_PSGP_THREADING, D3DCREATE_FPU_PRESERVE, D3DCREATE_HARDWARE_VERTEXPROCESSING, D3DCREATE_MULTITHREADED,
    D3DCREATE_NOWINDOWCHANGES, D3DDEVTYPE_HAL, D3DFMT_A8R8G8B8, D3DFMT_UNKNOWN, D3DFORMAT, D3DGETDATA_FLUSH, D3DISSUE_END,
    D3DPOOL_DEFAULT, D3DPRESENT_PARAMETERS, D3DQUERYTYPE_EVENT, D3DSWAPEFFECT_DISCARD, D3DUSAGE_RENDERTARGET, D3D_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIOutput, IDXGIResource};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

use super::capture::Latest;
use super::clock;
use super::d3d::Gpu;
use super::encoder::{Encoded, Settings};
use super::nvenc::{Matrix, Nvenc};

const DLL_VERSION: u32 = 0x70;

/// `NVFBC_STRUCT_VERSION`: the struct size, its version and the API version in one word.
const fn struct_version(size: usize, ver: u32) -> u32 {
    size as u32 | ver << 16 | DLL_VERSION << 24
}

/// The GeForce private-data key (see the module comment).
const KEY: [u32; 4] = [0xAEF5_7AC5, 0x401D_1A39, 0x1B85_6BBE, 0x9ED0_CEBA];

/// `NVFBC_TO_DX9_VID`: capture into the caller's Direct3D 9 surfaces.
const INTERFACE_DX9: u32 = 0x2003;
/// `NVFBC_TO_SYS`, only for `rbuf --nvfbc-status`.
const INTERFACE_SYS: u32 = 0x1204;
/// `NVFBC_TODX9VID_ARGB`: 8-bit B, G, R, A, the layout of `DXGI_FORMAT_B8G8R8A8_UNORM`.
const MODE_ARGB: u32 = 0;
/// `NVFBC_TODX9VID_NV12`: the driver converts to NV12 itself, into NV12 surfaces.
const MODE_NV12: u32 = 1;
/// Grab modes: the whole frame as it is, or scaled to the target size.
const GRAB_FULL: u32 = 0;
const GRAB_SCALE: u32 = 1;
/// Setup flag bit 0: draw the hardware cursor into the frame.
const SETUP_WITH_CURSOR: u32 = 0x1;
/// Grab flag `NVFBC_TODX9VID_NOWAIT`: return the newest frame at once (see the module comment).
const GRAB_NOWAIT: u32 = 0x1;
/// NvFBC registers at most three output surfaces.
const BUFFERS: usize = 3;

#[repr(C)]
struct CreateParams {
    version: u32,
    interface_type: u32,
    max_width: u32,
    max_height: u32,
    device: *mut c_void,
    private_data: *const c_void,
    private_data_size: u32,
    interface_version: u32,
    object: *mut c_void,
    adapter_idx: u32,
    nvfbc_version: u32,
    cuda_ctx: *mut c_void,
    /// `NvFBCCreateParamsPrivateData` (see `PidParams`).
    pid_data: *const c_void,
    pid_data_size: u32,
    reserved: [u32; 55],
    reserved_ptrs: [*mut c_void; 27],
}
const _: () = assert!(std::mem::size_of::<CreateParams>() == 512);
const _: () = assert!(std::mem::offset_of!(CreateParams, device) == 0x10 && std::mem::offset_of!(CreateParams, object) == 0x28);
const _: () =
    assert!(std::mem::offset_of!(CreateParams, pid_data) == 0x40 && std::mem::offset_of!(CreateParams, pid_data_size) == 0x48);

/// `NVFBC_TODX9VID_SETUP_PARAMS_V3`. Offsets from the checks in `NvFBCToDx9Vid_v3::NvFBCToDx9VidSetUp`.
#[repr(C)]
struct Dx9SetupParams {
    version: u32,
    /// Bit 0 hardware cursor, 1 stereo, 2 difference map, 3 separate cursor, 5 classification map.
    flags: u32,
    mode: u32,
    buffer_count: u32,
    diff_map_block_size: u32,
    stereo_format: u32,
    diff_map_size: u32,
    classification_map_size: u32,
    classification_stamp_width: u32,
    classification_stamp_height: u32,
    diff_maps: *mut c_void,
    classification_maps: *mut c_void,
    buffers: *const OutBuffer,
    cursor_event: *mut c_void,
    reserved: [u32; 46],
    reserved_ptrs: [*mut c_void; 32],
}
const _: () = assert!(std::mem::size_of::<Dx9SetupParams>() == 512);
const _: () =
    assert!(std::mem::offset_of!(Dx9SetupParams, buffer_count) == 0xc && std::mem::offset_of!(Dx9SetupParams, buffers) == 0x38);

/// `NVFBC_TODX9VID_OUT_BUF`: a surface per eye; only the first is used.
#[repr(C)]
struct OutBuffer {
    primary: *mut c_void,
    secondary: *mut c_void,
}

/// `NVFBC_TODX9VID_GRAB_FRAME_PARAMS`.
#[repr(C)]
struct Dx9GrabParams {
    version: u32,
    flags: u32,
    target_width: u32,
    target_height: u32,
    start_x: u32,
    start_y: u32,
    grab_mode: u32,
    buffer_index: u32,
    info: *mut GrabInfo,
    wait_ms: u32,
    reserved: [u32; 57],
    reserved_ptrs: [*mut c_void; 30],
}
const _: () = assert!(std::mem::size_of::<Dx9GrabParams>() == 512);
const _: () = assert!(std::mem::offset_of!(Dx9GrabParams, info) == 0x20);

/// `NvFBCFrameGrabInfo`. The driver writes it; it is padded here so a newer, larger layout still
/// lands inside it.
#[repr(C)]
#[derive(Clone, Copy)]
struct GrabInfo {
    width: u32,
    height: u32,
    buffer_width: u32,
    reserved: u32,
    overlay_active: i32,
    must_recreate: i32,
    first_buffer: i32,
    hw_mouse_visible: i32,
    protected_content: i32,
    driver_internal_error: u32,
    stereo: i32,
    igpu_capture: i32,
    source_pid: u32,
    reserved3: u32,
    flags: u32,
    wait_mode_used: u32,
    padding: [u32; 48],
}

// Vtable slots of the Direct3D 9 capture object (`INvFBCToDx9Vid`), found by following the
// object's vtable to the functions named in NvFBC64.dll's log strings.
const SLOT_SETUP: usize = 0;
const SLOT_GRAB: usize = 1;
const SLOT_RELEASE: usize = 3;
/// `INvFBCToSys`'s release, for `--nvfbc-status`.
const SLOT_SYS_RELEASE: usize = 4;

/// A readable NvFBC error.
fn nvfbc_error(code: i32) -> String {
    let name = match code {
        -1 => "generic error",
        -2 => "invalid parameter",
        -3 => "session invalidated (display mode change)",
        -4 => "protected content on screen",
        -5 => "driver failure",
        -7 => "unsupported",
        -9 => "incompatible driver",
        -10 => "unsupported platform",
        -12 => "invalid pointer",
        -13 => "incompatible struct version",
        -15 => "insufficient privileges",
        -18 => "invalid target (not an NVIDIA display)",
        -19 => "not set up",
        -20 => "dynamically disabled by the driver",
        _ => "error",
    };
    format!("NvFBC {name} ({code})")
}

unsafe fn sym<T>(m: HMODULE, name: PCSTR) -> Result<T, String> {
    match GetProcAddress(m, name) {
        Some(f) => Ok(std::mem::transmute_copy(&f)),
        None => Err(format!("{} is missing", name.display())),
    }
}

fn hr(e: windows::core::Error, what: &str) -> String {
    format!("{what}: {}", e.message())
}

/// What the driver reports before capture starts.
#[derive(Debug, Clone)]
pub struct Status {
    pub sdk_version: u32,
    /// Capture possible without the GeForce key (professional cards).
    pub possible_without_key: bool,
}

type CreateEx = unsafe extern "system" fn(*mut CreateParams) -> i32;

fn load_nvfbc() -> Result<HMODULE, String> {
    unsafe { LoadLibraryA(s!("NvFBC64.dll")) }.map_err(|_| "NvFBC64.dll not found (NVIDIA driver required)".to_string())
}

/// Asks the driver whether NvFBC is available, without starting a capture.
pub fn status() -> Result<Status, String> {
    #[repr(C)]
    struct StatusEx {
        version: u32,
        flags: u32,
        nvfbc_version: u32,
        adapter_idx: u32,
        private_data: *const c_void,
        private_data_size: u32,
        reserved: [u32; 59],
        reserved_ptrs: [*mut c_void; 31],
    }
    const _: () = assert!(std::mem::size_of::<StatusEx>() == 512);
    unsafe {
        let m = load_nvfbc()?;
        let get_version: unsafe extern "system" fn(*mut u32) -> i32 = sym(m, s!("NvFBC_GetSDKVersion"))?;
        let get_status: unsafe extern "system" fn(*mut StatusEx) -> i32 = sym(m, s!("NvFBC_GetStatusEx"))?;
        let mut sdk = 0;
        get_version(&mut sdk);
        let mut st: StatusEx = std::mem::zeroed();
        st.version = struct_version(std::mem::size_of::<StatusEx>(), 2);
        let r = get_status(&mut st);
        if r != 0 {
            return Err(nvfbc_error(r));
        }
        Ok(Status { sdk_version: sdk, possible_without_key: st.flags & 1 != 0 })
    }
}

/// What one NvFBC session captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A whole display, by NvFBC adapter ordinal.
    Display(u32),
    /// What one process presents, taken as it presents ("PID capture mode"): the game alone,
    /// without other windows, notifications or overlays, whether or not it has the focus.
    Process(u32),
}

/// `NvFBCCreateParamsPrivateData`, passed through `CreateParams::pid_data`. Not in the public
/// SDK: the field names come from NvFBC64.dll's log strings, the 200-byte size and the offsets
/// from the checks in its `NvFBCCore::InitCore`.
#[repr(C)]
struct PidParams {
    version: u32,
    capture_mode: u32,
    target_pid: u32,
    reserved: [u32; 47],
}
const _: () = assert!(std::mem::size_of::<PidParams>() == 200);
const CAPTURE_MODE_PID: u32 = 1;

/// A running capture. Frames go into `latest` as they arrive.
pub struct NvfbcCapture {
    pub size: (u32, u32),
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// The display's DXGI output, for vblank waits on the capture thread. DXGI objects are free-threaded.
pub struct VBlank(pub IDXGIOutput);
unsafe impl Send for VBlank {}

/// rbuf's Direct3D 9 device on the same GPU as its Direct3D 11 device, and the query that tells
/// when a grab has finished on the GPU.
struct Dx9 {
    device: IDirect3DDevice9Ex,
    done: IDirect3DQuery9,
}

impl Dx9 {
    fn new(luid: LUID) -> Result<Dx9, String> {
        unsafe {
            let d3d = Direct3DCreate9Ex(D3D_SDK_VERSION).map_err(|e| hr(e, "Direct3D 9"))?;
            let adapter = (0..d3d.GetAdapterCount())
                .find(|&i| {
                    let mut l = LUID::default();
                    d3d.GetAdapterLUID(i, &mut l).is_ok() && l.LowPart == luid.LowPart && l.HighPart == luid.HighPart
                })
                .unwrap_or(0);
            let mut pp = D3DPRESENT_PARAMETERS {
                BackBufferWidth: 1,
                BackBufferHeight: 1,
                BackBufferFormat: D3DFMT_UNKNOWN,
                BackBufferCount: 1,
                SwapEffect: D3DSWAPEFFECT_DISCARD,
                hDeviceWindow: GetDesktopWindow(),
                Windowed: true.into(),
                ..Default::default()
            };
            let mut device = None;
            d3d.CreateDeviceEx(
                adapter,
                D3DDEVTYPE_HAL,
                GetDesktopWindow(),
                // DISABLE_PSGP_THREADING: no driver worker thread. NVIDIA's Direct3D 9 driver
                // otherwise hands every call to a thread of its own that spins (a `pause` loop)
                // after each one waiting for the next: measured with xperf, that thread was 3 to
                // 6% of one logical CPU, most of rbuf's time; with the flag rbuf as a whole
                // took 1.8%.
                (D3DCREATE_HARDWARE_VERTEXPROCESSING
                    | D3DCREATE_FPU_PRESERVE
                    | D3DCREATE_MULTITHREADED
                    | D3DCREATE_NOWINDOWCHANGES
                    | D3DCREATE_DISABLE_PSGP_THREADING) as u32,
                &mut pp,
                std::ptr::null_mut(),
                &mut device,
            )
            .map_err(|e| hr(e, "Direct3D 9 device"))?;
            let device = device.ok_or("Direct3D 9 device: none returned")?;
            let done = device.CreateQuery(D3DQUERYTYPE_EVENT).map_err(|e| hr(e, "Direct3D 9 query"))?;
            Ok(Dx9 { device, done })
        }
    }

    /// Waits until the device's work so far (the grab) has finished on the GPU.
    fn finish(&self) {
        unsafe {
            if self.done.Issue(D3DISSUE_END).is_err() {
                return;
            }
            let get = Interface::vtable(&self.done).GetData;
            let end = std::time::Instant::now() + std::time::Duration::from_millis(100);
            // S_FALSE (1) until the GPU is past the query. Grabs are short, but behind a game
            // that fills the GPU they can wait some milliseconds: sleep rather than spin.
            while get(self.done.as_raw(), std::ptr::null_mut(), 0, D3DGETDATA_FLUSH).0 == 1 && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
        }
    }
}

/// A Direct3D 11 texture NvFBC writes into through Direct3D 9.
struct Buffer {
    texture: ID3D11Texture2D,
    /// Kept alive while NvFBC holds the surface.
    _dx9_texture: IDirect3DTexture9,
    surface: IDirect3DSurface9,
}

fn make_buffers(gpu: &Gpu, dx9: &Dx9, (w, h): (u32, u32)) -> Result<Vec<Buffer>, String> {
    (0..BUFFERS)
        .map(|_| unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
            };
            let mut texture = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)).map_err(|e| hr(e, "shared texture"))?;
            let texture = texture.ok_or("shared texture: none returned")?;
            let mut handle: HANDLE =
                texture.cast::<IDXGIResource>().and_then(|r| r.GetSharedHandle()).map_err(|e| hr(e, "shared handle"))?;
            let mut t9 = None;
            dx9.device
                .CreateTexture(w, h, 1, D3DUSAGE_RENDERTARGET as u32, D3DFMT_A8R8G8B8, D3DPOOL_DEFAULT, &mut t9, &mut handle)
                .map_err(|e| hr(e, "opening the shared texture in Direct3D 9"))?;
            let t9 = t9.ok_or("Direct3D 9 texture: none returned")?;
            let surface = t9.GetSurfaceLevel(0).map_err(|e| hr(e, "Direct3D 9 surface"))?;
            Ok(Buffer { texture, _dx9_texture: t9, surface })
        })
        .collect()
}

/// One NvFBC session.
struct Session {
    object: *mut c_void,
}
unsafe impl Send for Session {}

impl Session {
    unsafe fn slot<T>(&self, i: usize) -> T {
        let vtable = *(self.object as *const *const *const c_void);
        std::mem::transmute_copy(&*vtable.add(i))
    }

    fn open(create: CreateEx, dx9: &Dx9, source: Source) -> Result<Session, String> {
        let mut p: CreateParams = unsafe { std::mem::zeroed() };
        p.version = struct_version(std::mem::size_of::<CreateParams>(), 2);
        p.interface_type = INTERFACE_DX9;
        p.device = dx9.device.as_raw();
        p.private_data = KEY.as_ptr().cast();
        p.private_data_size = std::mem::size_of_val(&KEY) as u32;
        let mut pid: PidParams = unsafe { std::mem::zeroed() };
        match source {
            Source::Display(adapter) => p.adapter_idx = adapter,
            Source::Process(target) => {
                pid.version = struct_version(std::mem::size_of::<PidParams>(), 1);
                pid.capture_mode = CAPTURE_MODE_PID;
                pid.target_pid = target;
                p.pid_data = (&pid as *const PidParams).cast();
                p.pid_data_size = std::mem::size_of::<PidParams>() as u32;
            }
        }
        let r = unsafe { create(&mut p) };
        if r != 0 || p.object.is_null() {
            return Err(format!("creating the session: {}", nvfbc_error(r)));
        }
        Ok(Session { object: p.object })
    }

    /// Registers `buffers` as the grab targets (again after they are replaced).
    fn setup(&self, buffers: &[Buffer], cursor: bool) -> Result<(), String> {
        let surfaces: Vec<*mut c_void> = buffers.iter().map(|b| b.surface.as_raw()).collect();
        self.setup_surfaces(&surfaces, MODE_ARGB, cursor)
    }

    /// Registers Direct3D 9 surfaces of the format `mode` names as the grab targets.
    fn setup_surfaces(&self, surfaces: &[*mut c_void], mode: u32, cursor: bool) -> Result<(), String> {
        let out: Vec<OutBuffer> = surfaces.iter().map(|&s| OutBuffer { primary: s, secondary: std::ptr::null_mut() }).collect();
        let mut sp: Dx9SetupParams = unsafe { std::mem::zeroed() };
        sp.version = struct_version(std::mem::size_of::<Dx9SetupParams>(), 3);
        sp.flags = if cursor { SETUP_WITH_CURSOR } else { 0 };
        sp.mode = mode;
        sp.buffer_count = out.len() as u32;
        sp.buffers = out.as_ptr();
        let setup: unsafe extern "system" fn(*mut c_void, *mut Dx9SetupParams) -> i32 = unsafe { self.slot(SLOT_SETUP) };
        match unsafe { setup(self.object, &mut sp) } {
            0 => Ok(()),
            r => Err(format!("setup: {}", nvfbc_error(r))),
        }
    }

    fn grab(&self, index: usize, info: &mut GrabInfo) -> i32 {
        self.grab_scaled(index, None, info)
    }

    /// Grabs into buffer `index`, scaled to `target` when given.
    fn grab_scaled(&self, index: usize, target: Option<(u32, u32)>, info: &mut GrabInfo) -> i32 {
        let mut gp: Dx9GrabParams = unsafe { std::mem::zeroed() };
        gp.version = struct_version(std::mem::size_of::<Dx9GrabParams>(), 1);
        gp.flags = GRAB_NOWAIT;
        if let Some((w, h)) = target {
            gp.grab_mode = GRAB_SCALE;
            gp.target_width = w;
            gp.target_height = h;
        } else {
            gp.grab_mode = GRAB_FULL;
        }
        gp.buffer_index = index as u32;
        gp.info = info;
        let grab: unsafe extern "system" fn(*mut c_void, *mut Dx9GrabParams) -> i32 = unsafe { self.slot(SLOT_GRAB) };
        unsafe { grab(self.object, &mut gp) }
    }

    /// Ends the session (dropping it does the same).
    fn close(self) {}
}

impl Drop for Session {
    /// Releases the session, also on an early return, so NvFBC is free for the next client.
    fn drop(&mut self) {
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void) -> i32 = self.slot(SLOT_RELEASE);
            release(self.object);
        }
    }
}

/// The executable name of a process, for messages.
fn process_name(pid: u32) -> String {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return format!("pid {pid}") };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        let mut name = format!("pid {pid}");
        while ok {
            if e.th32ProcessID == pid {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                name = format!("{} (pid {pid})", String::from_utf16_lossy(&e.szExeFile[..n]));
                break;
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
        name
    }
}

fn describe(source: Source) -> String {
    match source {
        Source::Display(_) => "the display".into(),
        Source::Process(pid) => format!("{} as it presents", process_name(pid)),
    }
}

impl NvfbcCapture {
    /// Captures `source` at most `fps` times a second, once per vertical blank of `vblank` (or on
    /// a timer without it). `latest` receives every frame.
    pub fn start(
        gpu: &Gpu,
        source: Source,
        vblank: Option<VBlank>,
        fps: u32,
        cursor: bool,
        latest: Arc<Mutex<Latest>>,
    ) -> Result<NvfbcCapture, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(u32, u32), String>>();
        let (g, s2) = (gpu.clone(), stop.clone());
        // The first buffer size: the display's. A process's frames may be another size; the
        // buffers are made again when a frame does not fit.
        let first_size = vblank
            .as_ref()
            .and_then(|v| unsafe { v.0.GetDesc() }.ok())
            .map(|d| {
                let r = d.DesktopCoordinates;
                ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
            })
            .unwrap_or((1920, 1080));
        let thread = std::thread::spawn(move || {
            let load = || -> Result<(CreateEx, Dx9), String> {
                let m = load_nvfbc()?;
                Ok((unsafe { sym(m, s!("NvFBC_CreateEx"))? }, Dx9::new(g.luid)?))
            };
            let (create, dx9) = match load() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let pace = Pace::new(vblank, fps);
            run(&g, &dx9, create, source, cursor, first_size, &pace, &latest, &s2, tx);
        });
        match rx.recv() {
            Ok(Ok(size)) => Ok(NvfbcCapture { size, stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("NvFBC capture thread ended".into()),
        }
    }
}

/// When the capture thread grabs.
struct Pace {
    vblank: Option<VBlank>,
    /// The shortest time between grabs, in 100 ns ticks (one output frame).
    interval: i64,
}

impl Pace {
    fn new(vblank: Option<VBlank>, fps: u32) -> Pace {
        Pace { vblank, interval: 10_000_000 / fps.max(1) as i64 }
    }

    /// Waits for the first vertical blank at or after `due` (less an eighth of a frame for
    /// jitter), or without a usable output, until `due` itself; returns the time. Grabs fall on a
    /// grid of one per output frame, so a 240 Hz display recorded at 60 fps is grabbed on every
    /// fourth blank: each grab costs the game GPU time, so there are no spare ones. (Sleeping
    /// through the undue blanks on a timer instead saved no measurable CPU time and made some
    /// grabs late: Windows let the timer overshoot by up to 10 ms with a game in front.)
    fn wait(&self, due: i64) -> i64 {
        loop {
            let ok = match &self.vblank {
                Some(v) => unsafe { v.0.WaitForVBlank() }.is_ok(),
                None => false,
            };
            let now = clock::now();
            if !ok {
                if due > now {
                    std::thread::sleep(std::time::Duration::from_nanos((due - now) as u64 * 100));
                }
                return clock::now();
            }
            if now >= due - self.interval / 8 {
                return now;
            }
        }
    }
}

/// The capture loop: wait for the next grab time, grab into the next buffer, wait for the GPU to
/// finish it, publish.
#[allow(clippy::too_many_arguments)]
fn run(
    gpu: &Gpu,
    dx9: &Dx9,
    create: CreateEx,
    source: Source,
    cursor: bool,
    first_size: (u32, u32),
    pace: &Pace,
    latest: &Mutex<Latest>,
    stop: &AtomicBool,
    ready: mpsc::Sender<Result<(u32, u32), String>>,
) {
    let mut ready = Some(ready);
    let mut session: Option<Session> = None;
    let mut buffers: Vec<Buffer> = Vec::new();
    let mut size = first_size;
    let mut next = 0usize;
    let mut failures = 0;
    let mut due = clock::now();
    let (started, mut grabs) = (due, 0u64);
    while !stop.load(Ordering::Relaxed) {
        if session.is_none() {
            let opened = (|| -> Result<Session, String> {
                if buffers.is_empty() {
                    buffers = make_buffers(gpu, dx9, size)?;
                }
                let s = Session::open(create, dx9, source)?;
                if let Err(e) = s.setup(&buffers, cursor) {
                    s.close();
                    return Err(e);
                }
                Ok(s)
            })();
            match opened {
                Ok(s) => {
                    if ready.is_none() {
                        eprintln!("rbuf: NvFBC is capturing {} again", describe(source));
                    }
                    session = Some(s);
                    failures = 0;
                }
                Err(e) => {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Err(e));
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }
        }
        let s = session.as_ref().unwrap();

        let last = pace.wait(due);
        due += pace.interval;
        if last - due > pace.interval {
            // More than a frame behind (a stall): start the grid again from now.
            due = last + pace.interval;
        }
        // SAFETY: plain integers; all zero is the documented initial state.
        let mut info: GrabInfo = unsafe { std::mem::zeroed() };
        let r = s.grab(next, &mut info);
        grabs += 1;
        if r != 0 {
            failures += 1;
            if let Some(tx) = ready.take() {
                if failures >= 3 {
                    let _ = tx.send(Err(format!("first frame: {}", nvfbc_error(r))));
                    break;
                }
                ready = Some(tx);
                continue;
            }
            if failures > 50 {
                if matches!(source, Source::Process(_)) {
                    // The process stopped presenting (or exited): wait for it with a new session.
                    eprintln!("rbuf: NvFBC lost {} ({})", describe(source), nvfbc_error(r));
                    session.take().unwrap().close();
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                eprintln!("rbuf: NvFBC capture stopped: {}", nvfbc_error(r));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        failures = 0;
        let (w, h) = (info.width, info.height);
        if w == 0 || h == 0 {
            continue;
        }
        if w > size.0 || h > size.1 {
            // Larger than the buffers (a game at a higher resolution than the display): make them
            // again at the frame's size and register them; the next grab fills them.
            size = (w.max(size.0), h.max(size.1));
            match make_buffers(gpu, dx9, size).and_then(|b| s.setup(&b, cursor).map(|_| b)) {
                Ok(b) => buffers = b,
                Err(e) => {
                    eprintln!("rbuf: NvFBC: {e}");
                    session.take().unwrap().close();
                    buffers.clear();
                }
            }
            continue;
        }
        dx9.finish();
        {
            let mut l = latest.lock().unwrap();
            l.texture = Some(buffers[next].texture.clone());
            l.content = (w, h);
            l.time = last;
            l.seq += 1;
        }
        next = (next + 1) % buffers.len();
        if let Some(tx) = ready.take() {
            eprintln!("rbuf: NvFBC is capturing {}", describe(source));
            let _ = tx.send(Ok((w, h)));
        }
    }
    if std::env::var_os("RBUF_NVFBC_DEBUG").is_some() {
        let secs = (clock::now() - started) as f64 / 1e7;
        eprintln!("NvFBC: {grabs} grabs in {secs:.1} s ({:.1} per second)", grabs as f64 / secs);
    }
    if let Some(s) = session.take() {
        s.close();
    }
}

/// NvFBC capture straight into NVENC: the driver writes each grab as NV12 into a Direct3D 9
/// surface NVENC has registered, and the capture thread encodes it right there, on a grid of one
/// picture per output frame. No conversion pass, no copy, no pacer thread; this is the path
/// ShadowPlay takes (NvFBC to Direct3D 9, then NVENC).
pub struct NvfbcEncoder {
    /// The encoded size.
    pub size: (u32, u32),
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// What the direct path needs besides the capture source.
pub struct DirectOptions {
    pub fps: u32,
    pub cursor: bool,
    /// The output size; `None` takes the captured size.
    pub size: Option<(u32, u32)>,
    /// Encoder settings for the size the capture turns out to have.
    pub settings: Box<dyn Fn((u32, u32)) -> Settings + Send>,
    pub force_key: Arc<AtomicBool>,
    /// Encoded, repeated (always 0 here: every grab is a new frame), dropped.
    pub stats: Arc<[AtomicU64; 3]>,
    pub out: mpsc::Sender<Encoded>,
}

/// `MAKEFOURCC('N', 'V', '1', '2')`.
const D3DFMT_NV12: D3DFORMAT = D3DFORMAT(u32::from_le_bytes(*b"NV12"));

fn nv12_surfaces(dx9: &Dx9, (w, h): (u32, u32)) -> Result<Vec<IDirect3DSurface9>, String> {
    (0..BUFFERS)
        .map(|_| unsafe {
            let mut s = None;
            dx9.device
                .CreateOffscreenPlainSurface(w, h, D3DFMT_NV12, D3DPOOL_DEFAULT, &mut s, std::ptr::null_mut())
                .map_err(|e| hr(e, "NV12 surface"))?;
            s.ok_or_else(|| "NV12 surface: none returned".to_string())
        })
        .collect()
}

impl NvfbcEncoder {
    pub fn start(gpu: &Gpu, source: Source, vblank: Option<VBlank>, o: DirectOptions) -> Result<NvfbcEncoder, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(u32, u32), String>>();
        let (luid, s2) = (gpu.luid, stop.clone());
        let display = vblank
            .as_ref()
            .and_then(|v| unsafe { v.0.GetDesc() }.ok())
            .map(|d| {
                let r = d.DesktopCoordinates;
                ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
            })
            .unwrap_or((1920, 1080));
        let thread = std::thread::spawn(move || {
            let pace = Pace::new(vblank, o.fps);
            if let Err(e) = run_direct(luid, source, display, &pace, o, &s2, &tx) {
                let _ = tx.send(Err(e));
            }
        });
        match rx.recv() {
            Ok(Ok(size)) => Ok(NvfbcEncoder { size, stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("NvFBC capture thread ended".into()),
        }
    }
}

impl Drop for NvfbcEncoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The direct capture loop. Returns an error only before the first frame; later failures are
/// reported and handled in place.
fn run_direct(
    luid: LUID,
    source: Source,
    display: (u32, u32),
    pace: &Pace,
    o: DirectOptions,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(u32, u32), String>>,
) -> Result<(), String> {
    let m = load_nvfbc()?;
    let create: CreateEx = unsafe { sym(m, s!("NvFBC_CreateEx"))? };
    let dx9 = Dx9::new(luid)?;

    // The source size: the display's, or for a process the size of what it presents, read from
    // a first unscaled grab into surfaces of the display's size. (A scaled grab reports the
    // target size, so it cannot tell.) A source larger than those surfaces refuses the unscaled
    // grab; it is then scaled to the display's size.
    let mut session = Session::open(create, &dx9, source)?;
    let raw = |v: &[IDirect3DSurface9]| v.iter().map(|s| s.as_raw()).collect::<Vec<_>>();
    let mut surfaces = nv12_surfaces(&dx9, display)?;
    session.setup_surfaces(&raw(&surfaces), MODE_NV12, o.cursor)?;
    let mut info: GrabInfo = unsafe { std::mem::zeroed() };
    let mut source_size = None;
    let mut first = -1;
    for attempt in 0..50 {
        // Unscaled first; from the tenth failure on, scaled (a source larger than the display).
        let scale = (attempt >= 10).then_some(display);
        first = session.grab_scaled(0, scale, &mut info);
        if first == 0 && info.width > 0 {
            source_size = scale.is_none().then_some((info.width & !1, info.height & !1));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if first != 0 {
        return Err(format!("first frame: {}", nvfbc_error(first)));
    }
    let size = o.size.or(source_size).unwrap_or(display);
    if size != display {
        surfaces = nv12_surfaces(&dx9, size)?;
        session.setup_surfaces(&raw(&surfaces), MODE_NV12, o.cursor)?;
    }
    let settings = (o.settings)(size);
    let mut enc = Nvenc::new(dx9.device.as_raw(), &raw(&surfaces), &settings, Matrix::Bt601, o.out)?;
    let _ = ready.send(Ok(size));
    eprintln!("rbuf: NvFBC is capturing {} (NV12, straight into NVENC)", describe(source));

    let frame = |i: i64| i * 10_000_000 / o.fps.max(1) as i64;
    let (mut due, mut failures, mut grabs) = (clock::now(), 0, 0u64);
    let start = due;
    let mut last_index = -1i64;
    // Scale only when the source differs from the output (a process at another size, or -s).
    let mut target = (source_size != Some(size)).then_some(size);
    while !stop.load(Ordering::Relaxed) {
        let now = pace.wait(due);
        due += pace.interval;
        if now - due > pace.interval {
            due = now + pace.interval;
        }
        // The output frame this grab stands for: the grid point nearest to it.
        let index = ((now - start) as f64 / pace.interval as f64).round() as i64;
        let index = index.max(last_index + 1);
        let Some(slot) = enc.slot(std::time::Duration::ZERO) else {
            // All surfaces still being encoded: drop this frame rather than wait and fall behind.
            o.stats[2].fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let mut info: GrabInfo = unsafe { std::mem::zeroed() };
        let r = session.grab_scaled(slot, target, &mut info);
        grabs += 1;
        if r != 0 {
            enc.release(slot);
            failures += 1;
            if failures > 50 {
                eprintln!("rbuf: NvFBC lost {} ({}); trying again", describe(source), nvfbc_error(r));
                session.close();
                std::thread::sleep(std::time::Duration::from_secs(1));
                match Session::open(create, &dx9, source)
                    .and_then(|s| s.setup_surfaces(&raw(&surfaces), MODE_NV12, o.cursor).map(|_| s))
                {
                    Ok(s) => {
                        session = s;
                        failures = 0;
                        eprintln!("rbuf: NvFBC is capturing {} again", describe(source));
                    }
                    Err(e) => {
                        eprintln!("rbuf: NvFBC: {e}");
                        // Keep a session object to retry with: opening again is the retry.
                        session = loop {
                            if stop.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            if let Ok(s) = Session::open(create, &dx9, source)
                                .and_then(|s| s.setup_surfaces(&raw(&surfaces), MODE_NV12, o.cursor).map(|_| s))
                            {
                                break s;
                            }
                        };
                        failures = 0;
                    }
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            continue;
        }
        failures = 0;
        let src = (info.width & !1, info.height & !1);
        if target.is_none() && src.0 > 0 && src != size {
            // The source changed size (a game switching resolution): scale it to the output
            // size from now on, and grab this frame again.
            target = Some(size);
            enc.release(slot);
            continue;
        }
        last_index = index;
        let key = o.force_key.swap(false, Ordering::Relaxed);
        match enc.encode(slot, start + frame(index), frame(index + 1) - frame(index), key) {
            Ok(()) => {
                o.stats[0].fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("rbuf: {e}");
                o.stats[2].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if std::env::var_os("RBUF_NVFBC_DEBUG").is_some() {
        let secs = (clock::now() - start) as f64 / 1e7;
        eprintln!("NvFBC: {grabs} grabs in {secs:.1} s ({:.1} per second)", grabs as f64 / secs);
    }
    // The encoder first (it finishes what is in flight), then the capture session.
    drop(enc);
    session.close();
    Ok(())
}

impl Drop for NvfbcCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread checks the flag once per grab, so it ends within a frame; the wait below is a
        // safety margin, not the normal path.
        if let Some(t) = self.thread.take() {
            let end = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while !t.is_finished() && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

/// Tries to create a session of each interface type, with and without the key, and reports the
/// driver's answers. For `rbuf --nvfbc-status`.
pub fn probe() -> Vec<String> {
    let mut out = Vec::new();
    match status() {
        Ok(s) => out.push(format!(
            "NvFBC API version 0x{:x}; capture without the GeForce key: {}",
            s.sdk_version,
            if s.possible_without_key { "allowed" } else { "refused (GeForce)" }
        )),
        Err(e) => {
            out.push(e);
            return out;
        }
    }
    let Ok(m) = load_nvfbc() else { return out };
    let Ok(create) = (unsafe { sym::<CreateEx>(m, s!("NvFBC_CreateEx")) }) else {
        return out;
    };
    let dx9 = Dx9::new(LUID::default()).ok();
    for (name, kind) in [("to system memory", INTERFACE_SYS), ("to Direct3D 9", INTERFACE_DX9)] {
        for keyed in [false, true] {
            for adapter in 0..2u32 {
                let mut p: CreateParams = unsafe { std::mem::zeroed() };
                p.version = struct_version(std::mem::size_of::<CreateParams>(), 2);
                p.interface_type = kind;
                p.adapter_idx = adapter;
                if kind == INTERFACE_DX9 {
                    match &dx9 {
                        Some(d) => p.device = d.device.as_raw(),
                        None => continue,
                    }
                }
                if keyed {
                    p.private_data = KEY.as_ptr().cast();
                    p.private_data_size = 16;
                }
                let r = unsafe { create(&mut p) };
                let verdict = if r == 0 && !p.object.is_null() {
                    // Released by hand: the two interfaces keep Release in different slots.
                    unsafe {
                        let vtable = *(p.object as *const *const *const c_void);
                        let slot = if kind == INTERFACE_DX9 { SLOT_RELEASE } else { SLOT_SYS_RELEASE };
                        let release: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(*vtable.add(slot));
                        release(p.object);
                    }
                    format!("ok, up to {}x{}", p.max_width, p.max_height)
                } else {
                    nvfbc_error(r)
                };
                out.push(format!("  {name}, display {adapter}, {}: {verdict}", if keyed { "with key" } else { "no key" }));
            }
        }
    }
    out
}

/// Switches NvFBC on or off for the whole machine (`NvFBC_Enable`). Needs administrator rights;
/// the driver stores the setting and resets the display driver, so screens go black briefly.
pub fn enable(on: bool) -> Result<(), String> {
    unsafe {
        let m = load_nvfbc()?;
        let f: unsafe extern "system" fn(i32) -> i32 = sym(m, s!("NvFBC_Enable"))?;
        match f(on as i32) {
            0 => Ok(()),
            r => Err(nvfbc_error(r)),
        }
    }
}
