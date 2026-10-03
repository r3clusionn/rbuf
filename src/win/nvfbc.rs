//! NVIDIA Frame Buffer Capture (NvFBC), the capture path ShadowPlay was built on: the driver
//! copies the display's frame buffer into CUDA memory with no Desktop Window Manager or DXGI
//! involvement. From there CUDA copies it into a Direct3D 11 texture that the rest of rbuf uses
//! like any other captured frame, so the picture never leaves GPU memory.
//!
//! The driver ships NvFBC as `NvFBC64.dll` (the Windows API of NVIDIA Capture SDK 7, structure
//! version 0x70). On GeForce cards `NvFBC_CreateEx` refuses unless the caller passes a
//! private-data key; the key below is the one NVIDIA's own GeForce software sends, as documented
//! by the open source nvidia-patch project. Nothing is patched or injected: rbuf calls the
//! driver's DLL like any NvFBC application, it just includes the key.
//!
//! Both DLLs (`NvFBC64.dll`, `nvcuda.dll`) are loaded at run time, so rbuf still starts on
//! machines without them and falls back to Windows Graphics Capture.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{s, Interface, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

use super::capture::Latest;
use super::clock;
use super::d3d::Gpu;

const DLL_VERSION: u32 = 0x70;

/// `NVFBC_STRUCT_VERSION`: the struct size, its version and the API version in one word.
const fn struct_version(size: usize, ver: u32) -> u32 {
    size as u32 | ver << 16 | DLL_VERSION << 24
}

/// The GeForce private-data key (see the module comment).
const KEY: [u32; 4] = [0xAEF5_7AC5, 0x401D_1A39, 0x1B85_6BBE, 0x9ED0_CEBA];

/// `NVFBC_SHARED_CUDA`: capture into CUDA device memory.
const INTERFACE_CUDA: u32 = 0x1007;
/// `NVFBC_TOCUDA_ARGB`: 8-bit B, G, R, A bytes, the layout of `DXGI_FORMAT_B8G8R8A8_UNORM`.
const FORMAT_ARGB: u32 = 0;
/// Setup flag: draw the hardware cursor into the frame.
const SETUP_WITH_CURSOR: u32 = 1;
/// Grab flag: wait for a new frame (the default; `NOWAIT` would copy stale frames).
const GRAB_WAIT: u32 = 0;

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
    private_data2: *const c_void,
    private_data2_size: u32,
    reserved: [u32; 55],
    reserved_ptrs: [*mut c_void; 27],
}
const _: () = assert!(std::mem::size_of::<CreateParams>() == 512);
const _: () = assert!(std::mem::offset_of!(CreateParams, object) == 40);

#[repr(C)]
struct CudaSetupParams {
    version: u32,
    flags: u32,
    cursor_event: *mut c_void,
    format: u32,
    reserved: [u32; 61],
    reserved_ptrs: [*mut c_void; 31],
}
const _: () = assert!(std::mem::size_of::<CudaSetupParams>() == 512);
const _: () = assert!(std::mem::offset_of!(CudaSetupParams, format) == 16);

#[repr(C)]
struct CudaGrabParams {
    version: u32,
    flags: u32,
    buffer: u64,
    info: *mut GrabInfo,
    wait_ms: u32,
    reserved: [u32; 61],
    reserved_ptrs: [*mut c_void; 30],
}
const _: () = assert!(std::mem::size_of::<CudaGrabParams>() == 512);
const _: () = assert!(std::mem::offset_of!(CudaGrabParams, wait_ms) == 24);

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

// Vtable slots of the CUDA capture object (`INvFBCCuda`), in declaration order.
const SLOT_MAX_BUFFER_SIZE: usize = 0;
const SLOT_SETUP: usize = 1;
const SLOT_GRAB: usize = 2;
const SLOT_RELEASE: usize = 5;

/// A readable NvFBC error.
fn nvfbc_error(code: i32) -> String {
    let name = match code {
        -1 => "generic error",
        -2 => "invalid parameter",
        -3 => "session invalidated (display mode change)",
        -4 => "protected content on screen",
        -5 => "driver failure",
        -6 => "CUDA failure",
        -7 => "unsupported",
        -9 => "incompatible driver",
        -10 => "unsupported platform",
        -13 => "incompatible struct version",
        -15 => "insufficient privileges",
        -18 => "invalid target (not an NVIDIA display)",
        -20 => "dynamically disabled by the driver",
        _ => "error",
    };
    format!("NvFBC {name} ({code})")
}

// ---- CUDA driver API, the few calls needed ---------------------------------------------------

type CuResult = i32;

#[repr(C)]
#[derive(Default)]
struct Memcpy2D {
    src_x_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: usize,
    src_device: u64,
    src_array: usize,
    src_pitch: usize,
    dst_x_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: usize,
    dst_device: u64,
    dst_array: usize,
    dst_pitch: usize,
    width_bytes: usize,
    height: usize,
}
const _: () = assert!(std::mem::size_of::<Memcpy2D>() == 128);
const CU_MEMORYTYPE_DEVICE: u32 = 2;
const CU_MEMORYTYPE_ARRAY: u32 = 3;

struct Cuda {
    ctx_pop: unsafe extern "system" fn(*mut *mut c_void) -> CuResult,
    ctx_push: unsafe extern "system" fn(*mut c_void) -> CuResult,
    mem_alloc: unsafe extern "system" fn(*mut u64, usize) -> CuResult,
    mem_free: unsafe extern "system" fn(u64) -> CuResult,
    register: unsafe extern "system" fn(*mut *mut c_void, *mut c_void, u32) -> CuResult,
    unregister: unsafe extern "system" fn(*mut c_void) -> CuResult,
    map: unsafe extern "system" fn(u32, *mut *mut c_void, *mut c_void) -> CuResult,
    unmap: unsafe extern "system" fn(u32, *mut *mut c_void, *mut c_void) -> CuResult,
    mapped_array: unsafe extern "system" fn(*mut usize, *mut c_void, u32, u32) -> CuResult,
    memcpy_2d: unsafe extern "system" fn(*const Memcpy2D) -> CuResult,
}

unsafe fn sym<T>(m: HMODULE, name: PCSTR) -> Result<T, String> {
    match GetProcAddress(m, name) {
        Some(f) => Ok(std::mem::transmute_copy(&f)),
        None => Err(format!("{} is missing", name.display())),
    }
}

impl Cuda {
    fn load() -> Result<Cuda, String> {
        unsafe {
            let m = LoadLibraryA(s!("nvcuda.dll")).map_err(|_| "nvcuda.dll not found (no NVIDIA driver)".to_string())?;
            // The `_v2` names are what cuda.h maps these calls to on 64-bit.
            Ok(Cuda {
                ctx_pop: sym(m, s!("cuCtxPopCurrent_v2"))?,
                ctx_push: sym(m, s!("cuCtxPushCurrent_v2"))?,
                mem_alloc: sym(m, s!("cuMemAlloc_v2"))?,
                mem_free: sym(m, s!("cuMemFree_v2"))?,
                register: sym(m, s!("cuGraphicsD3D11RegisterResource"))?,
                unregister: sym(m, s!("cuGraphicsUnregisterResource"))?,
                map: sym(m, s!("cuGraphicsMapResources"))?,
                unmap: sym(m, s!("cuGraphicsUnmapResources"))?,
                mapped_array: sym(m, s!("cuGraphicsSubResourceGetMappedArray"))?,
                memcpy_2d: sym(m, s!("cuMemcpy2D_v2"))?,
            })
        }
    }
}

fn cu(r: CuResult, what: &str) -> Result<(), String> {
    if r == 0 {
        Ok(())
    } else {
        Err(format!("CUDA {what} failed ({r})"))
    }
}

// ---- the session ------------------------------------------------------------------------------

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
        if std::env::var_os("RBUF_NVFBC_DEBUG").is_some() {
            eprintln!("status flags 0x{:x}, version 0x{:x}, adapter {}", st.flags, st.nvfbc_version, st.adapter_idx);
        }
        Ok(Status { sdk_version: sdk, possible_without_key: st.flags & 1 != 0 })
    }
}

/// A running capture. Frames go into `latest` as they arrive.
pub struct NvfbcCapture {
    pub size: (u32, u32),
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Raw pointers handed to the capture thread.
struct Session {
    object: *mut c_void,
    buffer: u64,
}
unsafe impl Send for Session {}

impl Session {
    unsafe fn slot<T>(&self, i: usize) -> T {
        let vtable = *(self.object as *const *const *const c_void);
        std::mem::transmute_copy(&*vtable.add(i))
    }
}

/// The texture CUDA writes into, registered once and reused.
struct Target {
    texture: ID3D11Texture2D,
    resource: *mut c_void,
}

impl NvfbcCapture {
    /// Captures display `adapter` (0 is the primary display). `latest` receives every frame.
    pub fn start(gpu: &Gpu, adapter: u32, cursor: bool, latest: Arc<Mutex<Latest>>) -> Result<NvfbcCapture, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(u32, u32), String>>();
        let (g, s2) = (gpu.clone(), stop.clone());
        // The session, its CUDA context and every CUDA call live on this one thread: a CUDA
        // context is current on one thread at a time.
        let thread = std::thread::spawn(move || {
            let setup = || -> Result<(Cuda, Session), String> {
                let cuda = Cuda::load()?;
                let m = load_nvfbc()?;
                let create: CreateEx = unsafe { sym(m, s!("NvFBC_CreateEx"))? };
                let mut p: CreateParams = unsafe { std::mem::zeroed() };
                p.version = struct_version(std::mem::size_of::<CreateParams>(), 2);
                p.interface_type = INTERFACE_CUDA;
                p.adapter_idx = adapter;
                p.private_data = KEY.as_ptr().cast();
                p.private_data_size = std::mem::size_of_val(&KEY) as u32;
                let r = unsafe { create(&mut p) };
                if r != 0 || p.object.is_null() {
                    return Err(format!("creating the session: {}", nvfbc_error(r)));
                }
                let mut session = Session { object: p.object, buffer: 0 };
                unsafe {
                    // NvFBC created a CUDA context and made it current here; keep it current.
                    let mut ctx = std::ptr::null_mut();
                    cu((cuda.ctx_pop)(&mut ctx), "context pop")?;
                    cu((cuda.ctx_push)(ctx), "context push")?;
                    let max_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = session.slot(SLOT_MAX_BUFFER_SIZE);
                    let mut bytes = 0u32;
                    let r = max_size(session.object, &mut bytes);
                    if r != 0 {
                        return Err(format!("buffer size: {}", nvfbc_error(r)));
                    }
                    cu((cuda.mem_alloc)(&mut session.buffer, bytes as usize), "allocation")?;
                    let setup_fn: unsafe extern "system" fn(*mut c_void, *mut CudaSetupParams) -> i32 = session.slot(SLOT_SETUP);
                    let mut sp: CudaSetupParams = std::mem::zeroed();
                    sp.version = struct_version(std::mem::size_of::<CudaSetupParams>(), 1);
                    sp.flags = if cursor { SETUP_WITH_CURSOR } else { 0 };
                    sp.format = FORMAT_ARGB;
                    let r = setup_fn(session.object, &mut sp);
                    if r != 0 {
                        return Err(format!("setup: {}", nvfbc_error(r)));
                    }
                }
                Ok((cuda, session))
            };
            let (cuda, session) = match setup() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            run(&g, &cuda, session, &latest, &s2, tx);
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

/// The capture loop: grab (waiting for a new frame), copy into the registered texture, publish.
fn run(
    gpu: &Gpu,
    cuda: &Cuda,
    session: Session,
    latest: &Mutex<Latest>,
    stop: &AtomicBool,
    ready: mpsc::Sender<Result<(u32, u32), String>>,
) {
    let grab: unsafe extern "system" fn(*mut c_void, *mut CudaGrabParams) -> i32 = unsafe { session.slot(SLOT_GRAB) };
    let mut target: Option<Target> = None;
    let mut ready = Some(ready);
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: plain integers; all zero is the documented initial state.
        let mut info: GrabInfo = unsafe { std::mem::zeroed() };
        let mut gp: CudaGrabParams = unsafe { std::mem::zeroed() };
        gp.version = struct_version(std::mem::size_of::<CudaGrabParams>(), 1);
        gp.flags = GRAB_WAIT;
        gp.buffer = session.buffer;
        gp.info = &mut info;
        let r = unsafe { grab(session.object, &mut gp) };
        let time = clock::now();
        if r != 0 {
            failures += 1;
            if let Some(tx) = ready.take() {
                let _ = tx.send(Err(format!("first frame: {}", nvfbc_error(r))));
                break;
            }
            if failures > 50 {
                eprintln!("rbuf: NvFBC capture stopped: {}", nvfbc_error(r));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        failures = 0;
        let (w, h) = (info.width, info.height);
        let pitch = info.buffer_width.max(w) as usize * 4;
        if w == 0 || h == 0 {
            continue;
        }
        let copied = (|| -> Result<ID3D11Texture2D, String> {
            if target.as_ref().map(|t| super::d3d::texture_size(&t.texture)) != Some((w, h)) {
                if let Some(t) = target.take() {
                    unsafe { (cuda.unregister)(t.resource) };
                }
                let texture = gpu
                    .texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE)
                    .map_err(|e| e.message().to_string())?;
                let mut resource = std::ptr::null_mut();
                cu(unsafe { (cuda.register)(&mut resource, texture.as_raw(), 0) }, "D3D11 registration")?;
                target = Some(Target { texture, resource });
            }
            let t = target.as_mut().unwrap();
            unsafe {
                cu((cuda.map)(1, &mut t.resource, std::ptr::null_mut()), "map")?;
                let mut array = 0usize;
                let r = (cuda.mapped_array)(&mut array, t.resource, 0, 0);
                let c = Memcpy2D {
                    src_memory_type: CU_MEMORYTYPE_DEVICE,
                    src_device: session.buffer,
                    src_pitch: pitch,
                    dst_memory_type: CU_MEMORYTYPE_ARRAY,
                    dst_array: array,
                    width_bytes: w as usize * 4,
                    height: h as usize,
                    ..Default::default()
                };
                let r2 = if r == 0 { (cuda.memcpy_2d)(&c) } else { r };
                // Unmapping orders the copy before any Direct3D use of the texture.
                cu((cuda.unmap)(1, &mut t.resource, std::ptr::null_mut()), "unmap")?;
                cu(r2, "copy")?;
            }
            Ok(t.texture.clone())
        })();
        match copied {
            Ok(tex) => {
                super::capture::copy_into(gpu, latest, &tex, (w, h), time);
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok((w, h)));
                }
            }
            Err(e) => {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Err(e));
                } else {
                    eprintln!("rbuf: NvFBC capture stopped: {e}");
                }
                break;
            }
        }
    }
    unsafe {
        if let Some(t) = target.take() {
            (cuda.unregister)(t.resource);
        }
        // Free the buffer before releasing the session; the session owns the CUDA context.
        (cuda.mem_free)(session.buffer);
        let release: unsafe extern "system" fn(*mut c_void) -> i32 = session.slot(SLOT_RELEASE);
        release(session.object);
    }
}

impl Drop for NvfbcCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // A grab waits for the next frame, which on a still screen may not come: give it a moment,
        // then let the process exit take the thread.
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
    for (name, kind) in [("to system memory", 0x1204u32), ("to CUDA", INTERFACE_CUDA)] {
        for keyed in [false, true] {
            for adapter in 0..2u32 {
                let mut p: CreateParams = unsafe { std::mem::zeroed() };
                p.version = struct_version(std::mem::size_of::<CreateParams>(), 2);
                p.interface_type = kind;
                p.adapter_idx = adapter;
                if keyed {
                    p.private_data = KEY.as_ptr().cast();
                    p.private_data_size = 16;
                }
                let r = unsafe { create(&mut p) };
                let verdict = if r == 0 && !p.object.is_null() {
                    let s = Session { object: p.object, buffer: 0 };
                    let slot = if kind == INTERFACE_CUDA { SLOT_RELEASE } else { 4 };
                    unsafe {
                        let release: unsafe extern "system" fn(*mut c_void) -> i32 = s.slot(slot);
                        release(s.object);
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
