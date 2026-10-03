//! The replay buffer itself: encoded packets in memory, oldest evicted first, always a whole
//! group of pictures at a time, so whatever is saved starts on a keyframe and decodes.
//!
//! Stream 0 is video; the others are audio tracks. Time is in 100 ns ticks.

use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub pts: i64,
    pub key: bool,
    pub data: Arc<[u8]>,
}

/// What a save takes out of the buffer: clones of the packets (the data is shared, not copied).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// One list per stream, in presentation order. Video starts on a keyframe.
    pub streams: Vec<Vec<Packet>>,
}

impl Snapshot {
    pub fn duration_ticks(&self) -> i64 {
        match self.streams.first().and_then(|v| Some((v.first()?, v.last()?))) {
            Some((a, b)) => b.pts - a.pts,
            None => 0,
        }
    }
}

pub struct Ring {
    /// How much history to keep, in ticks.
    keep: i64,
    /// Upper bound on the bytes held, whatever `keep` says.
    max_bytes: usize,
    streams: Vec<VecDeque<Packet>>,
    bytes: usize,
}

impl Ring {
    pub fn new(streams: usize, keep_ticks: i64, max_bytes: usize) -> Ring {
        Ring { keep: keep_ticks, max_bytes, streams: (0..streams.max(1)).map(|_| VecDeque::new()).collect(), bytes: 0 }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self, stream: usize) -> usize {
        self.streams.get(stream).map(|s| s.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.streams.iter().all(|s| s.is_empty())
    }

    /// Seconds of video held, from the first keyframe to the newest frame.
    pub fn held_ticks(&self) -> i64 {
        let v = &self.streams[0];
        match (v.front(), v.back()) {
            (Some(a), Some(b)) => b.pts - a.pts,
            _ => 0,
        }
    }

    pub fn push(&mut self, stream: usize, p: Packet) {
        let Some(s) = self.streams.get_mut(stream) else { return };
        // A video stream must start on a keyframe; anything before the first one is useless.
        if stream == 0 && s.is_empty() && !p.key {
            return;
        }
        self.bytes += p.data.len();
        s.push_back(p);
        if stream == 0 {
            self.evict();
        }
    }

    /// Start of the oldest group of pictures that must be kept: the latest keyframe at or before
    /// `newest - keep`, so the kept range always covers `keep` (plus up to one GOP).
    fn evict(&mut self) {
        let v = &self.streams[0];
        let Some(newest) = v.back().map(|p| p.pts) else { return };
        let cutoff = newest - self.keep;
        let mut start = v.iter().take_while(|p| p.pts <= cutoff).filter(|p| p.key).last().map(|p| p.pts);
        // Over the byte budget: drop whole GOPs from the front until it fits (keeping the last).
        if self.bytes > self.max_bytes {
            let keys: Vec<i64> = v.iter().filter(|p| p.key).map(|p| p.pts).collect();
            let mut freed = 0usize;
            for (i, k) in keys.iter().enumerate().skip(1) {
                let gop: usize = v.iter().filter(|p| p.pts >= keys[i - 1] && p.pts < *k).map(|p| p.data.len()).sum();
                freed += gop;
                start = Some(start.map_or(*k, |s| s.max(*k)));
                if self.bytes - freed <= self.max_bytes {
                    break;
                }
            }
        }
        if let Some(start) = start {
            self.drop_before(start);
        }
    }

    fn drop_before(&mut self, t: i64) {
        for s in &mut self.streams {
            while s.front().is_some_and(|p| p.pts < t) {
                let p = s.pop_front().unwrap();
                self.bytes -= p.data.len();
            }
        }
    }

    /// The last `ticks` of everything (from the keyframe at or before `newest - ticks`), or all
    /// of it when less is held.
    pub fn snapshot(&self, ticks: i64) -> Snapshot {
        let v = &self.streams[0];
        let Some(newest) = v.back().map(|p| p.pts) else { return Snapshot { streams: vec![Vec::new(); self.streams.len()] } };
        let cutoff = newest - ticks;
        let start = v
            .iter()
            .rev()
            .find(|p| p.key && p.pts <= cutoff)
            .or_else(|| v.iter().find(|p| p.key))
            .map(|p| p.pts)
            .unwrap_or(i64::MIN);
        Snapshot {
            streams: self
                .streams
                .iter()
                .map(|s| s.iter().filter(|p| p.pts >= start && p.pts <= newest).cloned().collect())
                .collect(),
        }
    }

    pub fn clear(&mut self) {
        for s in &mut self.streams {
            s.clear();
        }
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 10_000_000;

    fn pkt(pts: i64, key: bool, n: usize) -> Packet {
        Packet { pts, key, data: vec![0u8; n].into() }
    }

    /// 30 fps video with a keyframe every second, audio every 21.3 ms.
    fn fill(r: &mut Ring, seconds: i64) {
        let frame = S / 30;
        let mut audio_t = 0;
        for i in 0..seconds * 30 {
            let t = i * frame;
            while audio_t <= t {
                r.push(1, pkt(audio_t, true, 10));
                audio_t += 213_333;
            }
            r.push(0, pkt(t, i % 30 == 0, 100));
        }
    }

    #[test]
    fn keeps_the_requested_history_and_starts_on_a_keyframe() {
        let mut r = Ring::new(2, 5 * S, usize::MAX);
        fill(&mut r, 20);
        let v = &r.streams[0];
        assert!(v.front().unwrap().key);
        // At least 5 s, at most 5 s plus one GOP.
        assert!(r.held_ticks() >= 5 * S && r.held_ticks() < 6 * S, "{}", r.held_ticks());
        // Audio was trimmed to the same start.
        assert!(r.streams[1].front().unwrap().pts >= v.front().unwrap().pts);
        let total: usize = r.streams.iter().flatten().map(|p| p.data.len()).sum();
        assert_eq!(r.bytes(), total);
    }

    #[test]
    fn snapshots_cover_the_asked_length_from_a_keyframe() {
        let mut r = Ring::new(2, 30 * S, usize::MAX);
        fill(&mut r, 20);
        let s = r.snapshot(10 * S);
        assert!(s.streams[0][0].key);
        assert!(s.duration_ticks() >= 10 * S - S / 30, "{}", s.duration_ticks());
        assert!(s.duration_ticks() < 11 * S);
        // Asking for more than is held gives everything.
        assert_eq!(r.snapshot(100 * S).streams[0].len(), 600);
        // Snapshots share the packet data.
        assert!(Arc::ptr_eq(&s.streams[0][0].data, &r.snapshot(10 * S).streams[0][0].data));
    }

    #[test]
    fn the_byte_budget_drops_whole_groups() {
        // 30 frames of 100 bytes per second: 3,000 bytes per GOP (plus audio).
        let mut r = Ring::new(2, 60 * S, 10_000);
        fill(&mut r, 20);
        assert!(r.bytes() <= 10_000, "{}", r.bytes());
        assert!(r.streams[0].front().unwrap().key);
        // The budget fits under 3 s; at least one whole GOP is always kept.
        assert!(r.held_ticks() >= S - S / 30, "{}", r.held_ticks());
    }

    #[test]
    fn frames_before_the_first_keyframe_are_ignored() {
        let mut r = Ring::new(1, S, usize::MAX);
        r.push(0, pkt(0, false, 5));
        assert!(r.is_empty());
        r.push(0, pkt(1, true, 5));
        assert_eq!(r.len(0), 1);
        r.clear();
        assert_eq!((r.bytes(), r.len(0)), (0, 0));
        assert!(Ring::new(1, S, 100).snapshot(S).streams[0].is_empty());
    }
}
