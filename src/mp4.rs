//! An MP4 (ISO BMFF) writer for H.264, HEVC and AV1 video and AAC audio tracks.
//!
//! Samples are written into one `mdat` as they arrive, so a long recording never sits in memory;
//! the sample tables (`moov`) are written at the end and the `mdat` size patched in. Tracks that
//! start later than the first one get an edit list, so audio and video stay in sync however the
//! clip was cut. Every video sample entry says BT.709 limited range (`colr nclx`), which is what
//! the encoder input is converted to.

use std::io::{self, Seek, SeekFrom, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    fn sample_entry(self) -> &'static [u8; 4] {
        match self {
            VideoCodec::H264 => b"avc1",
            VideoCodec::Hevc => b"hvc1",
            VideoCodec::Av1 => b"av01",
        }
    }

    fn config_box(self) -> &'static [u8; 4] {
        match self {
            VideoCodec::H264 => b"avcC",
            VideoCodec::Hevc => b"hvcC",
            VideoCodec::Av1 => b"av1C",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrackKind {
    /// `config` is the `avcC`, `hvcC` or `av1C` record.
    Video { codec: VideoCodec, width: u16, height: u16, config: Vec<u8> },
    /// `asc` is the AAC `AudioSpecificConfig`.
    Audio { rate: u32, channels: u16, asc: Vec<u8>, bitrate: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackSpec {
    pub kind: TrackKind,
    /// Shown by players as the track's name (the handler name), e.g. "game.exe".
    pub name: String,
}

impl TrackSpec {
    fn timescale(&self) -> u32 {
        match &self.kind {
            TrackKind::Video { .. } => 90_000,
            TrackKind::Audio { rate, .. } => *rate,
        }
    }

    fn is_video(&self) -> bool {
        matches!(self.kind, TrackKind::Video { .. })
    }
}

/// 100 ns units, the clock of Windows capture and audio timestamps.
pub const TICKS_PER_SECOND: i64 = 10_000_000;
const MOVIE_TIMESCALE: u64 = 1000;

#[derive(Clone, Copy, Debug)]
struct SampleRec {
    offset: u64,
    size: u32,
    /// Presentation time in the track's timescale.
    pts: i64,
    key: bool,
}

struct TrackState {
    spec: TrackSpec,
    samples: Vec<SampleRec>,
    /// Duration of the last sample, when nothing follows it.
    last_duration: u32,
}

pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    tracks: Vec<TrackState>,
    mdat_start: u64,
    pos: u64,
}

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
    b.extend_from_slice(body);
    bx(kind, &b)
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|m| m.to_be_bytes()).collect()
}

/// Converts 100 ns ticks to a timescale, rounding to nearest.
pub fn ticks_to(ticks: i64, timescale: u32) -> i64 {
    let t = ticks as i128 * timescale as i128;
    let d = TICKS_PER_SECOND as i128;
    (if t >= 0 { (t + d / 2) / d } else { (t - d / 2) / d }) as i64
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Writes `ftyp` and the start of `mdat`.
    pub fn new(mut out: W, tracks: Vec<TrackSpec>) -> io::Result<Mp4Writer<W>> {
        let mut brands: Vec<&[u8; 4]> = vec![b"isom", b"iso2", b"mp41"];
        for t in &tracks {
            if let TrackKind::Video { codec, .. } = t.kind {
                brands.push(match codec {
                    VideoCodec::H264 => b"avc1",
                    VideoCodec::Hevc => b"hvc1",
                    VideoCodec::Av1 => b"av01",
                });
            }
        }
        let mut f = b"isom".to_vec();
        f.extend_from_slice(&0x200u32.to_be_bytes());
        for b in brands {
            f.extend_from_slice(b);
        }
        let ftyp = bx(b"ftyp", &f);
        out.write_all(&ftyp)?;
        // mdat with a 64-bit size, patched in `finish`.
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        let mdat_start = ftyp.len() as u64;
        Ok(Mp4Writer {
            out,
            tracks: tracks.into_iter().map(|spec| TrackState { spec, samples: Vec::new(), last_duration: 0 }).collect(),
            mdat_start,
            pos: mdat_start + 16,
        })
    }

    /// Appends a sample. `pts` is in 100 ns ticks on a clock shared by all tracks; samples of one
    /// track must come in presentation order.
    pub fn write_sample(&mut self, track: usize, pts: i64, data: &[u8], key: bool) -> io::Result<()> {
        let t = self.tracks.get_mut(track).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no such track"))?;
        let ts_pts = ticks_to(pts, t.spec.timescale());
        if let Some(last) = t.samples.last() {
            if ts_pts <= last.pts {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "samples must have increasing timestamps"));
            }
        }
        self.out.write_all(data)?;
        t.samples.push(SampleRec { offset: self.pos, size: data.len() as u32, pts: ts_pts, key });
        self.pos += data.len() as u64;
        Ok(())
    }

    /// Sets how long the last sample of a track lasts (default: as long as the one before it, or
    /// one AAC frame for audio).
    pub fn set_last_duration(&mut self, track: usize, ticks: i64) {
        if let Some(t) = self.tracks.get_mut(track) {
            t.last_duration = ticks_to(ticks, t.spec.timescale()).max(1) as u32;
        }
    }

    /// Sets a video track's codec configuration (`avcC`, `hvcC`, `av1C`) when it is only known
    /// after the first keyframe, as in a live recording. Written by `finish`.
    pub fn set_video_config(&mut self, track: usize, cfg: Vec<u8>) {
        if let Some(TrackState { spec: TrackSpec { kind: TrackKind::Video { config, .. }, .. }, .. }) = self.tracks.get_mut(track)
        {
            *config = cfg;
        }
    }

    pub fn sample_count(&self, track: usize) -> usize {
        self.tracks.get(track).map(|t| t.samples.len()).unwrap_or(0)
    }

    /// Writes `moov`, patches the `mdat` size and returns the output.
    pub fn finish(mut self) -> io::Result<W> {
        let moov = self.moov();
        self.out.write_all(&moov)?;
        let end = self.out.stream_position()?;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&(self.pos - self.mdat_start).to_be_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(self.out)
    }

    fn durations(t: &TrackState) -> Vec<u32> {
        let s = &t.samples;
        let mut d: Vec<u32> = s.windows(2).map(|w| (w[1].pts - w[0].pts) as u32).collect();
        if !s.is_empty() {
            let last = if t.last_duration > 0 {
                t.last_duration
            } else if let Some(prev) = d.last() {
                *prev
            } else if let TrackKind::Audio { .. } = t.spec.kind {
                1024
            } else {
                t.spec.timescale() / 60
            };
            d.push(last);
        }
        d
    }

    fn moov(&self) -> Vec<u8> {
        // The track that starts first defines time zero; the others start later by an edit.
        let start_ticks: Vec<Option<f64>> =
            self.tracks.iter().map(|t| t.samples.first().map(|s| s.pts as f64 / t.spec.timescale() as f64)).collect();
        let zero = start_ticks.iter().flatten().cloned().fold(f64::INFINITY, f64::min);
        let zero = if zero.is_finite() { zero } else { 0.0 };
        let mut traks = Vec::new();
        let mut movie_duration = 0u64;
        for (i, t) in self.tracks.iter().enumerate() {
            let durs = Self::durations(t);
            let media_duration: u64 = durs.iter().map(|d| *d as u64).sum();
            let delay_s = start_ticks[i].map(|s| s - zero).unwrap_or(0.0);
            let delay = (delay_s * MOVIE_TIMESCALE as f64).round() as u64;
            let track_movie = media_duration * MOVIE_TIMESCALE / t.spec.timescale() as u64;
            movie_duration = movie_duration.max(delay + track_movie);
            traks.push(self.trak(i as u32 + 1, t, &durs, media_duration, delay, track_movie));
        }
        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&0u64.to_be_bytes()); // creation
        mvhd.extend_from_slice(&0u64.to_be_bytes()); // modification
        mvhd.extend_from_slice(&(MOVIE_TIMESCALE as u32).to_be_bytes());
        mvhd.extend_from_slice(&movie_duration.to_be_bytes());
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        mvhd.extend_from_slice(&[0; 10]);
        mvhd.extend_from_slice(&matrix());
        mvhd.extend_from_slice(&[0; 24]);
        mvhd.extend_from_slice(&(self.tracks.len() as u32 + 1).to_be_bytes());
        let mut body = full(b"mvhd", 1, 0, &mvhd);
        for t in traks {
            body.extend(t);
        }
        bx(b"moov", &body)
    }

    fn trak(&self, id: u32, t: &TrackState, durs: &[u32], media_duration: u64, delay: u64, track_movie: u64) -> Vec<u8> {
        let ts = t.spec.timescale();
        let mut tkhd = Vec::new();
        tkhd.extend_from_slice(&0u64.to_be_bytes());
        tkhd.extend_from_slice(&0u64.to_be_bytes());
        tkhd.extend_from_slice(&id.to_be_bytes());
        tkhd.extend_from_slice(&0u32.to_be_bytes());
        tkhd.extend_from_slice(&(delay + track_movie).to_be_bytes());
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
                                                     // Every audio track is its own alternate group, so players offer them as choices but a
                                                     // tool that mixes can still find them all.
        tkhd.extend_from_slice(&(if t.spec.is_video() { 0u16 } else { 1 }).to_be_bytes());
        tkhd.extend_from_slice(&(if t.spec.is_video() { 0u16 } else { 0x0100 }).to_be_bytes());
        tkhd.extend_from_slice(&0u16.to_be_bytes());
        tkhd.extend_from_slice(&matrix());
        let (w, h) = match &t.spec.kind {
            TrackKind::Video { width, height, .. } => (*width as u32, *height as u32),
            _ => (0, 0),
        };
        tkhd.extend_from_slice(&(w << 16).to_be_bytes());
        tkhd.extend_from_slice(&(h << 16).to_be_bytes());
        // Only the first audio track plays by default; the rest are enabled but not in the
        // presentation, so players do not mix them on top of each other.
        let first_audio = self.tracks.iter().position(|x| !x.spec.is_video());
        let default = t.spec.is_video() || first_audio == Some(id as usize - 1);
        let tkhd = full(b"tkhd", 1, if default { 3 } else { 1 }, &tkhd);

        let edts = if delay > 0 {
            let mut e = 2u32.to_be_bytes().to_vec();
            e.extend_from_slice(&delay.to_be_bytes());
            e.extend_from_slice(&(-1i64).to_be_bytes());
            e.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            e.extend_from_slice(&track_movie.to_be_bytes());
            e.extend_from_slice(&0i64.to_be_bytes());
            e.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            bx(b"edts", &full(b"elst", 1, 0, &e))
        } else {
            Vec::new()
        };

        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&0u64.to_be_bytes());
        mdhd.extend_from_slice(&0u64.to_be_bytes());
        mdhd.extend_from_slice(&ts.to_be_bytes());
        mdhd.extend_from_slice(&media_duration.to_be_bytes());
        mdhd.extend_from_slice(&0x55c4u16.to_be_bytes()); // "und"
        mdhd.extend_from_slice(&0u16.to_be_bytes());
        let mdhd = full(b"mdhd", 1, 0, &mdhd);

        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(if t.spec.is_video() { b"vide" } else { b"soun" });
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.extend_from_slice(t.spec.name.as_bytes());
        hdlr.push(0);
        let hdlr = full(b"hdlr", 0, 0, &hdlr);

        let media_header = if t.spec.is_video() { full(b"vmhd", 0, 1, &[0; 8]) } else { full(b"smhd", 0, 0, &[0; 4]) };
        let dinf = bx(b"dinf", &full(b"dref", 0, 0, &cat(&[1u32.to_be_bytes().to_vec(), full(b"url ", 0, 1, &[])])));
        let stbl = self.stbl(t, durs);
        let minf = bx(b"minf", &cat(&[media_header, dinf, stbl]));
        let mdia = bx(b"mdia", &cat(&[mdhd, hdlr, minf]));
        bx(b"trak", &cat(&[tkhd, edts, mdia]))
    }

    fn sample_entry(t: &TrackState) -> Vec<u8> {
        match &t.spec.kind {
            TrackKind::Video { codec, width, height, config } => {
                let mut v = vec![0; 6];
                v.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
                v.extend_from_slice(&[0; 16]);
                v.extend_from_slice(&width.to_be_bytes());
                v.extend_from_slice(&height.to_be_bytes());
                v.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                v.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                v.extend_from_slice(&0u32.to_be_bytes());
                v.extend_from_slice(&1u16.to_be_bytes()); // frame_count
                let mut name = [0u8; 32];
                let label = b"rbuf";
                name[0] = label.len() as u8;
                name[1..1 + label.len()].copy_from_slice(label);
                v.extend_from_slice(&name);
                v.extend_from_slice(&0x0018u16.to_be_bytes());
                v.extend_from_slice(&(-1i16).to_be_bytes());
                v.extend(bx(codec.config_box(), config));
                // BT.709 primaries, transfer and matrix, limited range.
                let mut colr = b"nclx".to_vec();
                colr.extend_from_slice(&[0, 1, 0, 1, 0, 1, 0]);
                v.extend(bx(b"colr", &colr));
                bx(codec.sample_entry(), &v)
            }
            TrackKind::Audio { rate, channels, asc, bitrate } => {
                let mut v = vec![0; 6];
                v.extend_from_slice(&1u16.to_be_bytes());
                v.extend_from_slice(&[0; 8]);
                v.extend_from_slice(&channels.to_be_bytes());
                v.extend_from_slice(&16u16.to_be_bytes());
                v.extend_from_slice(&[0; 4]);
                v.extend_from_slice(&(rate << 16).to_be_bytes());
                v.extend(esds(asc, *bitrate));
                bx(b"mp4a", &v)
            }
        }
    }

    fn stbl(&self, t: &TrackState, durs: &[u32]) -> Vec<u8> {
        let stsd = full(b"stsd", 0, 0, &cat(&[1u32.to_be_bytes().to_vec(), Self::sample_entry(t)]));

        let mut runs: Vec<(u32, u32)> = Vec::new();
        for d in durs {
            match runs.last_mut() {
                Some((n, v)) if v == d => *n += 1,
                _ => runs.push((1, *d)),
            }
        }
        let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
        for (n, d) in &runs {
            stts.extend_from_slice(&n.to_be_bytes());
            stts.extend_from_slice(&d.to_be_bytes());
        }
        let stts = full(b"stts", 0, 0, &stts);

        let mut parts = vec![stsd, stts];
        if t.spec.is_video() && t.samples.iter().any(|s| !s.key) {
            let keys: Vec<u32> = t.samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect();
            let mut b = (keys.len() as u32).to_be_bytes().to_vec();
            for k in keys {
                b.extend_from_slice(&k.to_be_bytes());
            }
            parts.push(full(b"stss", 0, 0, &b));
        }

        // Chunks: runs of this track's samples that are contiguous in the file.
        let mut chunks: Vec<(u64, u32)> = Vec::new();
        for s in &t.samples {
            match chunks.last_mut() {
                Some((off, n)) if *off + t.samples_len_before(*off, *n) == s.offset => *n += 1,
                _ => chunks.push((s.offset, 1)),
            }
        }
        let mut stsc_entries: Vec<(u32, u32)> = Vec::new();
        for (i, (_, n)) in chunks.iter().enumerate() {
            if stsc_entries.last().map(|e| e.1) != Some(*n) {
                stsc_entries.push((i as u32 + 1, *n));
            }
        }
        let mut stsc = (stsc_entries.len() as u32).to_be_bytes().to_vec();
        for (first, n) in stsc_entries {
            stsc.extend_from_slice(&first.to_be_bytes());
            stsc.extend_from_slice(&n.to_be_bytes());
            stsc.extend_from_slice(&1u32.to_be_bytes());
        }
        parts.push(full(b"stsc", 0, 0, &stsc));

        let mut stsz = 0u32.to_be_bytes().to_vec();
        stsz.extend_from_slice(&(t.samples.len() as u32).to_be_bytes());
        for s in &t.samples {
            stsz.extend_from_slice(&s.size.to_be_bytes());
        }
        parts.push(full(b"stsz", 0, 0, &stsz));

        let mut co64 = (chunks.len() as u32).to_be_bytes().to_vec();
        for (off, _) in &chunks {
            co64.extend_from_slice(&off.to_be_bytes());
        }
        parts.push(full(b"co64", 0, 0, &co64));
        bx(b"stbl", &cat(&parts))
    }
}

impl TrackState {
    /// Bytes of the `n` samples of this track that start at `offset`.
    fn samples_len_before(&self, offset: u64, n: u32) -> u64 {
        let i = self.samples.partition_point(|s| s.offset < offset);
        self.samples[i..i + n as usize].iter().map(|s| s.size as u64).sum()
    }
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    // Sizes in the four-byte form many muxers use.
    let n = body.len() as u32;
    let mut v =
        vec![tag, 0x80 | (n >> 21) as u8 & 0x7f, 0x80 | (n >> 14) as u8 & 0x7f, 0x80 | (n >> 7) as u8 & 0x7f, n as u8 & 0x7f];
    v.extend_from_slice(body);
    v
}

fn esds(asc: &[u8], bitrate: u32) -> Vec<u8> {
    let mut dcd = vec![0x40, 0x15, 0, 0x18, 0]; // MPEG-4 audio, audio stream, 6144-byte buffer
    dcd.extend_from_slice(&bitrate.to_be_bytes());
    dcd.extend_from_slice(&bitrate.to_be_bytes());
    dcd.extend(descriptor(5, asc));
    let mut es = vec![0, 0, 0]; // ES_ID 0, no flags
    es.extend(descriptor(4, &dcd));
    es.extend(descriptor(6, &[2]));
    full(b"esds", 0, 0, &descriptor(3, &es))
}

/// Reads the box tree of an MP4 file: (depth, type, size) per box, for tests and diagnostics.
pub fn boxes(data: &[u8]) -> Vec<(usize, String, u64)> {
    fn walk(d: &[u8], depth: usize, out: &mut Vec<(usize, String, u64)>) {
        let mut at = 0usize;
        while at + 8 <= d.len() {
            let mut size = u32::from_be_bytes(d[at..at + 4].try_into().unwrap()) as u64;
            let kind = String::from_utf8_lossy(&d[at + 4..at + 8]).into_owned();
            let mut hl = 8;
            if size == 1 && at + 16 <= d.len() {
                size = u64::from_be_bytes(d[at + 8..at + 16].try_into().unwrap());
                hl = 16;
            }
            if size < hl as u64 || at as u64 + size > d.len() as u64 {
                out.push((depth, format!("{kind}!bad"), size));
                return;
            }
            out.push((depth, kind.clone(), size));
            let body = &d[at + hl..at + size as usize];
            let skip = match kind.as_str() {
                "moov" | "trak" | "mdia" | "minf" | "stbl" | "edts" | "dinf" => Some(0),
                "stsd" => Some(8),
                "avc1" | "hvc1" | "av01" => Some(78),
                "mp4a" => Some(28),
                _ => None,
            };
            if let Some(s) = skip {
                if body.len() >= s {
                    walk(&body[s..], depth + 1, out);
                }
            }
            at += size as usize;
        }
    }
    let mut out = Vec::new();
    walk(data, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn video() -> TrackSpec {
        TrackSpec {
            kind: TrackKind::Video { codec: VideoCodec::H264, width: 320, height: 180, config: vec![1, 2, 3] },
            name: "screen".into(),
        }
    }

    fn audio() -> TrackSpec {
        TrackSpec {
            kind: TrackKind::Audio { rate: 48000, channels: 2, asc: vec![0x11, 0x90], bitrate: 192_000 },
            name: "system".into(),
        }
    }

    #[test]
    fn rounding_to_timescales() {
        assert_eq!(ticks_to(10_000_000, 90_000), 90_000);
        assert_eq!(ticks_to(166_667, 90_000), 1500); // 1/60 s
        assert_eq!(ticks_to(-5, 1000), 0);
        assert_eq!(ticks_to(213_333, 48_000), 1024);
    }

    #[test]
    fn box_tree_has_everything_a_player_needs() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![video(), audio()]).unwrap();
        for i in 0..10i64 {
            w.write_sample(0, i * 333_333, &[i as u8; 100], i % 5 == 0).unwrap();
            w.write_sample(1, 1_000_000 + i * 213_333, &[7; 20], true).unwrap();
        }
        let data = w.finish().unwrap().into_inner();
        let b = boxes(&data);
        let names: Vec<&str> = b.iter().map(|x| x.1.as_str()).collect();
        for n in ["ftyp", "mdat", "moov", "mvhd", "trak", "tkhd", "edts", "mdia", "stbl", "avcC", "colr", "esds", "stss", "co64"]
        {
            assert!(names.contains(&n), "missing {n}: {names:?}");
        }
        assert!(!names.iter().any(|n| n.ends_with("!bad")), "{names:?}");
        // mdat size covers exactly the sample bytes and its 16-byte header.
        let mdat = b.iter().find(|x| x.1 == "mdat").unwrap();
        assert_eq!(mdat.2, 16 + 10 * 100 + 10 * 20);
        // Only the audio track starts later, so it alone has an edit list.
        assert_eq!(names.iter().filter(|n| **n == "edts").count(), 1);
    }

    #[test]
    fn timestamps_must_increase() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![video()]).unwrap();
        w.write_sample(0, 1000, &[1], true).unwrap();
        assert!(w.write_sample(0, 1000, &[1], false).is_err());
        assert!(w.write_sample(3, 2000, &[1], false).is_err());
    }

    #[test]
    fn interleaved_samples_make_chunks_per_run() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![video(), audio()]).unwrap();
        // v v a a v a: video chunks of 2 and 1, audio chunks of 2 and 1.
        w.write_sample(0, 0, &[0; 10], true).unwrap();
        w.write_sample(0, 333_333, &[0; 10], false).unwrap();
        w.write_sample(1, 0, &[0; 4], true).unwrap();
        w.write_sample(1, 213_333, &[0; 4], true).unwrap();
        w.write_sample(0, 666_666, &[0; 10], false).unwrap();
        w.write_sample(1, 426_666, &[0; 4], true).unwrap();
        let t = &w.tracks[0];
        let mut chunks: Vec<(u64, u32)> = Vec::new();
        for s in &t.samples {
            match chunks.last_mut() {
                Some((off, n)) if *off + t.samples_len_before(*off, *n) == s.offset => *n += 1,
                _ => chunks.push((s.offset, 1)),
            }
        }
        assert_eq!(chunks.iter().map(|c| c.1).collect::<Vec<_>>(), vec![2, 1]);
        let data = w.finish().unwrap().into_inner();
        assert!(boxes(&data).iter().all(|b| !b.1.ends_with("!bad")));
    }
}
