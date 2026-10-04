//! NVENC driven directly through NVIDIA's Video Codec SDK interface (`nvEncodeAPI64.dll`, which
//! the display driver installs), instead of through Media Foundation's wrapper around it. rbuf uses
//! it where it matters most: NvFBC writes NV12 straight into surfaces NVENC has registered, so a
//! frame goes from the display to the encoder with one GPU pass and a handful of calls on the CPU.
//!
//! The SDK's structures are large, versioned and mostly reserved space, with unions and bitfields.
//! Rather than mirror them field by field, each one is a zeroed, 8-byte aligned buffer of the SDK's
//! size, and rbuf writes the fields it uses at their offsets. The sizes, offsets and bit positions
//! below were printed from `nvEncodeAPI.h` 13.1 by `scripts/nvenc_layout.c`.
//!
//! Encoding is asynchronous: every picture goes into a slot (its input surface and its output
//! buffer), the driver signals the slot's event when the picture is done, and a thread of its own
//! waits on those events in order, a kernel wait rather than polling, and hands the bitstream on.

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;

use windows::core::{s, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::encoder::{Encoded, RateControl, Settings};
use crate::mp4::VideoCodec;

const API_VERSION: u32 = 13 | 1 << 24;

const fn struct_version(v: u32) -> u32 {
    API_VERSION | v << 16 | 0x7 << 28
}

/// Structure sizes, versions and field offsets from `scripts/nvenc_layout.c`.
mod layout {
    use super::struct_version;

    pub const FUNCTION_LIST_SIZE: usize = 2552;
    pub const FUNCTION_LIST_VER: u32 = struct_version(2);

    pub const OPEN_SESSION_SIZE: usize = 1552;
    pub const OPEN_SESSION_VER: u32 = struct_version(1);
    pub const OPEN_SESSION_DEVICE_TYPE: usize = 4;
    pub const OPEN_SESSION_DEVICE: usize = 8;
    pub const OPEN_SESSION_API: usize = 24;

    pub const PRESET_CONFIG_SIZE: usize = 5128;
    pub const PRESET_CONFIG_VER: u32 = struct_version(5) | 1 << 31;
    pub const PRESET_CONFIG_CFG: usize = 8;
    const _: () = assert!(PRESET_CONFIG_CFG + CONFIG_SIZE <= PRESET_CONFIG_SIZE);

    pub const CONFIG_SIZE: usize = 3584;
    pub const CONFIG_VER: u32 = struct_version(9) | 1 << 31;
    pub const CONFIG_PROFILE: usize = 4;
    pub const CONFIG_GOP: usize = 20;
    pub const CONFIG_FRAME_INTERVAL_P: usize = 24;
    pub const CONFIG_RC: usize = 40;
    pub const CONFIG_CODEC: usize = 168;

    pub const RC_VER: u32 = struct_version(1);
    pub const RC_MODE: usize = 4;
    pub const RC_CONST_QP: usize = 8;
    pub const RC_AVG: usize = 20;
    pub const RC_MAX: usize = 24;
    pub const RC_VBV_SIZE: usize = 28;
    pub const RC_VBV_DELAY: usize = 32;
    pub const RC_MULTIPASS: usize = 100;

    pub const H264_REPEAT_SPSPPS: (usize, u32) = (1, 4);
    pub const H264_IDR_PERIOD: usize = 8;
    pub const H264_VUI: usize = 72;
    pub const HEVC_REPEAT_SPSPPS: (usize, u32) = (16, 7);
    pub const HEVC_IDR_PERIOD: usize = 20;
    pub const HEVC_VUI: usize = 64;
    pub const AV1_REPEAT_SEQ_HDR: (usize, u32) = (16, 5);
    pub const AV1_IDR_PERIOD: usize = 20;
    pub const AV1_COLOR_PRIMARIES: usize = 68;
    pub const AV1_TRANSFER: usize = 72;
    pub const AV1_MATRIX: usize = 76;
    pub const AV1_COLOR_RANGE: usize = 80;

    pub const VUI_SIGNAL_TYPE_PRESENT: usize = 8;
    pub const VUI_VIDEO_FORMAT: usize = 12;
    pub const VUI_FULL_RANGE: usize = 16;
    pub const VUI_COLOUR_PRESENT: usize = 20;
    pub const VUI_PRIMARIES: usize = 24;
    pub const VUI_TRANSFER: usize = 28;
    pub const VUI_MATRIX: usize = 32;

    pub const INIT_SIZE: usize = 1800;
    pub const INIT_VER: u32 = struct_version(7) | 1 << 31;
    pub const INIT_ENCODE_GUID: usize = 4;
    pub const INIT_PRESET_GUID: usize = 20;
    pub const INIT_WIDTH: usize = 36;
    pub const INIT_HEIGHT: usize = 40;
    pub const INIT_DAR_WIDTH: usize = 44;
    pub const INIT_DAR_HEIGHT: usize = 48;
    pub const INIT_FPS_NUM: usize = 52;
    pub const INIT_FPS_DEN: usize = 56;
    pub const INIT_ASYNC: usize = 60;
    pub const INIT_PTD: usize = 64;
    pub const INIT_CONFIG: usize = 88;
    pub const INIT_MAX_WIDTH: usize = 96;
    pub const INIT_MAX_HEIGHT: usize = 100;
    pub const INIT_TUNING: usize = 136;

    pub const BITSTREAM_SIZE: usize = 776;
    pub const BITSTREAM_VER: u32 = struct_version(1);
    pub const BITSTREAM_BUFFER: usize = 16;

    pub const EVENT_SIZE: usize = 1544;
    pub const EVENT_VER: u32 = struct_version(2);
    pub const EVENT_HANDLE: usize = 8;

    pub const REGISTER_SIZE: usize = 1536;
    pub const REGISTER_VER: u32 = struct_version(5);
    pub const REGISTER_TYPE: usize = 4;
    pub const REGISTER_WIDTH: usize = 8;
    pub const REGISTER_HEIGHT: usize = 12;
    pub const REGISTER_RESOURCE: usize = 24;
    pub const REGISTER_REGISTERED: usize = 32;
    pub const REGISTER_FORMAT: usize = 40;
    pub const REGISTER_USAGE: usize = 44;

    pub const MAP_SIZE: usize = 1544;
    pub const MAP_VER: u32 = struct_version(4);
    pub const MAP_REGISTERED: usize = 16;
    pub const MAP_MAPPED: usize = 24;
    pub const MAP_FORMAT: usize = 32;

    pub const PIC_SIZE: usize = 3360;
    pub const PIC_VER: u32 = struct_version(7) | 1 << 31;
    pub const PIC_WIDTH: usize = 4;
    pub const PIC_HEIGHT: usize = 8;
    pub const PIC_FLAGS: usize = 16;
    pub const PIC_FRAME_IDX: usize = 20;
    pub const PIC_TIMESTAMP: usize = 24;
    pub const PIC_DURATION: usize = 32;
    pub const PIC_INPUT: usize = 40;
    pub const PIC_OUTPUT: usize = 48;
    pub const PIC_EVENT: usize = 56;
    pub const PIC_FORMAT: usize = 64;
    pub const PIC_STRUCT: usize = 68;

    pub const LOCK_SIZE: usize = 1544;
    pub const LOCK_VER: u32 = struct_version(2) | 1 << 31;
    pub const LOCK_BITSTREAM: usize = 8;
    pub const LOCK_SIZE_BYTES: usize = 36;
    pub const LOCK_TIMESTAMP: usize = 40;
    pub const LOCK_DATA: usize = 56;
    pub const LOCK_PICTURE_TYPE: usize = 64;
}
use layout::*;

const BUFFER_FORMAT_NV12: u32 = 0x1;
const PIC_FLAG_FORCEIDR: u32 = 0x2;
const PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
const PIC_STRUCT_FRAME: u32 = 0x1;
const PIC_TYPE_I: u32 = 2;
const PIC_TYPE_IDR: u32 = 3;
const RC_CONSTQP: u32 = 0;
const RC_VBR: u32 = 1;
const RC_CBR: u32 = 2;
const TUNING_HIGH_QUALITY: u32 = 1;
const DEVICE_DIRECTX: u32 = 0;
const RESOURCE_DIRECTX: u32 = 0;
const USAGE_INPUT_IMAGE: u32 = 0;

const CODEC_H264: GUID = GUID::from_values(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
const CODEC_HEVC: GUID = GUID::from_values(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
const CODEC_AV1: GUID = GUID::from_values(0x0a352289, 0x0aa7, 0x4759, [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54]);
const PROFILE_H264_HIGH: GUID = GUID::from_values(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
const PROFILE_HEVC_MAIN: GUID = GUID::from_values(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
const PROFILE_AV1_MAIN: GUID = GUID::from_values(0x5f2a39f5, 0xf14e, 0x4f95, [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97]);
/// P4, the middle of NVENC's seven speed/quality presets, which ShadowPlay also uses.
const PRESET_P4: GUID = GUID::from_values(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]);

/// A zeroed, 8-byte aligned SDK structure with its version in the first word.
struct Blob(Vec<u64>);

impl Blob {
    fn new(size: usize, version: u32) -> Blob {
        let mut b = Blob(vec![0; size.div_ceil(8)]);
        b.u32(0, version);
        b
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: a Vec<u64> is valid as bytes of the same total length.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), self.0.len() * 8) }
    }

    fn u32(&mut self, off: usize, v: u32) {
        self.bytes()[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, off: usize, v: u64) {
        self.bytes()[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn ptr(&mut self, off: usize, p: *const c_void) {
        self.u64(off, p as u64);
    }

    fn guid(&mut self, off: usize, g: &GUID) {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&g.data1.to_le_bytes());
        b[4..6].copy_from_slice(&g.data2.to_le_bytes());
        b[6..8].copy_from_slice(&g.data3.to_le_bytes());
        b[8..].copy_from_slice(&g.data4);
        self.bytes()[off..off + 16].copy_from_slice(&b);
    }

    fn set_bit(&mut self, off: usize, bit: u32) {
        self.bytes()[off] |= 1 << bit;
    }

    fn get_u32(&mut self, off: usize) -> u32 {
        u32::from_le_bytes(self.bytes()[off..off + 4].try_into().unwrap())
    }

    fn get_u64(&mut self, off: usize) -> u64 {
        u64::from_le_bytes(self.bytes()[off..off + 8].try_into().unwrap())
    }

    fn get_ptr(&mut self, off: usize) -> *mut c_void {
        self.get_u64(off) as *mut c_void
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }
}

type Status = i32;
type Enc = *mut c_void;

/// The entry points rbuf uses, from `NV_ENCODE_API_FUNCTION_LIST`.
#[derive(Clone, Copy)]
struct Api {
    initialize: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    create_bitstream: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    destroy_bitstream: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    encode: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    lock: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    unlock: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    register_event: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    unregister_event: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    map: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    unmap: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    destroy: unsafe extern "system" fn(Enc) -> Status,
    open: unsafe extern "system" fn(*mut c_void, *mut Enc) -> Status,
    register: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    unregister: unsafe extern "system" fn(Enc, *mut c_void) -> Status,
    last_error: unsafe extern "system" fn(Enc) -> *const std::ffi::c_char,
    preset_config: unsafe extern "system" fn(Enc, GUID, GUID, u32, *mut c_void) -> Status,
}

/// Position of an entry point in the function list: after the two header words, in the order
/// the SDK declares them (`nvEncOpenEncodeSessionEx` is the 30th, at byte 240).
const fn slot(i: usize) -> usize {
    8 + i * 8
}
const _: () = assert!(slot(29) == 240 && slot(39) == 320, "nvEncOpenEncodeSessionEx, nvEncGetEncodePresetConfigEx");

/// A function pointer from the list, as the type of the field it goes into.
unsafe fn entry<F: Copy>(p: *mut c_void) -> F {
    const { assert!(std::mem::size_of::<F>() == std::mem::size_of::<*mut c_void>()) };
    std::mem::transmute_copy(&p)
}

fn load() -> Result<Api, String> {
    unsafe {
        let m = LoadLibraryA(s!("nvEncodeAPI64.dll")).map_err(|_| "nvEncodeAPI64.dll not found (NVIDIA driver required)")?;
        let max: unsafe extern "system" fn(*mut u32) -> Status = std::mem::transmute(
            GetProcAddress(m, s!("NvEncodeAPIGetMaxSupportedVersion")).ok_or("NvEncodeAPIGetMaxSupportedVersion is missing")?,
        );
        let mut v = 0;
        max(&mut v);
        if v < (13 << 4 | 1) {
            return Err(format!("the driver's NVENC API is {}.{}; 13.1 or later is needed", v >> 4, v & 15));
        }
        let create: unsafe extern "system" fn(*mut c_void) -> Status = std::mem::transmute(
            GetProcAddress(m, s!("NvEncodeAPICreateInstance")).ok_or("NvEncodeAPICreateInstance is missing")?,
        );
        let mut list = Blob::new(FUNCTION_LIST_SIZE, FUNCTION_LIST_VER);
        let r = create(list.as_mut_ptr());
        if r != 0 {
            return Err(format!("NvEncodeAPICreateInstance failed ({r})"));
        }
        let mut f = |i: usize| -> Result<*mut c_void, String> {
            let p = list.get_ptr(slot(i));
            if p.is_null() {
                Err(format!("NVENC entry point {i} is missing"))
            } else {
                Ok(p)
            }
        };
        Ok(Api {
            initialize: entry(f(11)?),
            create_bitstream: entry(f(14)?),
            destroy_bitstream: entry(f(15)?),
            encode: entry(f(16)?),
            lock: entry(f(17)?),
            unlock: entry(f(18)?),
            register_event: entry(f(23)?),
            unregister_event: entry(f(24)?),
            map: entry(f(25)?),
            unmap: entry(f(26)?),
            destroy: entry(f(27)?),
            open: entry(f(29)?),
            register: entry(f(30)?),
            unregister: entry(f(31)?),
            last_error: entry(f(37)?),
            preset_config: entry(f(39)?),
        })
    }
}

/// The colour matrix of the NV12 frames (always limited range, BT.709 primaries and transfer),
/// written into the bitstream so players convert back with the same one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt709,
    Bt601,
}

/// An open encoder and its slots, shared by the submitting side and the output thread; the last
/// one to let go closes the session.
struct Session {
    api: Api,
    enc: Enc,
    registered: Vec<*mut c_void>,
    bitstreams: Vec<*mut c_void>,
    events: Vec<HANDLE>,
}
// The SDK allows encoding on one thread while another locks finished bitstreams, which is the
// only concurrent use here.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    fn error(&self, what: &str, status: Status) -> String {
        let detail = unsafe {
            let p = (self.api.last_error)(self.enc);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        if detail.is_empty() {
            format!("NVENC {what} failed ({status})")
        } else {
            format!("NVENC {what} failed ({status}): {detail}")
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            for &e in &self.events {
                let mut p = Blob::new(EVENT_SIZE, EVENT_VER);
                p.ptr(EVENT_HANDLE, e.0);
                (self.api.unregister_event)(self.enc, p.as_mut_ptr());
                let _ = CloseHandle(e);
            }
            for &b in &self.bitstreams {
                (self.api.destroy_bitstream)(self.enc, b);
            }
            for &r in &self.registered {
                (self.api.unregister)(self.enc, r);
            }
            (self.api.destroy)(self.enc);
        }
    }
}

/// A picture handed to the output thread: its slot and the mapped input to release.
struct Pending {
    slot: usize,
    mapped: usize,
}

/// The submitting side of an encoder: take a free slot, fill its surface, encode it.
pub struct Nvenc {
    session: Arc<Session>,
    free: Receiver<usize>,
    free_tx: Sender<usize>,
    pending: Option<SyncSender<Pending>>,
    size: (u32, u32),
    frame: u32,
    output: Option<std::thread::JoinHandle<()>>,
}

// Owned by one thread at a time (the capture or pacer thread).
unsafe impl Send for Nvenc {}

impl Nvenc {
    /// Opens an encoder on `device` (a Direct3D 9 or 11 device) for NV12 `surfaces` of the
    /// output size; slot `i` encodes `surfaces[i]`. Pictures come out on `out` in order.
    pub fn new(
        device: *mut c_void,
        surfaces: &[*mut c_void],
        s: &Settings,
        matrix: Matrix,
        out: Sender<Encoded>,
    ) -> Result<Nvenc, String> {
        let api = load()?;
        let mut enc: Enc = std::ptr::null_mut();
        let mut open = Blob::new(OPEN_SESSION_SIZE, OPEN_SESSION_VER);
        open.u32(OPEN_SESSION_DEVICE_TYPE, DEVICE_DIRECTX);
        open.ptr(OPEN_SESSION_DEVICE, device);
        open.u32(OPEN_SESSION_API, API_VERSION);
        let r = unsafe { (api.open)(open.as_mut_ptr(), &mut enc) };
        if r != 0 || enc.is_null() {
            return Err(format!("NVENC: opening a session failed ({r})"));
        }
        let mut session = Session { api, enc, registered: Vec::new(), bitstreams: Vec::new(), events: Vec::new() };
        configure(&session, s, matrix)?;
        for &surface in surfaces {
            let mut p = Blob::new(REGISTER_SIZE, REGISTER_VER);
            p.u32(REGISTER_TYPE, RESOURCE_DIRECTX);
            p.u32(REGISTER_WIDTH, s.width);
            p.u32(REGISTER_HEIGHT, s.height);
            p.ptr(REGISTER_RESOURCE, surface);
            p.u32(REGISTER_FORMAT, BUFFER_FORMAT_NV12);
            p.u32(REGISTER_USAGE, USAGE_INPUT_IMAGE);
            let r = unsafe { (api.register)(enc, p.as_mut_ptr()) };
            if r != 0 {
                return Err(session.error("registering a surface", r));
            }
            session.registered.push(p.get_ptr(REGISTER_REGISTERED));

            let mut b = Blob::new(BITSTREAM_SIZE, BITSTREAM_VER);
            let r = unsafe { (api.create_bitstream)(enc, b.as_mut_ptr()) };
            if r != 0 {
                return Err(session.error("creating an output buffer", r));
            }
            session.bitstreams.push(b.get_ptr(BITSTREAM_BUFFER));

            let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| format!("event: {}", e.message()))?;
            session.events.push(event);
            let mut p = Blob::new(EVENT_SIZE, EVENT_VER);
            p.ptr(EVENT_HANDLE, event.0);
            let r = unsafe { (api.register_event)(enc, p.as_mut_ptr()) };
            if r != 0 {
                return Err(session.error("registering an event", r));
            }
        }
        let session = Arc::new(session);
        let (free_tx, free) = mpsc::channel();
        for i in 0..surfaces.len() {
            let _ = free_tx.send(i);
        }
        let (pending_tx, pending_rx) = mpsc::sync_channel::<Pending>(surfaces.len());
        let output = {
            let (session, free_tx) = (session.clone(), free_tx.clone());
            std::thread::spawn(move || drain(&session, pending_rx, free_tx, out))
        };
        Ok(Nvenc { session, free, free_tx, pending: Some(pending_tx), size: (s.width, s.height), frame: 0, output: Some(output) })
    }

    /// A slot whose surface may be written, waiting up to `timeout` for the encoder to finish one.
    pub fn slot(&self, timeout: std::time::Duration) -> Option<usize> {
        self.free.recv_timeout(timeout).ok()
    }

    /// Gives back a slot that was taken but not encoded.
    pub fn release(&self, slot: usize) {
        let _ = self.free_tx.send(slot);
    }

    /// Encodes slot `slot`'s surface as the picture at `pts` (100 ns ticks); `key` asks for an IDR
    /// picture with the parameter sets in front. The slot comes back free once it is encoded.
    pub fn encode(&mut self, slot: usize, pts: i64, duration: i64, key: bool) -> Result<(), String> {
        let s = &*self.session;
        let mut m = Blob::new(MAP_SIZE, MAP_VER);
        m.ptr(MAP_REGISTERED, s.registered[slot]);
        let r = unsafe { (s.api.map)(s.enc, m.as_mut_ptr()) };
        if r != 0 {
            self.release(slot);
            return Err(s.error("mapping the input", r));
        }
        let mapped = m.get_ptr(MAP_MAPPED);
        let mut p = Blob::new(PIC_SIZE, PIC_VER);
        p.u32(PIC_WIDTH, self.size.0);
        p.u32(PIC_HEIGHT, self.size.1);
        p.u32(PIC_FLAGS, if key { PIC_FLAG_FORCEIDR | PIC_FLAG_OUTPUT_SPSPPS } else { 0 });
        p.u32(PIC_FRAME_IDX, self.frame);
        p.u64(PIC_TIMESTAMP, pts as u64);
        p.u64(PIC_DURATION, duration as u64);
        p.ptr(PIC_INPUT, mapped);
        p.ptr(PIC_OUTPUT, s.bitstreams[slot]);
        p.ptr(PIC_EVENT, s.events[slot].0);
        p.u32(PIC_FORMAT, m.get_u32(MAP_FORMAT));
        p.u32(PIC_STRUCT, PIC_STRUCT_FRAME);
        self.frame = self.frame.wrapping_add(1);
        let r = unsafe { (s.api.encode)(s.enc, p.as_mut_ptr()) };
        if r != 0 {
            unsafe { (s.api.unmap)(s.enc, mapped) };
            self.release(slot);
            return Err(s.error("encoding", r));
        }
        self.pending
            .as_ref()
            .unwrap()
            .send(Pending { slot, mapped: mapped as usize })
            .map_err(|_| "NVENC output thread ended".to_string())
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        // The output thread finishes the pictures in flight and ends; the session closes after.
        drop(self.pending.take());
        if let Some(t) = self.output.take() {
            let _ = t.join();
        }
    }
}

/// The output thread: waits for each picture in submission order, copies its bitstream out and
/// frees its slot.
fn drain(s: &Session, pending: Receiver<Pending>, free: Sender<usize>, out: Sender<Encoded>) {
    while let Ok(p) = pending.recv() {
        if unsafe { WaitForSingleObject(s.events[p.slot], 2000) } != WAIT_OBJECT_0 {
            eprintln!("rbuf: NVENC did not finish a picture in 2 s");
        }
        let mut l = Blob::new(LOCK_SIZE, LOCK_VER);
        l.ptr(LOCK_BITSTREAM, s.bitstreams[p.slot]);
        let r = unsafe { (s.api.lock)(s.enc, l.as_mut_ptr()) };
        let encoded = if r == 0 {
            let len = l.get_u32(LOCK_SIZE_BYTES) as usize;
            let data = unsafe { std::slice::from_raw_parts(l.get_ptr(LOCK_DATA) as *const u8, len) }.to_vec();
            let pts = l.get_u64(LOCK_TIMESTAMP) as i64;
            let kind = l.get_u32(LOCK_PICTURE_TYPE);
            unsafe { (s.api.unlock)(s.enc, s.bitstreams[p.slot]) };
            Some(Encoded { data, pts, key: kind == PIC_TYPE_IDR || kind == PIC_TYPE_I })
        } else {
            eprintln!("rbuf: {}", s.error("reading a picture", r));
            None
        };
        unsafe { (s.api.unmap)(s.enc, p.mapped as *mut c_void) };
        let _ = free.send(p.slot);
        if let Some(e) = encoded {
            if out.send(e).is_err() {
                break;
            }
        }
    }
}

/// Initializes the encoder from preset P4 with rbuf's settings on top.
fn configure(s: &Session, set: &Settings, matrix: Matrix) -> Result<(), String> {
    let (codec, profile) = match set.codec {
        VideoCodec::H264 => (CODEC_H264, PROFILE_H264_HIGH),
        VideoCodec::Hevc => (CODEC_HEVC, PROFILE_HEVC_MAIN),
        VideoCodec::Av1 => (CODEC_AV1, PROFILE_AV1_MAIN),
    };
    let mut preset = Blob::new(PRESET_CONFIG_SIZE, PRESET_CONFIG_VER);
    preset.u32(PRESET_CONFIG_CFG, CONFIG_VER);
    let r = unsafe { (s.api.preset_config)(s.enc, codec, PRESET_P4, TUNING_HIGH_QUALITY, preset.as_mut_ptr()) };
    if r != 0 {
        return Err(s.error("reading preset P4", r));
    }
    // The preset's NV_ENC_CONFIG, changed in place and passed to the initialization.
    let c = PRESET_CONFIG_CFG;
    preset.u32(c, CONFIG_VER);
    preset.guid(c + CONFIG_PROFILE, &profile);
    preset.u32(c + CONFIG_GOP, set.gop);
    // No B pictures, so pictures come out in the order they go in.
    preset.u32(c + CONFIG_FRAME_INTERVAL_P, 1);
    let rc = c + CONFIG_RC;
    preset.u32(rc, RC_VER);
    preset.u32(rc + RC_MULTIPASS, 0);
    preset.u32(rc + RC_VBV_SIZE, 0);
    preset.u32(rc + RC_VBV_DELAY, 0);
    match set.rate_control {
        RateControl::Cbr => {
            preset.u32(rc + RC_MODE, RC_CBR);
            preset.u32(rc + RC_AVG, set.bitrate);
            preset.u32(rc + RC_MAX, set.bitrate);
        }
        RateControl::Vbr => {
            preset.u32(rc + RC_MODE, RC_VBR);
            preset.u32(rc + RC_AVG, set.bitrate);
            preset.u32(rc + RC_MAX, set.bitrate / 2 * 3);
        }
        RateControl::Quality(q) => {
            // 0 to 100 onto the codec's quantizer range (best at 100), as a constant QP.
            let top = if set.codec == VideoCodec::Av1 { 255.0 } else { 51.0 };
            let qp = (top * (0.9 - 0.8 * q.min(100) as f64 / 100.0)).round() as u32;
            preset.u32(rc + RC_MODE, RC_CONSTQP);
            for i in 0..3 {
                preset.u32(rc + RC_CONST_QP + 4 * i, qp);
            }
        }
    }
    let cc = c + CONFIG_CODEC;
    let (primaries, transfer, coefficients) = match matrix {
        Matrix::Bt709 => (1, 1, 1),
        Matrix::Bt601 => (1, 1, 6),
    };
    let vui = |p: &mut Blob, at: usize| {
        p.u32(at + VUI_SIGNAL_TYPE_PRESENT, 1);
        p.u32(at + VUI_VIDEO_FORMAT, 5);
        p.u32(at + VUI_FULL_RANGE, 0);
        p.u32(at + VUI_COLOUR_PRESENT, 1);
        p.u32(at + VUI_PRIMARIES, primaries);
        p.u32(at + VUI_TRANSFER, transfer);
        p.u32(at + VUI_MATRIX, coefficients);
    };
    match set.codec {
        VideoCodec::H264 => {
            preset.set_bit(cc + H264_REPEAT_SPSPPS.0, H264_REPEAT_SPSPPS.1);
            preset.u32(cc + H264_IDR_PERIOD, set.gop);
            vui(&mut preset, cc + H264_VUI);
        }
        VideoCodec::Hevc => {
            preset.set_bit(cc + HEVC_REPEAT_SPSPPS.0, HEVC_REPEAT_SPSPPS.1);
            preset.u32(cc + HEVC_IDR_PERIOD, set.gop);
            vui(&mut preset, cc + HEVC_VUI);
        }
        VideoCodec::Av1 => {
            preset.set_bit(cc + AV1_REPEAT_SEQ_HDR.0, AV1_REPEAT_SEQ_HDR.1);
            preset.u32(cc + AV1_IDR_PERIOD, set.gop);
            preset.u32(cc + AV1_COLOR_PRIMARIES, primaries);
            preset.u32(cc + AV1_TRANSFER, transfer);
            preset.u32(cc + AV1_MATRIX, coefficients);
            preset.u32(cc + AV1_COLOR_RANGE, 0);
        }
    }

    let mut init = Blob::new(INIT_SIZE, INIT_VER);
    init.guid(INIT_ENCODE_GUID, &codec);
    init.guid(INIT_PRESET_GUID, &PRESET_P4);
    init.u32(INIT_WIDTH, set.width);
    init.u32(INIT_HEIGHT, set.height);
    init.u32(INIT_DAR_WIDTH, set.width);
    init.u32(INIT_DAR_HEIGHT, set.height);
    init.u32(INIT_FPS_NUM, set.fps);
    init.u32(INIT_FPS_DEN, 1);
    init.u32(INIT_ASYNC, 1);
    init.u32(INIT_PTD, 1);
    // SAFETY: the config lives in `preset`, which outlives the call.
    let cfg = unsafe { (preset.as_mut_ptr() as *mut u8).add(PRESET_CONFIG_CFG) };
    init.ptr(INIT_CONFIG, cfg as *const c_void);
    init.u32(INIT_MAX_WIDTH, set.width);
    init.u32(INIT_MAX_HEIGHT, set.height);
    init.u32(INIT_TUNING, TUNING_HIGH_QUALITY);
    let r = unsafe { (s.api.initialize)(s.enc, init.as_mut_ptr()) };
    if r != 0 {
        return Err(s.error("initialization", r));
    }
    Ok(())
}
