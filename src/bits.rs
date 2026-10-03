//! A bit reader for codec headers: big-endian bits, Exp-Golomb codes, and removal of the
//! emulation-prevention bytes (`00 00 03`) that H.264 and HEVC insert into their payloads.

/// Removes emulation-prevention bytes: every `03` that follows `00 00`.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    pub fn bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Some(v)
    }

    pub fn u(&mut self, n: u32) -> Option<u32> {
        self.bits(n).map(|v| v as u32)
    }

    pub fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Unsigned Exp-Golomb (`ue(v)`).
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)? as u32;
        Some((1u32 << zeros) - 1 + rest)
    }

    /// AV1 `uvlc()`.
    pub fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        let rest = self.bits(zeros)? as u32;
        Some(((1u64 << zeros) - 1 + rest as u64) as u32)
    }

    pub fn position(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 5]), vec![0, 0, 1, 0, 0, 0, 5]);
        // A 03 not preceded by two zeros stays.
        assert_eq!(unescape(&[0, 3, 0, 0, 3]), vec![0, 3, 0, 0]);
        assert_eq!(unescape(&[]), Vec::<u8>::new());
    }

    #[test]
    fn bits_and_exp_golomb() {
        // 1 | 010 | 011 | 00100 | 0001000 -> ue 0, 1, 2, 3, 7
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0001, 0b0000_0000]);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(3));
        assert_eq!(r.ue(), Some(7));
        let mut r = BitReader::new(&[0xa5]);
        assert_eq!(r.u(4), Some(0xa));
        assert_eq!(r.flag(), Some(false));
        assert_eq!(r.u(3), Some(5));
        assert_eq!(r.bit(), None);
        assert!(BitReader::new(&[0, 0, 0, 0, 0]).ue().is_none());
    }
}
