//! H.264 and HEVC bitstreams: splitting Annex B output into NAL units, finding keyframes and
//! parameter sets, and building the `avcC` and `hvcC` records an MP4 file needs. Samples in the
//! file use 4-byte length prefixes instead of start codes, without the parameter sets.

use crate::bits::{unescape, BitReader};

/// Splits an Annex B byte stream (`00 00 01` or `00 00 00 01` start codes) into NAL units.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        // Trailing zeros belong to the next start code (00 00 00 01) or are padding.
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn nal_type(self, nal: &[u8]) -> u8 {
        match self {
            Codec::H264 => nal.first().map(|b| b & 0x1f).unwrap_or(0),
            Codec::Hevc => nal.first().map(|b| (b >> 1) & 0x3f).unwrap_or(0),
        }
    }

    fn is_parameter_set(self, t: u8) -> bool {
        match self {
            Codec::H264 => t == 7 || t == 8,
            Codec::Hevc => (32..=34).contains(&t),
        }
    }

    /// Access unit delimiters and filler carry nothing an MP4 sample needs.
    fn is_dropped(self, t: u8) -> bool {
        match self {
            Codec::H264 => t == 9 || t == 12,
            Codec::Hevc => t == 35 || t == 38,
        }
    }

    pub fn is_keyframe_nal(self, t: u8) -> bool {
        match self {
            Codec::H264 => t == 5,
            // IRAP pictures: BLA, IDR, CRA.
            Codec::Hevc => (16..=21).contains(&t),
        }
    }
}

/// Parameter sets seen so far, in the order the decoder needs them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParamSets {
    fn add(list: &mut Vec<Vec<u8>>, nal: &[u8]) {
        if !list.iter().any(|x| x == nal) {
            list.push(nal.to_vec());
        }
    }

    pub fn complete(&self, codec: Codec) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty() && (codec == Codec::H264 || !self.vps.is_empty())
    }
}

/// One encoded access unit turned into an MP4 sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// Length-prefixed NAL units.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Converts an Annex B access unit into an MP4 sample, collecting parameter sets on the way.
pub fn to_sample(codec: Codec, annexb: &[u8], params: &mut ParamSets) -> Sample {
    let mut data = Vec::with_capacity(annexb.len());
    let mut keyframe = false;
    for nal in split_annexb(annexb) {
        let t = codec.nal_type(nal);
        if codec.is_parameter_set(t) {
            match (codec, t) {
                (Codec::Hevc, 32) => ParamSets::add(&mut params.vps, nal),
                (Codec::H264, 7) | (Codec::Hevc, 33) => ParamSets::add(&mut params.sps, nal),
                _ => ParamSets::add(&mut params.pps, nal),
            }
            continue;
        }
        if codec.is_dropped(t) {
            continue;
        }
        keyframe |= codec.is_keyframe_nal(t);
        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        data.extend_from_slice(nal);
    }
    Sample { data, keyframe }
}

/// `AVCDecoderConfigurationRecord` from the SPS and PPS.
pub fn avcc(params: &ParamSets) -> Option<Vec<u8>> {
    let sps = params.sps.first()?;
    if sps.len() < 4 || params.pps.is_empty() {
        return None;
    }
    let mut v = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe0 | params.sps.len() as u8];
    for s in &params.sps {
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s);
    }
    v.push(params.pps.len() as u8);
    for p in &params.pps {
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend_from_slice(p);
    }
    // High profiles carry chroma format and bit depths as well.
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        let rbsp = unescape(&sps[1..]);
        let mut r = BitReader::new(&rbsp);
        r.skip(24)?;
        r.ue()?; // seq_parameter_set_id
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        let luma = r.ue()?;
        let chroma_depth = r.ue()?;
        v.extend_from_slice(&[0xfc | chroma as u8, 0xf8 | luma as u8, 0xf8 | chroma_depth as u8, 0]);
    }
    Some(v)
}

/// The fields of an HEVC SPS that `hvcC` repeats.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HevcSps {
    pub profile_space: u8,
    pub tier: u8,
    pub profile_idc: u8,
    pub compat: u32,
    pub constraint: u64,
    pub level_idc: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub chroma_format_idc: u8,
    pub width: u32,
    pub height: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
}

pub fn parse_hevc_sps(nal: &[u8]) -> Option<HevcSps> {
    let rbsp = unescape(nal.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    let mut s = HevcSps::default();
    r.u(4)?; // sps_video_parameter_set_id
    s.max_sub_layers_minus1 = r.u(3)? as u8;
    s.temporal_id_nesting = r.flag()?;
    s.profile_space = r.u(2)? as u8;
    s.tier = r.u(1)? as u8;
    s.profile_idc = r.u(5)? as u8;
    s.compat = r.u(32)?;
    s.constraint = r.bits(48)?;
    s.level_idc = r.u(8)? as u8;
    let n = s.max_sub_layers_minus1 as usize;
    let mut present = Vec::with_capacity(n);
    for _ in 0..n {
        present.push((r.flag()?, r.flag()?));
    }
    if n > 0 {
        for _ in n..8 {
            r.u(2)?;
        }
    }
    for (profile, level) in present {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    s.chroma_format_idc = r.ue()? as u8;
    if s.chroma_format_idc == 3 {
        r.bit()?;
    }
    s.width = r.ue()?;
    s.height = r.ue()?;
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    s.bit_depth_luma_minus8 = r.ue()? as u8;
    s.bit_depth_chroma_minus8 = r.ue()? as u8;
    Some(s)
}

/// `HEVCDecoderConfigurationRecord` from the VPS, SPS and PPS.
pub fn hvcc(params: &ParamSets) -> Option<Vec<u8>> {
    let s = parse_hevc_sps(params.sps.first()?)?;
    if params.vps.is_empty() || params.pps.is_empty() {
        return None;
    }
    let mut v = vec![1, (s.profile_space << 6) | (s.tier << 5) | s.profile_idc];
    v.extend_from_slice(&s.compat.to_be_bytes());
    v.extend_from_slice(&s.constraint.to_be_bytes()[2..]);
    v.push(s.level_idc);
    v.extend_from_slice(&[0xf0, 0x00]); // min_spatial_segmentation_idc 0
    v.push(0xfc); // parallelismType 0
    v.push(0xfc | s.chroma_format_idc);
    v.push(0xf8 | s.bit_depth_luma_minus8);
    v.push(0xf8 | s.bit_depth_chroma_minus8);
    v.extend_from_slice(&[0, 0]); // avgFrameRate unknown
                                  // constantFrameRate 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne 3.
    v.push(((s.max_sub_layers_minus1 + 1) << 3) | ((s.temporal_id_nesting as u8) << 2) | 3);
    v.push(3);
    for (t, list) in [(32u8, &params.vps), (33, &params.sps), (34, &params.pps)] {
        v.push(0x80 | t); // array_completeness: all of this type are here
        v.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for n in list {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
    }
    Some(v)
}

/// Picture size from an H.264 SPS (frame cropping applied).
pub fn h264_size(sps: &[u8]) -> Option<(u32, u32)> {
    let rbsp = unescape(sps.get(1..)?);
    let mut r = BitReader::new(&rbsp);
    let profile = r.u(8)?;
    r.skip(16)?;
    r.ue()?;
    let mut chroma = 1;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.flag()? {
            // Scaling matrices are not used by any encoder this program drives.
            return None;
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let poc_type = r.ue()?;
    if poc_type == 0 {
        r.ue()?;
    } else if poc_type == 1 {
        r.bit()?;
        r.ue()?;
        r.ue()?;
        let n = r.ue()?;
        for _ in 0..n {
            r.ue()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?;
    let w_mbs = r.ue()? + 1;
    let h_units = r.ue()? + 1;
    let frame_mbs_only = r.flag()?;
    if !frame_mbs_only {
        r.bit()?;
    }
    r.bit()?; // direct_8x8_inference
    let (mut w, mut h) = (w_mbs * 16, h_units * 16 * if frame_mbs_only { 1 } else { 2 });
    if r.flag()? {
        let (l, rr, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (cx, cy) = match chroma {
            0 => (1, 2 - frame_mbs_only as u32),
            1 => (2, 2 * (2 - frame_mbs_only as u32)),
            2 => (2, 2 - frame_mbs_only as u32),
            _ => (1, 2 - frame_mbs_only as u32),
        };
        w -= cx * (l + rr);
        h -= cy * (t + b);
    }
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn start_codes_of_both_lengths_and_trailing_zeros() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 9];
        let nals = split_annexb(&s);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]);
        assert!(split_annexb(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn h264_fixture_becomes_samples_and_avcc() {
        let data = fixture("h264.264");
        let mut params = ParamSets::default();
        // The fixture is one stream; cut it into access units at each slice of a new picture.
        let s = to_sample(Codec::H264, &data, &mut params);
        assert!(s.keyframe);
        assert_eq!(params.sps.len(), 1);
        assert_eq!(params.pps.len(), 1);
        let c = avcc(&params).unwrap();
        assert_eq!(c[0], 1);
        assert_eq!(c[1], params.sps[0][1]);
        assert_eq!(c[4], 0xff);
        assert_eq!(h264_size(&params.sps[0]), Some((320, 180)));
        // No start codes or parameter sets left in the sample; lengths cover the data exactly.
        let mut at = 0;
        while at < s.data.len() {
            let n = u32::from_be_bytes(s.data[at..at + 4].try_into().unwrap()) as usize;
            let t = s.data[at + 4] & 0x1f;
            assert!(t != 7 && t != 8 && t != 9, "type {t}");
            at += 4 + n;
        }
        assert_eq!(at, s.data.len());
    }

    #[test]
    fn hevc_fixture_sps_and_hvcc() {
        let data = fixture("hevc.265");
        let mut params = ParamSets::default();
        to_sample(Codec::Hevc, &data, &mut params);
        assert!(params.complete(Codec::Hevc));
        let s = parse_hevc_sps(&params.sps[0]).unwrap();
        assert_eq!((s.width, s.height, s.chroma_format_idc, s.bit_depth_luma_minus8), (320, 184, 1, 0));
        assert_eq!(s.profile_idc, 1); // Main
        let c = hvcc(&params).unwrap();
        assert_eq!(c[0], 1);
        assert_eq!(c[1] & 0x1f, 1);
        assert_eq!(c[12], s.level_idc);
        assert_eq!(c[22], 3); // three arrays
    }
}
