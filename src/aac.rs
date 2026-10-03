//! AAC helpers: the `AudioSpecificConfig` an MP4 track needs, and an ADTS reader (ADTS is what
//! many tools write; raw AAC frames are what goes into the file).

const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// `AudioSpecificConfig` for AAC-LC.
pub fn audio_specific_config(rate: u32, channels: u8) -> Option<[u8; 2]> {
    let idx = RATES.iter().position(|r| *r == rate)? as u8;
    // object type 2 (5 bits), frequency index (4), channel configuration (4), 3 zero bits.
    Some([(2 << 3) | (idx >> 1), ((idx & 1) << 7) | (channels << 3)])
}

/// One ADTS frame: the raw AAC payload and the stream parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct AdtsFrame<'a> {
    pub payload: &'a [u8],
    pub rate: u32,
    pub channels: u8,
}

pub fn parse_adts(data: &[u8]) -> Option<Vec<AdtsFrame<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 7 <= data.len() {
        let h = &data[at..];
        if h[0] != 0xff || h[1] & 0xf0 != 0xf0 {
            return None;
        }
        let protection_absent = h[1] & 1 == 1;
        let rate = *RATES.get(((h[2] >> 2) & 0x0f) as usize)?;
        let channels = ((h[2] & 1) << 2) | (h[3] >> 6);
        let len = (((h[3] & 3) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] >> 5) as usize);
        let hl = if protection_absent { 7 } else { 9 };
        if len < hl || at + len > data.len() {
            return None;
        }
        out.push(AdtsFrame { payload: &data[at + hl..at + len], rate, channels });
        at += len;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_for_common_formats() {
        // 48 kHz stereo AAC-LC is the well-known 11 90.
        assert_eq!(audio_specific_config(48000, 2), Some([0x11, 0x90]));
        assert_eq!(audio_specific_config(44100, 2), Some([0x12, 0x10]));
        assert_eq!(audio_specific_config(44100, 1), Some([0x12, 0x08]));
        assert_eq!(audio_specific_config(12345, 2), None);
    }

    #[test]
    fn fixture_has_two_seconds_of_frames() {
        let d = std::fs::read(format!("{}/tests/fixtures/aac.adts", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let f = parse_adts(&d).unwrap();
        // 2 s at 48 kHz in 1024-sample frames, plus the encoder's priming frame.
        assert!((94..=96).contains(&f.len()), "{}", f.len());
        assert!(f.iter().all(|x| x.rate == 48000 && x.channels == 2));
        assert!(parse_adts(&[0xff, 0x00, 0, 0, 0, 0, 0]).is_none());
    }
}
