//! Audio capture with WASAPI, one track per source:
//!
//! * `default_output`: what the speakers play (loopback of the default render device),
//! * `default_input`: the default microphone,
//! * `app:NAME` or `app:PID`: one program and its child processes, through Windows' process
//!   loopback (Windows 10 2004 or later), the per-game track ShadowPlay calls "separate tracks".
//!
//! Every source is asked for 48 kHz 16-bit stereo and Windows converts. Timestamps come from the
//! buffer's performance-counter position, so audio lines up with video. When nothing plays,
//! loopback delivers no data at all; the gap is filled with silence so the track stays continuous.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{implement, Interface, Ref, Result, HRESULT};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Variant::{VARENUM, VT_BLOB};

use super::clock;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// AAC-LC at 192 kbit/s, the highest rate the Windows AAC encoder offers.
pub const AAC_BYTES_PER_SECOND: u32 = 24_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    DefaultOutput,
    DefaultInput,
    App { pid: u32, name: String },
}

impl Source {
    /// `default_output`, `default_input`, `app:game.exe` or `app:1234`.
    pub fn parse(s: &str) -> std::result::Result<Source, String> {
        match s {
            "default_output" => Ok(Source::DefaultOutput),
            "default_input" => Ok(Source::DefaultInput),
            _ => {
                let Some(app) = s.strip_prefix("app:") else {
                    return Err(format!("unknown audio source `{s}` (default_output, default_input, app:NAME or app:PID)"));
                };
                if let Ok(pid) = app.parse::<u32>() {
                    return Ok(Source::App { pid, name: format!("pid {pid}") });
                }
                let pid = find_process(app).ok_or_else(|| format!("no running process named `{app}`"))?;
                Ok(Source::App { pid, name: app.to_string() })
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Source::DefaultOutput => "Desktop audio".into(),
            Source::DefaultInput => "Microphone".into(),
            Source::App { name, .. } => name.clone(),
        }
    }
}

/// The process id of a running program, by executable name (with or without `.exe`).
pub fn find_process(name: &str) -> Option<u32> {
    let want = name.to_ascii_lowercase();
    let want_exe = if want.ends_with(".exe") { want.clone() } else { format!("{want}.exe") };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]).to_ascii_lowercase();
                if exe == want || exe == want_exe {
                    found = Some(e.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

fn wave_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CHANNELS as u32 * 2,
        nBlockAlign: CHANNELS * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

pub fn process_loopback_client(pid: u32) -> Result<IAudioClient> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // Not dropped: the windows crate's Drop for PROPVARIANT calls PropVariantClear, which would
        // CoTaskMemFree the blob, and the blob is `params` on this stack.
        let pv = ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VARENUM(VT_BLOB.0),
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                }),
            },
        });
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(std::sync::Mutex::new(Some(tx))).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| windows::core::Error::new(HRESULT(-1), "process loopback activation timed out"))?;
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        unk.unwrap().cast()
    }
}

fn open(source: &Source) -> Result<IAudioClient> {
    unsafe {
        let (client, loopback): (IAudioClient, bool) = match source {
            Source::App { pid, .. } => (process_loopback_client(*pid)?, true),
            _ => {
                let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let flow = if *source == Source::DefaultOutput { eRender } else { eCapture };
                let dev = e.GetDefaultAudioEndpoint(flow, eConsole)?;
                (dev.Activate(CLSCTX_ALL, None)?, *source == Source::DefaultOutput)
            }
        };
        let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        // Half a second of buffer; it is read every 10 ms.
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 5_000_000, 0, &wave_format(), None)?;
        Ok(client)
    }
}

/// Encoded AAC frames of one source.
pub struct AacFrame {
    pub pts: i64,
    pub data: Vec<u8>,
}

/// Windows' AAC encoder transform, used synchronously.
struct AacEncoder {
    t: IMFTransform,
    out_size: u32,
}

impl AacEncoder {
    fn new() -> Result<AacEncoder> {
        unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
            let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut n = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut ptr,
                &mut n,
            )?;
            let mut acts = Vec::new();
            for i in 0..n as usize {
                if let Some(a) = std::ptr::read(ptr.add(i)) {
                    acts.push(a);
                }
            }
            if !ptr.is_null() {
                CoTaskMemFree(Some(ptr as *const _));
            }
            let a = acts.first().ok_or_else(|| windows::core::Error::new(HRESULT(-1), "no AAC encoder on this machine"))?;
            let t: IMFTransform = a.ActivateObject()?;
            let common = |sub: &windows::core::GUID| -> Result<IMFMediaType> {
                let m = MFCreateMediaType()?;
                m.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                m.SetGUID(&MF_MT_SUBTYPE, sub)?;
                m.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                m.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
                m.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
                Ok(m)
            };
            let out = common(&MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC, what MP4 stores
            t.SetOutputType(0, &out, 0)?;
            let inp = common(&MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, CHANNELS as u32 * 2)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * CHANNELS as u32 * 2)?;
            t.SetInputType(0, &inp, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            let info = t.GetOutputStreamInfo(0)?;
            Ok(AacEncoder { t, out_size: info.cbSize.max(8192) })
        }
    }

    fn encode(&self, pcm: &[u8], pts: i64, out: &mut Vec<AacFrame>) -> Result<()> {
        unsafe {
            let buf = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), p, pcm.len());
            buf.Unlock()?;
            buf.SetCurrentLength(pcm.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(pts)?;
            s.SetSampleDuration((pcm.len() as i64 / 4) * 10_000_000 / RATE as i64)?;
            self.t.ProcessInput(0, &s, 0)?;
            loop {
                let ob = MFCreateMemoryBuffer(self.out_size)?;
                let os = MFCreateSample()?;
                os.AddBuffer(&ob)?;
                let mut db = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(os.clone())),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let r = self.t.ProcessOutput(0, &mut db, &mut status);
                drop(ManuallyDrop::take(&mut db[0].pSample));
                drop(ManuallyDrop::take(&mut db[0].pEvents));
                match r {
                    Ok(()) => {
                        let b = os.ConvertToContiguousBuffer()?;
                        let mut q = std::ptr::null_mut();
                        let mut len = 0u32;
                        b.Lock(&mut q, None, Some(&mut len))?;
                        let data = std::slice::from_raw_parts(q, len as usize).to_vec();
                        b.Unlock()?;
                        if !data.is_empty() {
                            out.push(AacFrame { pts: os.GetSampleTime().unwrap_or(pts), data });
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// A running capture of one source, sending AAC frames.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const FRAME_TICKS: f64 = 10_000_000.0 / RATE as f64;

impl AudioCapture {
    /// Starts capturing. Fails at once if the source cannot be opened.
    pub fn start(source: Source, out: Sender<AacFrame>) -> Result<AudioCapture> {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let setup = || -> Result<(IAudioClient, IAudioCaptureClient, AacEncoder)> {
                let client = open(&source)?;
                let cap: IAudioCaptureClient = unsafe { client.GetService()? };
                let enc = AacEncoder::new()?;
                unsafe { client.Start()? };
                Ok((client, cap, enc))
            };
            let (client, cap, enc) = match setup() {
                Ok(x) => {
                    let _ = ready_tx.send(Ok(()));
                    x
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // The sample-accurate time line of this track, in ticks; starts at the first data.
            let mut next: Option<f64> = None;
            let mut frames_out = Vec::new();
            let mut pending: Vec<u8> = Vec::new();
            let mut pending_pts = 0i64;
            // Feed the encoder in 1024-sample blocks with their exact start time.
            let mut push = |pcm: &[u8], pts: f64, pending: &mut Vec<u8>, pending_pts: &mut i64| {
                if pending.is_empty() {
                    *pending_pts = pts.round() as i64;
                }
                pending.extend_from_slice(pcm);
                while pending.len() >= 4096 {
                    let block: Vec<u8> = pending.drain(..4096).collect();
                    frames_out.clear();
                    if enc.encode(&block, *pending_pts, &mut frames_out).is_err() {
                        return;
                    }
                    for f in frames_out.drain(..) {
                        let _ = out.send(f);
                    }
                    *pending_pts += (1024.0 * FRAME_TICKS).round() as i64;
                }
            };
            while !s2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                loop {
                    let n = unsafe { cap.GetNextPacketSize() }.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    let mut qpc = 0u64;
                    if unsafe { cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }.is_err() {
                        break;
                    }
                    let bytes = frames as usize * 4;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    let pcm: Vec<u8> = if silent || data.is_null() {
                        vec![0; bytes]
                    } else {
                        unsafe { std::slice::from_raw_parts(data, bytes).to_vec() }
                    };
                    unsafe {
                        let _ = cap.ReleaseBuffer(frames);
                    }
                    let t = qpc as f64;
                    let expect = *next.get_or_insert(t);
                    // A gap of more than 20 ms (nothing was playing): fill it with silence.
                    if t - expect > 200_000.0 {
                        let gap = ((t - expect) / FRAME_TICKS) as usize;
                        push(&vec![0; gap * 4], expect, &mut pending, &mut pending_pts);
                        next = Some(expect + gap as f64 * FRAME_TICKS);
                    }
                    let at = next.unwrap();
                    push(&pcm, at, &mut pending, &mut pending_pts);
                    next = Some(at + frames as f64 * FRAME_TICKS);
                }
                // Loopback of a silent device delivers nothing: keep the track going up to 30 ms ago.
                if let Some(at) = next {
                    let now = clock::now() as f64 - 300_000.0;
                    if now - at > 200_000.0 {
                        let gap = ((now - at) / FRAME_TICKS) as usize;
                        push(&vec![0; gap * 4], at, &mut pending, &mut pending_pts);
                        next = Some(at + gap as f64 * FRAME_TICKS);
                    }
                } else if source != Source::DefaultInput {
                    // Nothing has played since the start: begin the time line now.
                    next = Some(clock::now() as f64 - 300_000.0);
                }
            }
            unsafe {
                let _ = client.Stop();
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(AudioCapture { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(windows::core::Error::new(HRESULT(-1), "audio thread failed")),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
