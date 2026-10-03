//! The running program: capture, pacer, encoder and audio threads feeding the replay buffer and,
//! while recording, a file; commands from hotkeys, other terminals and Ctrl+C.
//!
//! Threads: the capture callback copies each new frame into one texture; the pacer wakes at the
//! output frame rate, converts the newest frame to NV12 and hands it to the encoder thread; each
//! audio source has a thread that captures and encodes AAC. Everything encoded arrives on one
//! channel at the main loop, which turns it into MP4 samples, keeps it in the ring and writes it to
//! a recording. Saving a clip copies references to the packets and writes the file on its own
//! thread, so the buffer keeps running.

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::SystemInformation::GetLocalTime;

use super::audio::{self, AacFrame, AudioCapture, Source};
use super::capture::{self, Capture, Method, Target};
use super::clock;
use super::control::{self, Cmd};
use super::convert::{self, Converter, Nv12};
use super::d3d::Gpu;
use super::encoder::{Encoded, Encoder, Input, RateControl, Settings};
use crate::args::{self, BitrateMode, Options, Quality, Window};
use crate::mp4::{Mp4Writer, TrackKind, TrackSpec, VideoCodec};
use crate::ring::{Packet, Ring, Snapshot};
use crate::{aac, av1, nal};

enum Msg {
    Video(Encoded),
    Audio(usize, AacFrame),
    Cmd(Cmd),
    /// Path, seconds of video, bytes, milliseconds it took.
    Saved(std::result::Result<(PathBuf, f64, u64, u128), String>),
}

/// Turns encoder output into MP4 samples and remembers the codec configuration.
struct VideoState {
    codec: VideoCodec,
    size: (u32, u32),
    params: nal::ParamSets,
    seq: Option<(av1::SequenceHeader, Vec<u8>)>,
}

impl VideoState {
    fn sample(&mut self, e: &Encoded) -> Option<(Vec<u8>, bool)> {
        match self.codec {
            VideoCodec::H264 | VideoCodec::Hevc => {
                let c = if self.codec == VideoCodec::H264 { nal::Codec::H264 } else { nal::Codec::Hevc };
                let s = nal::to_sample(c, &e.data, &mut self.params);
                (!s.data.is_empty()).then_some((s.data, s.keyframe || e.key))
            }
            VideoCodec::Av1 => {
                let s = av1::to_sample_sized(&e.data, &mut self.seq, Some(self.size))?;
                (!s.data.is_empty()).then_some((s.data, s.keyframe || e.key))
            }
        }
    }

    fn config(&self) -> Option<Vec<u8>> {
        match self.codec {
            VideoCodec::H264 => nal::avcc(&self.params),
            VideoCodec::Hevc => nal::hvcc(&self.params),
            VideoCodec::Av1 => self.seq.as_ref().map(|(h, o)| av1::av1c(h, o)),
        }
    }
}

/// `12.3 MB` or `850 KB`.
fn size(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1e6)
    } else {
        format!("{} KB", bytes.div_ceil(1000))
    }
}

fn local_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn default_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Videos")).unwrap_or_else(|| PathBuf::from("."))
}

fn unique(dir: &Path, prefix: &str) -> PathBuf {
    let base = format!("{prefix}_{}", local_stamp());
    let mut p = dir.join(format!("{base}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base}_{n}.mp4"));
        n += 1;
    }
    p
}

/// Writes a snapshot of the ring as an MP4 file, interleaving the streams by time.
pub fn write_clip(path: &Path, specs: Vec<TrackSpec>, snap: &Snapshot, frame_ticks: i64) -> std::io::Result<u64> {
    let f = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    let mut w = Mp4Writer::new(f, specs)?;
    let mut idx = vec![0usize; snap.streams.len()];
    loop {
        // The stream whose next packet is earliest.
        let next =
            (0..snap.streams.len()).filter(|s| idx[*s] < snap.streams[*s].len()).min_by_key(|s| snap.streams[*s][idx[*s]].pts);
        let Some(s) = next else { break };
        let p = &snap.streams[s][idx[s]];
        w.write_sample(s, p.pts, &p.data, p.key)?;
        idx[s] += 1;
    }
    w.set_last_duration(0, frame_ticks);
    let mut out = w.finish()?;
    use std::io::Write;
    out.flush()?;
    Ok(out.get_ref().metadata().map(|m| m.len()).unwrap_or(0))
}

/// A recording in progress.
struct Recording {
    path: PathBuf,
    writer: Mp4Writer<BufWriter<std::fs::File>>,
    /// Video starts at a keyframe; nothing is written before it.
    start: Option<i64>,
    last: Vec<i64>,
}

impl Recording {
    fn new(path: PathBuf, specs: Vec<TrackSpec>) -> std::io::Result<Recording> {
        if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(d)?;
        }
        let n = specs.len();
        let f = BufWriter::with_capacity(4 << 20, std::fs::File::create(&path)?);
        Ok(Recording { path, writer: Mp4Writer::new(f, specs)?, start: None, last: vec![i64::MIN; n] })
    }

    fn write(&mut self, stream: usize, p: &Packet) -> std::io::Result<()> {
        if stream == 0 && self.start.is_none() {
            if !p.key {
                return Ok(());
            }
            self.start = Some(p.pts);
        }
        let Some(start) = self.start else {
            return Ok(());
        };
        if p.pts < start || p.pts <= self.last[stream] {
            return Ok(());
        }
        self.last[stream] = p.pts;
        self.writer.write_sample(stream, p.pts, &p.data, p.key)
    }

    fn finish(mut self, config: Option<Vec<u8>>, frame_ticks: i64) -> std::io::Result<(PathBuf, f64, u64)> {
        if let Some(c) = config {
            self.writer.set_video_config(0, c);
        }
        let frames = self.writer.sample_count(0);
        self.writer.set_last_duration(0, frame_ticks);
        let out = self.writer.finish()?;
        let size = out.get_ref().metadata().map(|m| m.len()).unwrap_or(0);
        if frames == 0 {
            let _ = std::fs::remove_file(&self.path);
            return Err(std::io::Error::other("nothing was recorded"));
        }
        let media = (self.last[0] - self.start.unwrap_or(self.last[0]) + frame_ticks) as f64 / 1e7;
        Ok((self.path, media, size))
    }
}

fn resolve_target(w: &Window) -> Result<Target, String> {
    Ok(match w {
        Window::Screen(i) => {
            if let Some(i) = i {
                if *i >= capture::monitors().len() {
                    return Err(format!("there is no screen {i} (see rbuf --list-capture-options)"));
                }
            }
            Target::Monitor(*i)
        }
        Window::Focused => Target::Focused,
        Window::Handle(h) => Target::Window(*h),
        Window::Title(t) => {
            let want = t.to_ascii_lowercase();
            let found = capture::windows().into_iter().find(|(_, title)| title.to_ascii_lowercase().contains(&want));
            Target::Window(
                found.ok_or_else(|| format!("no visible window has `{t}` in its title (see rbuf --list-capture-options)"))?.0,
            )
        }
    })
}

fn codec_of(c: args::Codec) -> VideoCodec {
    match c {
        args::Codec::H264 => VideoCodec::H264,
        args::Codec::Hevc => VideoCodec::Hevc,
        args::Codec::Av1 => VideoCodec::Av1,
    }
}

fn codec_label(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "H.264",
        VideoCodec::Hevc => "HEVC",
        VideoCodec::Av1 => "AV1",
    }
}

/// Starts capture with the requested method. `auto` tries NvFBC for screens and falls back to
/// Windows Graphics Capture, saying why.
fn start_capture(gpu: &Gpu, target: &Target, m: args::CaptureMethod, cursor: bool) -> Result<(Capture, Method), String> {
    let e = |x: windows::core::Error| x.message().to_string();
    let method = match m {
        args::CaptureMethod::Nvfbc => Method::Nvfbc,
        args::CaptureMethod::Wgc => Method::Wgc,
        args::CaptureMethod::Dxgi => Method::Dxgi,
        args::CaptureMethod::Auto => {
            if matches!(target, Target::Monitor(_)) {
                match Capture::start(gpu, target, Method::Nvfbc, cursor) {
                    Ok(c) => return Ok((c, Method::Nvfbc)),
                    Err(x) => eprintln!("rbuf: NvFBC unavailable ({}), using Windows Graphics Capture", x.message()),
                }
            }
            Method::Wgc
        }
    };
    Ok((Capture::start(gpu, target, method, cursor).map_err(e)?, method))
}

pub fn run(o: Options) -> Result<(), String> {
    let e = |x: windows::core::Error| x.message().to_string();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let target = resolve_target(&o.window)?;
    let sources: Vec<Source> = o.audio.iter().map(|a| Source::parse(a)).collect::<Result<_, _>>()?;
    let gpu = Gpu::new(o.adapter).map_err(e)?;
    let (cap, method) = start_capture(&gpu, &target, o.capture, o.cursor)?;
    // Wait for the first frame (Windows Graphics Capture sends one at once; duplication when the screen changes).
    let t0 = Instant::now();
    while cap.latest.lock().unwrap().seq == 0 && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let (cw, ch) = {
        let l = cap.latest.lock().unwrap();
        if l.seq > 0 {
            l.content
        } else {
            cap.size
        }
    };
    let (w, h) = o.size.unwrap_or((cw & !1, ch & !1));
    if !convert::nv12_uav_supported(&gpu) {
        return Err(format!(
            "{} cannot write NV12 from a compute shader (typed UAV store); this GPU is not supported yet",
            gpu.adapter_name
        ));
    }
    let codec = codec_of(o.codec);
    let mut conv = Converter::new(&gpu, w, h).map_err(e)?;
    let pool: Vec<Nv12> = (0..8).map(|_| conv.target()).collect::<Result<_, _>>().map_err(e)?;

    let (bitrate, rc) = match (&o.quality, o.bitrate_mode) {
        (Quality::Number(q), BitrateMode::Qp) => {
            (args::preset_bitrate("very_high", o.codec, w, h, o.fps), RateControl::Quality(*q))
        }
        (Quality::Preset(p), BitrateMode::Qp) => {
            let q = match *p {
                "low" => 40,
                "medium" => 55,
                "high" => 70,
                "very_high" => 80,
                _ => 90,
            };
            (args::preset_bitrate(p, o.codec, w, h, o.fps), RateControl::Quality(q))
        }
        (Quality::Number(k), m) => (k * 1000, if m == BitrateMode::Cbr { RateControl::Cbr } else { RateControl::Vbr }),
        (Quality::Preset(p), m) => (
            args::preset_bitrate(p, o.codec, w, h, o.fps),
            if m == BitrateMode::Cbr { RateControl::Cbr } else { RateControl::Vbr },
        ),
    };
    let gop = ((o.fps as f64 * o.gop_seconds).round() as u32).max(1);
    let enc =
        Encoder::new(&gpu, Settings { codec, width: w, height: h, fps: o.fps, bitrate, rate_control: rc, gop }).map_err(e)?;
    let enc_name = enc.name.clone();

    let (tx, rx) = mpsc::channel::<Msg>();
    // Audio first, so a missing app fails before anything else runs.
    let mut audio_caps = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        let (atx, arx) = mpsc::channel::<AacFrame>();
        audio_caps.push(AudioCapture::start(s.clone(), atx).map_err(|x| format!("audio source {}: {}", s.label(), x.message()))?);
        let t = tx.clone();
        std::thread::spawn(move || {
            while let Ok(f) = arx.recv() {
                if t.send(Msg::Audio(i, f)).is_err() {
                    break;
                }
            }
        });
    }

    // Encoder thread and its forwarder.
    let (ftx, frx) = mpsc::sync_channel::<Input>(1);
    let (vtx, vrx) = mpsc::channel::<Encoded>();
    let enc_thread = std::thread::spawn(move || enc.run(frx, vtx));
    {
        let t = tx.clone();
        std::thread::spawn(move || {
            while let Ok(v) = vrx.recv() {
                if t.send(Msg::Video(v)).is_err() {
                    break;
                }
            }
        });
    }

    // Pacer.
    let stop = Arc::new(AtomicBool::new(false));
    let force_key = Arc::new(AtomicBool::new(false));
    let stats = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]); // encoded, repeated, dropped
    let fps = o.fps as i64;
    let cfr = o.cfr;
    let pacer = {
        let (stop, force_key, stats, latest) = (stop.clone(), force_key.clone(), stats.clone(), cap.latest.clone());
        std::thread::spawn(move || {
            let start = clock::now();
            let mut i: i64 = 0;
            let mut last_seq = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let pts = start + i * 10_000_000 / fps;
                let next = start + (i + 1) * 10_000_000 / fps;
                loop {
                    let now = clock::now();
                    if now >= pts {
                        break;
                    }
                    let left = Duration::from_nanos(((pts - now) * 100) as u64);
                    std::thread::sleep(left.saturating_sub(Duration::from_micros(800)).max(Duration::from_micros(200)));
                }
                i += 1;
                let l = latest.lock().unwrap();
                let Some(tex) = l.texture.clone() else {
                    continue;
                };
                let fresh = l.seq != last_seq;
                if !cfr && !fresh {
                    continue;
                }
                last_seq = l.seq;
                let out = &pool[i as usize % pool.len()];
                if conv.convert(&tex, l.content, out).is_err() {
                    continue;
                }
                drop(l);
                let input = Input {
                    texture: out.texture.clone(),
                    pts,
                    duration: next - pts,
                    keyframe: force_key.swap(false, Ordering::Relaxed),
                };
                match ftx.try_send(input) {
                    Ok(()) => {
                        stats[0].fetch_add(1, Ordering::Relaxed);
                        if !fresh {
                            stats[1].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(mpsc::TrySendError::Full(_)) => {
                        stats[2].fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
            drop(ftx);
        })
    };

    control::listen_ctrl_c({
        let (ctx, crx) = mpsc::channel();
        let t = tx.clone();
        std::thread::spawn(move || {
            while let Ok(c) = crx.recv() {
                if t.send(Msg::Cmd(c)).is_err() {
                    break;
                }
            }
        });
        ctx
    });
    let cmd_tx = {
        let (ctx, crx) = mpsc::channel::<Cmd>();
        let t = tx.clone();
        std::thread::spawn(move || {
            while let Ok(c) = crx.recv() {
                if t.send(Msg::Cmd(c)).is_err() {
                    break;
                }
            }
        });
        ctx
    };
    let _ = control::listen_events(cmd_tx.clone());
    if o.replay.is_some() {
        let keys = vec![
            (args::parse_hotkey(&o.hotkey_save)?, Cmd::Save, o.hotkey_save.clone()),
            (args::parse_hotkey(&o.hotkey_record)?, Cmd::ToggleRecord, o.hotkey_record.clone()),
        ];
        control::listen_hotkeys(keys, cmd_tx);
    }

    // Track specs. The video config is filled in when the first keyframe has been seen.
    let audio_specs: Vec<TrackSpec> = sources
        .iter()
        .map(|s| TrackSpec {
            kind: TrackKind::Audio {
                rate: audio::RATE,
                channels: audio::CHANNELS,
                asc: aac::audio_specific_config(audio::RATE, audio::CHANNELS as u8).unwrap().to_vec(),
                bitrate: audio::AAC_BYTES_PER_SECOND * 8,
            },
            name: s.label(),
        })
        .collect();
    let mut video = VideoState { codec, size: (w, h), params: Default::default(), seq: None };
    let specs = |video: &VideoState| -> Vec<TrackSpec> {
        let mut v = vec![TrackSpec {
            kind: TrackKind::Video { codec, width: w as u16, height: h as u16, config: video.config().unwrap_or_default() },
            name: "Video".into(),
        }];
        v.extend(audio_specs.iter().cloned());
        v
    };

    let frame_ticks = 10_000_000 / fps;
    let out_dir = match (&o.replay, &o.output) {
        (Some(_), Some(p)) => p.clone(),
        (Some(_), None) => default_dir(),
        (None, Some(p)) => p.parent().map(|d| d.to_path_buf()).unwrap_or_default(),
        (None, None) => unreachable!(),
    };
    let mut ring = o.replay.map(|r| {
        let budget = o.ram_limit_mb.map(|m| m as usize * 1024 * 1024).unwrap_or(usize::MAX);
        Ring::new(1 + sources.len(), r as i64 * 10_000_000, budget)
    });
    let mut recording = match (&o.replay, &o.output) {
        (None, Some(p)) => {
            force_key.store(true, Ordering::Relaxed);
            Some(Recording::new(p.clone(), specs(&video)).map_err(|x| format!("{}: {x}", p.display()))?)
        }
        _ => None,
    };

    let what = match &target {
        Target::Monitor(Some(i)) => format!("screen {i}"),
        Target::Monitor(None) => "the primary screen".into(),
        Target::Window(_) => "a window".into(),
        Target::Focused => "the focused window".into(),
    };
    eprintln!(
        "rbuf: {what} at {w}x{h} via {}, {} fps {}, {} on {enc_name}, {:.1} Mbit/s {}",
        match method {
            Method::Nvfbc => "NvFBC",
            Method::Wgc => "Windows Graphics Capture",
            Method::Dxgi => "Desktop Duplication",
        },
        o.fps,
        if cfr { "constant" } else { "variable" },
        codec_label(codec),
        bitrate as f64 / 1e6,
        match rc {
            RateControl::Cbr => "CBR".to_string(),
            RateControl::Vbr => "VBR".to_string(),
            RateControl::Quality(q) => format!("(quality {q})"),
        }
    );
    if !sources.is_empty() {
        eprintln!("rbuf: audio tracks: {}", sources.iter().map(|s| s.label()).collect::<Vec<_>>().join(", "));
    }
    match (&o.replay, &recording) {
        (Some(r), _) => eprintln!(
            "rbuf: replay buffer of {r} s; {} saves to {}, {} starts and stops a recording, Ctrl+C quits (or rbuf save/record/stop)",
            o.hotkey_save,
            out_dir.display(),
            o.hotkey_record
        ),
        (None, Some(rec)) => eprintln!("rbuf: recording to {}; Ctrl+C or rbuf stop ends it", rec.path.display()),
        _ => {}
    }

    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut last_stats = [0u64; 3];
    let mut saving = 0usize;
    let mut stopping = false;
    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Some(Msg::Video(enc)) => {
                if let Some((data, key)) = video.sample(&enc) {
                    let p = Packet { pts: enc.pts, key, data: data.into() };
                    if let Some(r) = ring.as_mut() {
                        r.push(0, p.clone());
                    }
                    if let Some(rec) = recording.as_mut() {
                        if let Err(x) = rec.write(0, &p) {
                            eprintln!("rbuf: recording failed: {x}");
                            recording = None;
                        }
                    }
                }
            }
            Some(Msg::Audio(i, f)) => {
                let p = Packet { pts: f.pts, key: true, data: f.data.into() };
                if let Some(r) = ring.as_mut() {
                    r.push(i + 1, p.clone());
                }
                if let Some(rec) = recording.as_mut() {
                    let _ = rec.write(i + 1, &p);
                }
            }
            Some(Msg::Cmd(Cmd::Save)) => {
                if let (Some(r), Some(secs)) = (ring.as_ref(), o.replay) {
                    if video.config().is_none() {
                        eprintln!("rbuf: nothing to save yet");
                        continue;
                    }
                    let snap = r.snapshot(secs as i64 * 10_000_000);
                    let path = unique(&out_dir, "Replay");
                    let sp = specs(&video);
                    let t = tx.clone();
                    saving += 1;
                    std::thread::spawn(move || {
                        let t0 = Instant::now();
                        let dur = snap.duration_ticks() as f64 / 1e7;
                        let r = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
                            .and_then(|_| write_clip(&path, sp, &snap, frame_ticks))
                            .map(|size| (path.clone(), dur, size, t0.elapsed().as_millis()))
                            .map_err(|x| format!("{}: {x}", path.display()));
                        let _ = t.send(Msg::Saved(r));
                    });
                } else {
                    eprintln!("rbuf: not running as a replay buffer (start with -r SECONDS)");
                }
            }
            Some(Msg::Cmd(Cmd::ToggleRecord)) => {
                if o.replay.is_none() {
                    continue;
                }
                match recording.take() {
                    Some(rec) => match rec.finish(video.config(), frame_ticks) {
                        Ok((p, d, s)) => {
                            eprintln!("rbuf: recorded {d:.1} s, {}: {}", size(s), p.display())
                        }
                        Err(x) => eprintln!("rbuf: recording failed: {x}"),
                    },
                    None => {
                        let path = unique(&out_dir, "Recording");
                        match Recording::new(path, specs(&video)) {
                            Ok(r) => {
                                force_key.store(true, Ordering::Relaxed);
                                eprintln!("rbuf: recording to {}", r.path.display());
                                recording = Some(r);
                            }
                            Err(x) => eprintln!("rbuf: cannot record: {x}"),
                        }
                    }
                }
            }
            Some(Msg::Cmd(Cmd::Stop)) => stopping = true,
            Some(Msg::Saved(r)) => {
                saving -= 1;
                match r {
                    Ok((p, d, s, ms)) => eprintln!("rbuf: saved {d:.1} s, {} in {ms} ms: {}", size(s), p.display()),
                    Err(x) => eprintln!("rbuf: saving failed: {x}"),
                }
            }
            None => {}
        }
        if o.duration.is_some_and(|d| started.elapsed().as_secs_f64() >= d) {
            stopping = true;
        }
        if o.verbose && last_report.elapsed() >= Duration::from_secs(1) {
            let s: Vec<u64> = stats.iter().map(|x| x.load(Ordering::Relaxed)).collect();
            let held = ring
                .as_ref()
                .map(|r| format!(", buffer {:.1} s {:.0} MB", r.held_ticks() as f64 / 1e7, r.bytes() as f64 / 1e6))
                .unwrap_or_default();
            eprintln!(
                "rbuf: {:.0} fps encoded ({} repeated, {} dropped){held}",
                (s[0] - last_stats[0]) as f64 / last_report.elapsed().as_secs_f64(),
                s[1] - last_stats[1],
                s[2] - last_stats[2]
            );
            last_stats = [s[0], s[1], s[2]];
            last_report = Instant::now();
        }
        if stopping {
            break;
        }
    }

    // Shut down in order: stop producing, let the encoder drain, then write what is left.
    stop.store(true, Ordering::Relaxed);
    let _ = pacer.join();
    drop(audio_caps);
    let enc_result = enc_thread.join().map_err(|_| "encoder thread panicked".to_string())?;
    drop(tx);
    while let Ok(m) = rx.recv_timeout(Duration::from_millis(500)) {
        match m {
            Msg::Video(enc) => {
                if let (Some(rec), Some((data, key))) = (recording.as_mut(), video.sample(&enc)) {
                    let _ = rec.write(0, &Packet { pts: enc.pts, key, data: data.into() });
                }
            }
            Msg::Audio(i, f) => {
                if let Some(rec) = recording.as_mut() {
                    let _ = rec.write(i + 1, &Packet { pts: f.pts, key: true, data: f.data.into() });
                }
            }
            Msg::Saved(r) => {
                saving = saving.saturating_sub(1);
                match r {
                    Ok((p, d, s, ms)) => eprintln!("rbuf: saved {d:.1} s, {} in {ms} ms: {}", size(s), p.display()),
                    Err(x) => eprintln!("rbuf: saving failed: {x}"),
                }
            }
            Msg::Cmd(_) => {}
        }
        if saving == 0 && rx.try_recv().is_err() {
            break;
        }
    }
    if let Some(rec) = recording.take() {
        match rec.finish(video.config(), frame_ticks) {
            Ok((p, d, s)) => eprintln!("rbuf: recorded {d:.1} s, {}: {}", size(s), p.display()),
            Err(x) => return Err(format!("recording failed: {x}")),
        }
    }
    let s: Vec<u64> = stats.iter().map(|x| x.load(Ordering::Relaxed)).collect();
    if o.verbose {
        eprintln!("rbuf: {} frames encoded, {} repeated, {} dropped", s[0], s[1], s[2]);
    }
    enc_result.map_err(e)?;
    drop(cap);
    Ok(())
}
