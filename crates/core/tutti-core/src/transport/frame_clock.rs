//! [`FrameClock`]: a playhead whose source of truth is an integer frame
//! count, the one every transport clock in the engine keeps.
//!
//! Doc 013 §6, "the frame is the source of truth". A clock that adds
//! `beats_per_sample` to its beat, per frame or per block, drifts: at 90 BPM
//! and 48 kHz frame 96 000 is exactly beat 3, and the sum reads
//! `2.999999999999891` after 1 500 chunks of 64 frames. A clip placed at
//! beat 3 then enters a chunk late, and a MIDI note at beat 1 a frame early.
//!
//! A `FrameClock` counts frames and derives its beat in closed form from the
//! origin of its current [`TimelineSegment`]. The segment restarts, at an
//! integer frame, on a seek, a tempo or rate change and a loop wrap; a
//! stopped transport simply does not count. So the beat at a frame is the
//! same `f64` whether the clock got there one frame at a time (a `Net`'s
//! `TransportClock`, the graph's `EnvClock`) or a block at a time (the graph
//! engine's walk, the offline timeline): they emit the same beats, bit for
//! bit, by construction rather than by agreeing to rounding.

use tutti_types::{Frame, Samples, TimelineSegment};

use super::state::LoopRange;
use crate::{Beat, Bpm, SampleRate};

/// A playhead as a frame count on a [`TimelineSegment`]. See the
/// [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameClock {
    /// The segment the beat is derived from, on this clock's frame count.
    segment: TimelineSegment,
    /// Frames rolled since the clock started, the segment's frame zero
    /// being wherever it started.
    at: Frame,
}

impl FrameClock {
    /// A clock at `beat`, at `tempo` and `sample_rate`.
    pub(crate) fn new(beat: Beat, tempo: Bpm, sample_rate: SampleRate) -> Self {
        Self {
            segment: TimelineSegment::new(Frame::ZERO, beat, tempo, sample_rate),
            at: Frame::ZERO,
        }
    }

    /// The clock a transport snapshot describes: its
    /// [`segment`](tutti_graph::Transport::segment) at `rate`, at the
    /// snapshot's frame on it. The graph's `EnvClock` continues the host's
    /// clock from here, frame by frame, with the host's arithmetic.
    pub(crate) fn of(transport: &tutti_graph::Transport, rate: SampleRate) -> Self {
        let (segment, at) = transport.segment(rate);
        Self { segment, at }
    }

    /// The beat, in closed form from the segment's origin.
    #[inline]
    pub(crate) fn beat(&self) -> Beat {
        self.segment.beat_at(self.at)
    }

    /// The tempo the beat moves at.
    #[inline]
    pub(crate) fn tempo(&self) -> Bpm {
        self.segment.tempo
    }

    /// The rate frames are counted at.
    #[inline]
    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.segment.sample_rate
    }

    /// Where the beat is counted from, for a transport snapshot: the
    /// segment's origin beat and the frames rolled since it.
    pub(crate) fn origin(&self) -> tutti_graph::SegmentOrigin {
        tutti_graph::SegmentOrigin {
            beat: self.segment.origin_beat,
            // `at` never falls behind the origin: every restart puts the
            // origin on `at`, and `at` only moves forward.
            frame: Frame(self.at.get() - self.segment.origin_frame.get()),
        }
    }

    /// Jump to `beat`: a new segment from this frame.
    pub(crate) fn seat(&mut self, beat: Beat) {
        self.restart(beat);
    }

    /// Move at `tempo` from this frame on. A new segment only when it
    /// differs, so an unchanged tempo leaves the arithmetic untouched.
    pub(crate) fn set_tempo(&mut self, tempo: Bpm) {
        if tempo != self.segment.tempo {
            self.restart(self.beat());
            self.segment.tempo = tempo;
        }
    }

    /// Count frames at `sample_rate` from this frame on, the beat held.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        if sample_rate != self.segment.sample_rate {
            self.restart(self.beat());
            self.segment.sample_rate = sample_rate;
        }
    }

    /// Start a new segment at `beat` on the current frame.
    fn restart(&mut self, beat: Beat) {
        self.segment.origin_frame = self.at;
        self.segment.origin_beat = beat;
    }

    /// Roll `frames` forward, wrapping at `looping`.
    ///
    /// A wrap happens on the first frame whose beat reaches the loop's end,
    /// and only for a playhead that was before it: a loop armed behind the
    /// playhead does not jump ([`LoopRange::advance`]). On that frame the
    /// clock starts a new segment at the wrapped beat, so the next pass is
    /// as exact as the first. Rolling `n` frames at once lands where `n`
    /// single frames land, bit for bit: both find the same wrap frames and
    /// compute the same closed form.
    pub(crate) fn advance(&mut self, frames: usize, looping: Option<LoopRange>) {
        let target = self.at + Samples(frames);
        if let Some(region) = looping {
            if self.segment.frames_per_beat().is_some() {
                while self.beat() < region.end() {
                    let Some(wrap) = self.wrap_frame(region.end(), target) else {
                        break;
                    };
                    let reached = self.segment.beat_at(wrap);
                    self.at = wrap;
                    self.restart(region.wrap(reached));
                }
            }
        }
        self.at = target;
    }

    /// The first frame after this one, and no later than `limit`, whose beat
    /// is at or past `end`; `None` when there is none. The beat is below
    /// `end` here, and non-decreasing in the frame (every step of the closed
    /// form is a correctly rounded, monotone operation), so the answer is
    /// the unique frame `f` with `beat_at(f - 1) < end <= beat_at(f)`. The
    /// estimate is within a frame or two of it; the walk makes it exact.
    fn wrap_frame(&self, end: Beat, limit: Frame) -> Option<Frame> {
        let next = self.at + Samples(1);
        if next > limit {
            return None;
        }
        // The common case, one frame at a time: not there yet.
        if self.segment.beat_at(next) < end {
            if next == limit {
                return None;
            }
        } else {
            return Some(next);
        }
        let fpb = self.segment.frames_per_beat()?;
        let ahead = ((end - self.beat()).get() * fpb).ceil();
        // Saturating: a far end is past `limit` anyway.
        let mut f = Frame(self.at.get().saturating_add(ahead.max(1.0) as u64)).max(next);
        while f > next && self.segment.beat_at(Frame(f.get() - 1)) >= end {
            f = Frame(f.get() - 1);
        }
        while f <= limit && self.segment.beat_at(f) < end {
            f += Samples(1);
        }
        (f <= limit).then_some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: SampleRate = SampleRate(48_000.0);

    /// The reviewer's figures: at 90 BPM / 48 kHz, 1 500 chunks of 64 frames
    /// is beat 3 and 500 is beat 1, exactly, where an accumulated playhead
    /// reads 2.999999999999891 and 1.0000000000000007.
    ///
    /// Mutation (run): accumulate in `advance` (`origin_beat += frames *
    /// beats_per_sample` and `restart` each call) → 2.999999999999891 →
    /// fails.
    #[test]
    fn chunked_frames_land_on_the_beat_exactly() {
        let mut c = FrameClock::new(Beat(0.0), Bpm(90.0), SR);
        for _ in 0..500 {
            c.advance(64, None);
        }
        assert_eq!(c.beat(), Beat(1.0));
        for _ in 500..1_500 {
            c.advance(64, None);
        }
        assert_eq!(c.beat(), Beat(3.0));
    }

    /// Frame by frame and a block at a time land on the same bits, through
    /// loop wraps (including a loop shorter than a block), a tempo change
    /// and a seek.
    ///
    /// Mutation (run): wrap once per call at the call's end (the old
    /// offline rule, `region.advance(from, beat_at(target))`, with no new
    /// segment on the wrap's frame) → the block-stepped clock parts from the
    /// frame-stepped one at the first wrap → fails.
    #[test]
    fn a_frame_at_a_time_is_a_block_at_a_time() {
        let loops = [
            None,
            LoopRange::new(0.25, 1.0),
            LoopRange::new(0.5, 0.5007),
            LoopRange::new(1.0 / 3.0, 2.0 / 3.0),
        ];
        for tempo in [90.0, 97.0, 120.0, 133.3] {
            for looping in loops {
                let mut one = FrameClock::new(Beat(0.1), Bpm(tempo), SR);
                let mut block = one;
                for (i, n) in [64usize, 300, 1, 1024, 77, 512]
                    .iter()
                    .cycle()
                    .take(60)
                    .enumerate()
                {
                    if i == 20 {
                        one.set_tempo(Bpm(tempo * 1.5));
                        block.set_tempo(Bpm(tempo * 1.5));
                    }
                    if i == 40 {
                        one.seat(Beat(0.3));
                        block.seat(Beat(0.3));
                    }
                    for _ in 0..*n {
                        one.advance(1, looping);
                    }
                    block.advance(*n, looping);
                    assert_eq!(
                        one.beat().get().to_bits(),
                        block.beat().get().to_bits(),
                        "tempo {tempo}, loop {looping:?}, block {i}"
                    );
                    assert_eq!(one, block, "tempo {tempo}, loop {looping:?}, block {i}");
                }
            }
        }
    }

    /// A loop wraps on the first frame that reaches its end, to the start
    /// exactly when the end is on a frame, and a loop armed behind the
    /// playhead does not wrap.
    ///
    /// Mutation (run): wrap regardless of where the playhead is (drop the
    /// `beat() < end` guard) → the loop armed behind jumps → fails.
    #[test]
    fn a_loop_wraps_on_its_frame() {
        // 4 beats at 90 BPM is 128 000 frames.
        let four = LoopRange::new(0.0, 4.0);
        let mut c = FrameClock::new(Beat(0.0), Bpm(90.0), SR);
        c.advance(127_999, four);
        assert!(c.beat() < Beat(4.0));
        c.advance(1, four);
        assert_eq!(c.beat(), Beat(0.0), "on the loop's end frame: the start");
        c.advance(96_000, four);
        assert_eq!(c.beat(), Beat(3.0), "the second pass is as exact");

        let behind = LoopRange::new(1.0, 2.0);
        let mut c = FrameClock::new(Beat(3.0), Bpm(90.0), SR);
        c.advance(32_000, behind);
        assert_eq!(c.beat(), Beat(4.0), "armed behind: plays on");
    }

    /// After N blocks at any tempo and rate, the beat equals the closed form
    /// `origin + frames × tempo / (60 × rate)` to the bit: correctly rounded
    /// `*`, `/` and `+`, no libm, so the same on every target.
    ///
    /// Mutation (run): accumulate (see the first test) → fails at the first
    /// tempo whose step is not representable.
    #[test]
    fn a_long_run_is_the_closed_form() {
        // A small deterministic generator: no dependency, same on every run.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..50 {
            let tempo = 40.0 + (next() % 20_000) as f64 / 100.0;
            let rate = [22_050.0, 44_100.0, 48_000.0, 88_200.0, 96_000.0, 192_000.0]
                [(next() % 6) as usize];
            let start = (next() % 1_000) as f64 / 7.0;
            let mut c = FrameClock::new(Beat(start), Bpm(tempo), SampleRate(rate));
            let mut frames = 0u64;
            for _ in 0..2_000 {
                let n = 1 + (next() % 1_024) as usize;
                c.advance(n, None);
                frames += n as u64;
            }
            let closed = start + (frames as f64 * tempo) / (60.0 * rate);
            assert_eq!(
                c.beat().get().to_bits(),
                closed.to_bits(),
                "{tempo} BPM at {rate} Hz from {start}, {frames} frames"
            );
        }
    }

    /// The wrap frame is the first frame whose beat reaches the end, exactly:
    /// the frame before it is short of the end, whatever rounding the
    /// estimate `ceil((end - beat) × frames_per_beat)` suffers.
    ///
    /// Mutation (run): trust the estimate (drop the two walks in
    /// `wrap_frame`) → some end lands a frame off → fails.
    #[test]
    fn the_wrap_frame_is_the_first_to_reach_the_end() {
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200_000 {
            let tempo = 40.0 + (next() % 20_000) as f64 / 100.0;
            let rate = [44_100.0, 48_000.0, 96_000.0][(next() % 3) as usize];
            let origin = (next() % 4_096) as f64 / 64.0;
            let mut c = FrameClock::new(Beat(origin), Bpm(tempo), SampleRate(rate));
            c.advance((next() % 10_000) as usize, None);
            // An end that is itself a frame's beat on this segment, nudged by
            // up to an ulp: the case the estimate's rounding gets wrong.
            let k = 1 + next() % 100_000;
            let on = c.segment.beat_at(c.at + Samples(k as usize)).get();
            let end = Beat(f64::from_bits(
                (on.to_bits() as i64 + (next() % 3) as i64 - 1) as u64,
            ));
            if end <= c.beat() {
                continue;
            }
            let f = c
                .wrap_frame(end, Frame(u64::MAX))
                .expect("a rolling clock reaches it");
            assert!(c.segment.beat_at(f) >= end, "{tempo} {rate}: {f} short");
            assert!(
                f == c.at + Samples(1) || c.segment.beat_at(Frame(f.get() - 1)) < end,
                "{tempo} BPM at {rate} Hz: frame {f} is not the first to reach {end:?}"
            );
        }
    }

    /// A snapshot's origin rebuilds the clock it came from.
    ///
    /// Mutation (run): report the clock's own frame count as the origin's
    /// (`frame: self.at`, not the frames since the origin) → the rebuilt
    /// clock counts the frames before the tempo change twice → fails.
    #[test]
    fn a_snapshot_continues_the_clock() {
        let mut c = FrameClock::new(Beat(0.0), Bpm(97.0), SR);
        c.advance(10_000, None);
        c.set_tempo(Bpm(133.0));
        c.advance(5_555, None);
        let t = tutti_graph::Transport {
            playing: true,
            tempo: c.tempo(),
            beat: c.beat(),
            looping: None,
            origin: Some(c.origin()),
        };
        let mut again = FrameClock::of(&t, SR);
        assert_eq!(again.beat(), c.beat());
        again.advance(777, None);
        c.advance(777, None);
        assert_eq!(again.beat().get().to_bits(), c.beat().get().to_bits());
    }
}
