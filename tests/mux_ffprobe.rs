//! Muxes real encoder output (the fixtures, made by ffmpeg's libx264, libx265, libaom and AAC
//! encoders) with rbuf's MP4 writer and has ffprobe and ffmpeg check the result: codec, size,
//! frame count, duration, keyframes, audio, and a full decode without errors. Skipped when
//! ffprobe is not on PATH.

use std::io::Cursor;
use std::process::Command;

use rbuf::mp4::{Mp4Writer, TrackKind, TrackSpec, VideoCodec};
use rbuf::{aac, av1, nal};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// Splits an Annex B stream into access units at the first slice of each picture.
fn access_units(codec: nal::Codec, data: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut has_vcl = false;
    for n in nal::split_annexb(data) {
        let t = codec.nal_type(n);
        let (vcl, first) = match codec {
            nal::Codec::H264 => ((1..=5).contains(&t), n.len() > 1 && n[1] & 0x80 != 0),
            nal::Codec::Hevc => (t < 32, n.len() > 2 && n[2] & 0x80 != 0),
        };
        if has_vcl && (!vcl || first) {
            out.push(std::mem::take(&mut cur));
            has_vcl = false;
        }
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(n);
        has_vcl |= vcl;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn ffprobe(path: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new("ffprobe").args(["-v", "error"]).args(args).arg(path).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn audio_track() -> (TrackSpec, Vec<Vec<u8>>) {
    let d = fixture("aac.adts");
    let frames = aac::parse_adts(&d).unwrap();
    let asc = aac::audio_specific_config(48000, 2).unwrap().to_vec();
    (
        TrackSpec { kind: TrackKind::Audio { rate: 48000, channels: 2, asc, bitrate: 128_000 }, name: "system audio".into() },
        frames.iter().map(|f| f.payload.to_vec()).collect(),
    )
}

fn write(codec: VideoCodec, samples: Vec<(Vec<u8>, bool)>, config: Vec<u8>, audio_delay: i64) -> Vec<u8> {
    let (atrack, aframes) = audio_track();
    let v = TrackSpec { kind: TrackKind::Video { codec, width: 320, height: 180, config }, name: "screen".into() };
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![v, atrack]).unwrap();
    // Interleave as a recorder would: by time.
    let frame = 10_000_000 / 30;
    let mut ai = 0;
    for (i, (s, key)) in samples.iter().enumerate() {
        let t = i as i64 * frame;
        while ai < aframes.len() && audio_delay + ai as i64 * 1024 * 10_000_000 / 48_000 <= t {
            w.write_sample(1, audio_delay + ai as i64 * 1024 * 10_000_000 / 48_000, &aframes[ai], true).unwrap();
            ai += 1;
        }
        w.write_sample(0, t, s, *key).unwrap();
    }
    while ai < aframes.len() {
        w.write_sample(1, audio_delay + ai as i64 * 1024 * 10_000_000 / 48_000, &aframes[ai], true).unwrap();
        ai += 1;
    }
    w.finish().unwrap().into_inner()
}

fn check(name: &str, data: &[u8], codec_name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, data).unwrap();
    let Some(streams) =
        ffprobe(&path, &["-show_entries", "stream=codec_name,width,height,nb_frames,channels,sample_rate", "-of", "csv=p=0"])
    else {
        eprintln!("SKIPPED: ffprobe not found");
        return;
    };
    let lines: Vec<&str> = streams.lines().collect();
    assert_eq!(lines.len(), 2, "{streams}");
    assert!(lines[0].starts_with(&format!("{codec_name},320,180")), "{streams}");
    assert!(lines[0].ends_with(",60"), "60 frames: {streams}");
    assert!(lines[1].starts_with("aac,48000,2"), "{streams}");
    // Keyframes where the encoder put them (every 30 frames).
    let keys = ffprobe(&path, &["-select_streams", "v", "-show_entries", "packet=flags", "-of", "csv=p=0"]).unwrap();
    let k: Vec<usize> = keys.lines().enumerate().filter(|(_, l)| l.starts_with('K')).map(|(i, _)| i).collect();
    assert_eq!(k, vec![0, 30], "{name}");
    let dur: f64 = ffprobe(&path, &["-show_entries", "format=duration", "-of", "csv=p=0"]).unwrap().trim().parse().unwrap();
    assert!((dur - 2.0).abs() < 0.1, "{name}: duration {dur}");
    // A full decode reports nothing.
    let dec = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-f", "null", "-"]).output().unwrap();
    assert!(dec.status.success() && dec.stderr.is_empty(), "{name}: {}", String::from_utf8_lossy(&dec.stderr));
}

#[test]
fn h264_and_aac() {
    let data = fixture("h264.264");
    let mut params = nal::ParamSets::default();
    let samples: Vec<(Vec<u8>, bool)> = access_units(nal::Codec::H264, &data)
        .iter()
        .map(|au| {
            let s = nal::to_sample(nal::Codec::H264, au, &mut params);
            (s.data, s.keyframe)
        })
        .collect();
    assert_eq!(samples.len(), 60);
    check("h264.mp4", &write(VideoCodec::H264, samples, nal::avcc(&params).unwrap(), 0), "h264");
}

#[test]
fn hevc_and_aac() {
    let data = fixture("hevc.265");
    let mut params = nal::ParamSets::default();
    let samples: Vec<(Vec<u8>, bool)> = access_units(nal::Codec::Hevc, &data)
        .iter()
        .map(|au| {
            let s = nal::to_sample(nal::Codec::Hevc, au, &mut params);
            (s.data, s.keyframe)
        })
        .collect();
    assert_eq!(samples.len(), 60);
    check("hevc.mp4", &write(VideoCodec::Hevc, samples, nal::hvcc(&params).unwrap(), 0), "hevc");
}

#[test]
fn av1_and_aac() {
    let data = fixture("av1.obu");
    let mut seq = None;
    let samples: Vec<(Vec<u8>, bool)> = av1::temporal_units(&data)
        .unwrap()
        .iter()
        .map(|tu| {
            let s = av1::to_sample(tu, &mut seq).unwrap();
            (s.data, s.keyframe)
        })
        .collect();
    let (h, obu) = seq.unwrap();
    check("av1.mp4", &write(VideoCodec::Av1, samples, av1::av1c(&h, &obu), 0), "av1");
}

#[test]
fn audio_that_starts_later_is_delayed_by_an_edit_list() {
    let data = fixture("h264.264");
    let mut params = nal::ParamSets::default();
    let samples: Vec<(Vec<u8>, bool)> = access_units(nal::Codec::H264, &data)
        .iter()
        .map(|au| {
            let s = nal::to_sample(nal::Codec::H264, au, &mut params);
            (s.data, s.keyframe)
        })
        .collect();
    let mp4 = write(VideoCodec::H264, samples, nal::avcc(&params).unwrap(), 5_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("delayed.mp4");
    std::fs::write(&path, &mp4).unwrap();
    let Some(t) = ffprobe(&path, &["-select_streams", "a", "-show_entries", "stream=start_time", "-of", "csv=p=0"]) else {
        eprintln!("SKIPPED: ffprobe not found");
        return;
    };
    let start: f64 = t.trim().parse().unwrap();
    assert!((start - 0.5).abs() < 0.005, "audio starts at {start}");
}
