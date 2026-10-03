//! AV1 bitstreams (low-overhead OBU format): splitting temporal units into OBUs, reading the
//! sequence header, telling key frames apart, and building `av1C` for MP4. An MP4 sample is a
//! temporal unit without its temporal delimiter, every OBU carrying its size field.

use crate::bits::BitReader;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;
pub const OBU_PADDING: u8 = 15;

/// One OBU: its type and its bytes, header included.
#[derive(Clone, Debug, PartialEq)]
pub struct Obu<'a> {
    pub kind: u8,
    /// Whole OBU (header, optional size field, payload).
    pub bytes: &'a [u8],
    /// Payload only.
    pub payload: &'a [u8],
    pub has_size: bool,
    pub header_len: usize,
}

fn leb128(d: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(i)?;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

pub fn write_leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

/// Splits a buffer of OBUs. An OBU without a size field runs to the end of the buffer.
pub fn split(data: &[u8]) -> Option<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let h = data[at];
        if h & 0x80 != 0 {
            return None; // forbidden bit
        }
        let kind = (h >> 3) & 0x0f;
        let ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let mut hl = 1 + ext as usize;
        let size = if has_size {
            let (s, n) = leb128(data.get(at + hl..)?)?;
            hl += n;
            s as usize
        } else {
            data.len() - at - hl
        };
        let end = at.checked_add(hl)?.checked_add(size)?;
        let bytes = data.get(at..end)?;
        out.push(Obu { kind, bytes, payload: &bytes[hl..], has_size, header_len: hl });
        at = end;
    }
    Some(out)
}

/// The sequence header fields that `av1C` repeats and frame parsing needs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SequenceHeader {
    pub profile: u8,
    pub level_idx0: u8,
    pub tier0: u8,
    pub reduced_still_picture_header: bool,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub max_width: u32,
    pub max_height: u32,
    /// Bit offset of `max_frame_width_minus_1` in the payload, and the widths of it and of
    /// `max_frame_height_minus_1`.
    pub size_field: (usize, u32, u32),
}

pub fn parse_sequence_header(p: &[u8]) -> Option<SequenceHeader> {
    let mut r = BitReader::new(p);
    let mut s = SequenceHeader { profile: r.u(3)? as u8, ..Default::default() };
    r.bit()?; // still_picture
    s.reduced_still_picture_header = r.flag()?;
    let mut decoder_model_info = false;
    let mut buffer_delay_len = 0;
    if s.reduced_still_picture_header {
        s.level_idx0 = r.u(5)? as u8;
    } else {
        if r.flag()? {
            // timing_info
            r.skip(64)?;
            if r.flag()? {
                r.uvlc()?;
            }
            decoder_model_info = r.flag()?;
            if decoder_model_info {
                buffer_delay_len = r.u(5)? + 1;
                r.skip(32)?;
                r.skip(10)?;
            }
        }
        let initial_display_delay = r.flag()?;
        let ops = r.u(5)? + 1;
        for i in 0..ops {
            r.u(12)?; // operating_point_idc
            let level = r.u(5)? as u8;
            let tier = if level > 7 { r.u(1)? as u8 } else { 0 };
            if i == 0 {
                s.level_idx0 = level;
                s.tier0 = tier;
            }
            if decoder_model_info && r.flag()? {
                r.skip(2 * buffer_delay_len as usize + 1)?;
            }
            if initial_display_delay && r.flag()? {
                r.u(4)?;
            }
        }
    }
    let wb = r.u(4)? + 1;
    let hb = r.u(4)? + 1;
    s.size_field = (r.position(), wb, hb);
    s.max_width = r.u(wb)? + 1;
    s.max_height = r.u(hb)? + 1;
    let mut order_hint = false;
    // frame_id_numbers_present_flag and its two lengths.
    if !s.reduced_still_picture_header && r.flag()? {
        r.u(4)?;
        r.u(3)?;
    }
    r.skip(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !s.reduced_still_picture_header {
        r.skip(4)?; // interintra, masked, warped, dual filter
        order_hint = r.flag()?;
        if order_hint {
            r.skip(2)?;
        }
        let force_sct = if r.flag()? { 2 } else { r.u(1)? };
        if force_sct > 0 && !r.flag()? {
            r.bit()?;
        }
        if order_hint {
            r.u(3)?;
        }
    }
    let _ = order_hint;
    r.skip(3)?; // superres, cdef, restoration
                // color_config
    s.high_bitdepth = r.flag()?;
    if s.profile == 2 && s.high_bitdepth {
        s.twelve_bit = r.flag()?;
    }
    s.monochrome = if s.profile == 1 { false } else { r.flag()? };
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if r.flag()? {
        cp = r.u(8)?;
        tc = r.u(8)?;
        mc = r.u(8)?;
    }
    if s.monochrome {
        s.subsampling_x = true;
        s.subsampling_y = true;
        return Some(s);
    }
    if cp == 1 && tc == 13 && mc == 0 {
        return Some(s); // sRGB: 4:4:4
    }
    r.bit()?; // color_range
    match s.profile {
        0 => {
            s.subsampling_x = true;
            s.subsampling_y = true;
        }
        1 => {}
        _ => {
            if s.twelve_bit {
                s.subsampling_x = r.flag()?;
                s.subsampling_y = if s.subsampling_x { r.flag()? } else { false };
            } else {
                s.subsampling_x = true;
            }
        }
    }
    if s.subsampling_x && s.subsampling_y {
        s.chroma_sample_position = r.u(2)? as u8;
    }
    Some(s)
}

fn put_bits(buf: &mut [u8], pos: usize, n: u32, v: u32) {
    for i in 0..n as usize {
        let bit = (v >> (n as usize - 1 - i)) & 1;
        let (byte, shift) = ((pos + i) / 8, 7 - (pos + i) % 8);
        buf[byte] = (buf[byte] & !(1 << shift)) | ((bit as u8) << shift);
    }
}

/// Rewrites the maximum frame size in a sequence header OBU to the real picture size.
///
/// NVIDIA's AV1 encoder transform declares 1920x1088 as the maximum whatever it encodes and gives
/// every frame its real size. That is valid AV1, but players and ffprobe report the maximum as the
/// video's size. The fields have fixed widths, so smaller values are written in place.
pub fn set_max_frame_size(obu: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let o = split(obu)?.into_iter().next()?;
    let hdr = parse_sequence_header(o.payload)?;
    let (pos, wb, hb) = hdr.size_field;
    if w == 0 || h == 0 || (w - 1) >> wb != 0 || (h - 1) >> hb != 0 {
        return None;
    }
    let mut out = o.bytes.to_vec();
    let payload = &mut out[o.header_len..];
    put_bits(payload, pos, wb, w - 1);
    put_bits(payload, pos + wb as usize, hb, h - 1);
    Some(out)
}

/// `AV1CodecConfigurationRecord` with the sequence header OBU as its config OBU.
pub fn av1c(seq: &SequenceHeader, seq_obu: &[u8]) -> Vec<u8> {
    let mut v = vec![
        0x81,
        (seq.profile << 5) | seq.level_idx0,
        (seq.tier0 << 7)
            | ((seq.high_bitdepth as u8) << 6)
            | ((seq.twelve_bit as u8) << 5)
            | ((seq.monochrome as u8) << 4)
            | ((seq.subsampling_x as u8) << 3)
            | ((seq.subsampling_y as u8) << 2)
            | seq.chroma_sample_position,
        0,
    ];
    v.extend_from_slice(seq_obu);
    v
}

/// Whether a frame (or frame header) OBU starts a key frame that is shown.
pub fn is_key_frame(seq: &SequenceHeader, payload: &[u8]) -> bool {
    if seq.reduced_still_picture_header {
        return true;
    }
    let mut r = BitReader::new(payload);
    match r.flag() {
        Some(false) => r.u(2) == Some(0),
        _ => false, // show_existing_frame
    }
}

/// Rewrites an OBU so it carries a size field (MP4 requires it) and drops nothing else.
pub fn with_size(o: &Obu) -> Vec<u8> {
    if o.has_size {
        return o.bytes.to_vec();
    }
    let mut v = vec![o.bytes[0] | 0x02];
    v.extend_from_slice(&o.bytes[1..o.header_len]);
    write_leb128(o.payload.len() as u64, &mut v);
    v.extend_from_slice(o.payload);
    v
}

/// A temporal unit turned into an MP4 sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Converts one temporal unit, remembering the latest sequence header (header fields and OBU).
pub fn to_sample(tu: &[u8], seq: &mut Option<(SequenceHeader, Vec<u8>)>) -> Option<Sample> {
    to_sample_sized(tu, seq, None)
}

/// Like [`to_sample`], also setting the sequence header's maximum frame size to `size`.
pub fn to_sample_sized(tu: &[u8], seq: &mut Option<(SequenceHeader, Vec<u8>)>, size: Option<(u32, u32)>) -> Option<Sample> {
    let mut data = Vec::with_capacity(tu.len());
    let mut keyframe = false;
    for o in split(tu)? {
        match o.kind {
            OBU_TEMPORAL_DELIMITER | OBU_PADDING => continue,
            OBU_SEQUENCE_HEADER => {
                let mut bytes = with_size(&o);
                if let Some((w, h)) = size {
                    if let Some(fixed) = set_max_frame_size(&bytes, w, h) {
                        bytes = fixed;
                    }
                }
                if let Some(h) = split(&bytes).and_then(|v| v.into_iter().next()).and_then(|x| parse_sequence_header(x.payload)) {
                    *seq = Some((h, bytes.clone()));
                }
                data.extend_from_slice(&bytes);
                continue;
            }
            OBU_FRAME | OBU_FRAME_HEADER => {
                if let Some((h, _)) = seq.as_ref() {
                    keyframe |= is_key_frame(h, o.payload);
                }
            }
            _ => {}
        }
        data.extend_from_slice(&with_size(&o));
    }
    Some(Sample { data, keyframe })
}

/// Splits a low-overhead OBU stream (as `ffmpeg -f obu` writes it) into temporal units.
pub fn temporal_units(data: &[u8]) -> Option<Vec<&[u8]>> {
    let obus = split(data)?;
    let mut out = Vec::new();
    let base = data.as_ptr() as usize;
    let mut start: Option<usize> = None;
    for o in &obus {
        let at = o.bytes.as_ptr() as usize - base;
        if o.kind == OBU_TEMPORAL_DELIMITER {
            if let Some(s) = start {
                out.push(&data[s..at]);
            }
            start = Some(at);
        }
    }
    if let Some(s) = start {
        out.push(&data[s..]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/av1.obu", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn leb128_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 1 << 20, (1 << 35) + 5] {
            let mut b = Vec::new();
            write_leb128(v, &mut b);
            assert_eq!(leb128(&b), Some((v, b.len())));
        }
        assert_eq!(leb128(&[0x80, 0x80]), None);
    }

    #[test]
    fn fixture_splits_into_sixty_temporal_units_with_keyframes_every_thirty() {
        let data = fixture();
        let tus = temporal_units(&data).unwrap();
        assert_eq!(tus.len(), 60);
        let mut seq = None;
        let keys: Vec<usize> =
            tus.iter().enumerate().filter_map(|(i, tu)| to_sample(tu, &mut seq).unwrap().keyframe.then_some(i)).collect();
        assert_eq!(keys[0], 0);
        assert!(keys.contains(&30), "{keys:?}");
        let (h, obu) = seq.unwrap();
        assert_eq!((h.max_width, h.max_height, h.profile), (320, 180, 0));
        assert!(h.subsampling_x && h.subsampling_y && !h.monochrome && !h.high_bitdepth);
        let c = av1c(&h, &obu);
        assert_eq!(c[0], 0x81);
        assert_eq!(c[2] & 0x0c, 0x0c);
        assert_eq!(&c[4..], &obu[..]);
    }

    #[test]
    fn the_maximum_frame_size_is_rewritten_in_place() {
        let data = fixture();
        let tus = temporal_units(&data).unwrap();
        let mut seq = None;
        to_sample(tus[0], &mut seq).unwrap();
        let (h, obu) = seq.unwrap();
        assert_eq!((h.max_width, h.max_height), (320, 180));
        let fixed = set_max_frame_size(&obu, 300, 170).unwrap();
        assert_eq!(fixed.len(), obu.len());
        let o = &split(&fixed).unwrap()[0];
        let h2 = parse_sequence_header(o.payload).unwrap();
        assert_eq!((h2.max_width, h2.max_height), (300, 170));
        // Everything else is unchanged.
        assert_eq!(SequenceHeader { max_width: 320, max_height: 180, ..h2.clone() }, h);
        // A size that does not fit the field is refused.
        assert!(set_max_frame_size(&obu, 1 << 16, 10).is_none());
        // Through to_sample_sized, the sample and the remembered header both carry the new size.
        let mut seq = None;
        let s = to_sample_sized(tus[0], &mut seq, Some((300, 170))).unwrap();
        assert_eq!(seq.as_ref().unwrap().0.max_width, 300);
        let in_sample = split(&s.data).unwrap().into_iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).unwrap();
        assert_eq!(parse_sequence_header(in_sample.payload).unwrap().max_height, 170);
    }

    #[test]
    fn samples_drop_temporal_delimiters_and_every_obu_has_a_size() {
        let data = fixture();
        let tus = temporal_units(&data).unwrap();
        let mut seq = None;
        let s = to_sample(tus[1], &mut seq).unwrap();
        for o in split(&s.data).unwrap() {
            assert!(o.has_size && o.kind != OBU_TEMPORAL_DELIMITER);
        }
        // An OBU without a size field gets one.
        let bare = [OBU_FRAME << 3, 1, 2, 3];
        let o = &split(&bare).unwrap()[0];
        assert_eq!(with_size(o), vec![(OBU_FRAME << 3) | 2, 3, 1, 2, 3]);
    }
}
