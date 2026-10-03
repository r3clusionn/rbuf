//! Hardware video encoding through Media Foundation: the GPU vendor's encoder transform (NVENC on
//! NVIDIA, AMF on AMD, Quick Sync on Intel) in asynchronous mode, fed NV12 textures through a DXGI
//! device manager, so frames go from capture to encoder without a copy to system memory.

use std::mem::ManuallyDrop;
use std::sync::mpsc::{Receiver, Sender};

use windows::core::{Interface, Result, GUID, PWSTR};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};

use super::d3d::Gpu;
use crate::mp4::VideoCodec;

pub fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateControl {
    /// Constant bitrate.
    Cbr,
    /// Peak-constrained variable bitrate (peak 1.5 x the mean).
    Vbr,
    /// Constant quality, 0 to 100 (the encoder picks the bitrate).
    Quality(u32),
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub rate_control: RateControl,
    /// Frames between keyframes; the replay buffer cuts on these.
    pub gop: u32,
}

/// An encoder transform found on this machine.
#[derive(Clone, Debug)]
pub struct EncoderInfo {
    pub name: String,
    pub codec: VideoCodec,
    pub luid: Option<LUID>,
}

fn startup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

fn enum_activates(codec: VideoCodec) -> Vec<IMFActivate> {
    startup();
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let mut out = Vec::new();
    unsafe {
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut ptr,
            &mut n,
        )
        .is_ok()
            && !ptr.is_null()
        {
            for i in 0..n as usize {
                if let Some(a) = std::ptr::read(ptr.add(i)) {
                    out.push(a);
                }
            }
            CoTaskMemFree(Some(ptr as *const _));
        }
    }
    out
}

fn info_of(a: &IMFActivate, codec: VideoCodec) -> EncoderInfo {
    let mut name = String::new();
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
            name = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
        }
    }
    let mut luid = [0u8; 8];
    let luid = unsafe { a.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut luid, None) }.ok().map(|_| LUID {
        LowPart: u32::from_le_bytes(luid[..4].try_into().unwrap()),
        HighPart: i32::from_le_bytes(luid[4..].try_into().unwrap()),
    });
    EncoderInfo { name, codec, luid }
}

/// Every hardware encoder transform for every codec.
pub fn list() -> Vec<EncoderInfo> {
    [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
        .iter()
        .flat_map(|c| enum_activates(*c).iter().map(|a| info_of(a, *c)).collect::<Vec<_>>())
        .collect()
}

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VARENUM(VT_UI4.0),
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn set_api(api: &ICodecAPI, key: &GUID, v: u32) -> bool {
    unsafe { api.SetValue(key, &variant_u32(v)).is_ok() }
}

fn debug_size(t: &IMFTransform, when: &str) {
    if std::env::var_os("RBUF_DEBUG").is_none() {
        return;
    }
    unsafe {
        let o = t.GetOutputCurrentType(0).and_then(|m| m.GetUINT64(&MF_MT_FRAME_SIZE));
        let i = t.GetInputCurrentType(0).and_then(|m| m.GetUINT64(&MF_MT_FRAME_SIZE));
        let f =
            |r: Result<u64>| r.map(|v| format!("{}x{}", v >> 32, v & 0xffff_ffff)).unwrap_or_else(|e| e.message().to_string());
        eprintln!("rbuf debug {when}: output {}, input {}", f(o), f(i));
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn video_type(s: &Settings, sub: &GUID, output: bool) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, sub)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(s.width, s.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(s.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // What the converter produces, so the bitstream says so too.
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        // The picture is exactly the frame size, even where the encoder pads to whole blocks.
        let area = MFVideoArea {
            OffsetX: MFOffset { fract: 0, value: 0 },
            OffsetY: MFOffset { fract: 0, value: 0 },
            Area: windows::Win32::Foundation::SIZE { cx: s.width as i32, cy: s.height as i32 },
        };
        let bytes = std::slice::from_raw_parts(&area as *const _ as *const u8, std::mem::size_of::<MFVideoArea>());
        let _ = t.SetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes);
        if output {
            t.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate)?;
            match s.codec {
                VideoCodec::H264 => t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?,
                VideoCodec::Hevc => t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?,
                VideoCodec::Av1 => {}
            }
        }
        Ok(t)
    }
}

/// One encoded picture as the encoder wrote it (Annex B for H.264 and HEVC, OBUs for AV1).
#[derive(Debug)]
pub struct Encoded {
    pub data: Vec<u8>,
    /// 100 ns ticks, as given with the input.
    pub pts: i64,
    pub key: bool,
}

/// A frame for the encoder: a texture it may read until it asks for the next input.
pub struct Input {
    pub texture: ID3D11Texture2D,
    pub pts: i64,
    pub duration: i64,
    /// Ask for a keyframe (the start of a recording).
    pub keyframe: bool,
}

// COM interfaces here are agile (the device is multithread protected); they move to the encoder thread.
unsafe impl Send for Input {}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    api: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    pub name: String,
    pub settings: Settings,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(gpu: &Gpu, s: Settings) -> Result<Encoder> {
        let acts = enum_activates(s.codec);
        // Prefer the encoder on the adapter that captures (a second GPU's encoder cannot read our textures).
        let pick = acts
            .iter()
            .find(|a| info_of(a, s.codec).luid.is_some_and(|l| l.LowPart == gpu.luid.LowPart && l.HighPart == gpu.luid.HighPart))
            .or(acts.first())
            .ok_or_else(|| {
                windows::core::Error::new(
                    windows::core::HRESULT(-1),
                    format!("no hardware {:?} encoder on this machine", s.codec),
                )
            })?;
        let name = info_of(pick, s.codec).name;
        unsafe {
            let transform: IMFTransform = pick.ActivateObject()?;
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&gpu.device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let api: Option<ICodecAPI> = transform.cast().ok();
            if let Some(api) = &api {
                let (mode, quality) = match s.rate_control {
                    RateControl::Cbr => (eAVEncCommonRateControlMode_CBR.0 as u32, None),
                    RateControl::Vbr => (eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32, None),
                    RateControl::Quality(q) => (eAVEncCommonRateControlMode_Quality.0 as u32, Some(q)),
                };
                set_api(api, &CODECAPI_AVEncCommonRateControlMode, mode);
                set_api(api, &CODECAPI_AVEncCommonMeanBitRate, s.bitrate);
                if s.rate_control == RateControl::Vbr {
                    set_api(api, &CODECAPI_AVEncCommonMaxBitRate, s.bitrate / 2 * 3);
                }
                if let Some(q) = quality {
                    set_api(api, &CODECAPI_AVEncCommonQuality, q);
                }
                set_api(api, &CODECAPI_AVEncMPVGOPSize, s.gop);
                set_api(api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
                set_api(api, &CODECAPI_AVLowLatencyMode, 1);
            }

            transform.SetOutputType(0, &video_type(&s, &subtype(s.codec), true)?, 0)?;
            debug_size(&transform, "after SetOutputType");
            transform.SetInputType(0, &video_type(&s, &MFVideoFormat_NV12, false)?, 0)?;
            debug_size(&transform, "after SetInputType");
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = transform.cast()?;
            Ok(Encoder { transform, events, api, _manager: manager, name, settings: s })
        }
    }

    fn input(&self, f: &Input) -> Result<()> {
        unsafe {
            if f.keyframe {
                if let Some(api) = &self.api {
                    set_api(api, &CODECAPI_AVEncVideoForceKeyFrame, 1);
                }
            }
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &f.texture, 0, false)?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                buf.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(f.pts)?;
            sample.SetSampleDuration(f.duration)?;
            self.transform.ProcessInput(0, &sample, 0)
        }
    }

    fn output(&self) -> Result<Option<Encoded>> {
        unsafe {
            let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.transform.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    // The offered type can carry the encoder's defaults (NVIDIA's AV1 transform
                    // offers 1920x1080); keep our size, rate and bitrate on it.
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    let s = &self.settings;
                    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(s.width, s.height))?;
                    t.SetUINT64(&MF_MT_FRAME_RATE, pack(s.fps, 1))?;
                    t.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    debug_size(&self.transform, "after stream change");
                    return Ok(None);
                }
                return Err(e);
            }
            let Some(sample) = sample else { return Ok(None) };
            let b = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            b.Lock(&mut p, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(p, len as usize).to_vec();
            b.Unlock()?;
            let pts = sample.GetSampleTime().unwrap_or(0);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            Ok(Some(Encoded { data, pts, key }))
        }
    }

    /// Runs the encoder until `frames` closes: feeds frames when the transform asks for input and
    /// sends every picture it produces. Drains the transform at the end.
    pub fn run(self, frames: Receiver<Input>, out: Sender<Encoded>) -> Result<()> {
        let mut finished = false;
        loop {
            let ev = unsafe { self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
            let kind = unsafe { ev.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                if finished {
                    continue;
                }
                match frames.recv() {
                    Ok(f) => self.input(&f)?,
                    Err(_) => {
                        finished = true;
                        unsafe {
                            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                        }
                    }
                }
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(e) = self.output()? {
                    if out.send(e).is_err() {
                        break;
                    }
                }
            } else if kind == METransformDrainComplete.0 as u32 {
                break;
            }
        }
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = MFShutdownObject(&self.transform);
        }
        Ok(())
    }
}
