//! [`TimelineSegment`]: the one conversion between a frame and a beat, and
//! [`first_frame_at_or_after`], the one rule for which frame a beat lands on.
//!
//! # The frame is the source of truth
//!
//! A playhead that *accumulates* its beat — `beat += beats_per_sample`, per
//! frame or per block — drifts: at 90 BPM and 48 kHz one frame is 1/32 000
//! of a beat, which binary cannot represent, and after 1 500 chunks of 64
//! frames the sum reads `2.999999999999891` where frame 96 000 is exactly
//! beat 3. A reader deciding "has the playhead reached beat 3?" then answers
//! a chunk late, or a frame early, depending on which way the error went.
//!
//! So a clock keeps an **integer frame count** and derives the beat from it
//! in closed form: a segment's origin beat plus `frames × tempo / (60 × rate)`.
//! Each step is one correctly rounded IEEE operation on exact integers (a
//! frame count below 2⁵³, a tempo and rate that are whole numbers in
//! practice), so the beat is exact whenever the true value is representable:
//! `96 000 × 90 / (60 × 48 000)` is `8 640 000 / 2 880 000`, which is `3.0`.
//! And it is portable: no libm, only `*`, `/` and `+`, which IEEE 754 fixes
//! to the bit on every target.
//!
//! A segment ends where the arithmetic would stop being one straight line: a
//! tempo change, a seek, a loop wrap, a rate change. The clock starts a new
//! segment there, at an integer frame, so nothing accumulates across it
//! either.
//!
//! # One rule for "which frame"
//!
//! The other direction — a beat to the frame playback reaches it on — is the
//! **first frame at or after the beat, within a millionth of a frame**
//! ([`FRAME_TOLERANCE`]): a beat that falls exactly on a frame, but whose
//! `f64` came out an ulp late, still lands on that frame rather than the next.
//! Every reader that decides whether a beat is inside a block applies this
//! one rule to a frame distance ([`first_frame_at_or_after`]) and compares
//! integers, instead of comparing beats with `<` or flooring an offset with
//! `as u32`. Blocks that tile the frames then place every beat in exactly one
//! of them: a beat between a block's last frame and its end rounds up to the
//! next block's first frame (offset 0 there, `len` here), never to both or
//! neither.

use super::frame::Frame;
use super::units::{Beat, BeatDuration, Bpm, SampleRate};

/// How far past a frame a beat may fall, in frames, and still land on it.
///
/// A millionth of a frame is far below anything musical (20 ns at 48 kHz) and
/// far above the rounding of an `f64` beat (an ulp of beat 10⁶ is ~10⁻¹⁰
/// beat, ~4·10⁻⁶ of a frame only past beat 10⁶ at 90 BPM). The native graph's
/// beat-timed commands (`tutti_graph::Env::due`) have used this tolerance
/// since they landed; it moved here so every reader uses the same one.
pub const FRAME_TOLERANCE: f64 = 1e-6;

/// The frame on which playback reaches a point `frames_ahead` frames after
/// some origin frame: the first frame at or after it, within
/// [`FRAME_TOLERANCE`]. Negative when that frame is before the origin.
///
/// **The** beat→frame rule: every "is this beat in this block, and where"
/// decision goes through it, so two readers cannot disagree about which
/// block a beat falls in. A point exactly on a frame is that frame; a point
/// a hair past one (up to the tolerance) is that frame too; anything later
/// is the next frame. A block `[0, len)` contains the point iff the result is
/// in `0..len`.
///
/// `frames_ahead` is a distance already converted to frames — a
/// [`TimelineSegment`] does that ([`TimelineSegment::frame_of`]); a reader
/// holding only a beat span and a per-frame beat span divides them. A NaN
/// distance is frame 0 (Rust's saturating cast), so guard the tempo first.
#[inline]
pub fn first_frame_at_or_after(frames_ahead: f64) -> i64 {
    (frames_ahead - FRAME_TOLERANCE).ceil() as i64
}

/// A stretch of the timeline at one tempo: the beat at its first frame, and
/// the tempo and rate it rolls at. The frame→beat conversion
/// ([`beat_at`](Self::beat_at)) and its inverse ([`frame_of`](Self::frame_of)),
/// in one place. See the [module docs](self).
///
/// Frames are counted on whatever clock the owner keeps (an engine's clock
/// counts frames rolled since it started); `origin_frame` is where this
/// segment begins on it. A reader with no clock of its own (a block reading a
/// timeline) uses the block's first frame as [`Frame::ZERO`].
///
/// ```
/// use tutti_types::{Beat, Bpm, Frame, SampleRate, TimelineSegment};
/// let seg = TimelineSegment::new(Frame::ZERO, Beat(0.0), Bpm(90.0), SampleRate(48_000.0));
/// // Exact, not 2.999999999999891: no accumulation.
/// assert_eq!(seg.beat_at(Frame(96_000)), Beat(3.0));
/// assert_eq!(seg.frame_of(Beat(3.0)), Some(Frame(96_000)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineSegment {
    /// The segment's first frame, on its owner's clock.
    pub origin_frame: Frame,
    /// The beat at [`origin_frame`](Self::origin_frame).
    pub origin_beat: Beat,
    /// The tempo it rolls at.
    pub tempo: Bpm,
    /// The rate its frames are counted at.
    pub sample_rate: SampleRate,
}

impl TimelineSegment {
    /// A segment at `origin_beat` from `origin_frame`, at `tempo` and
    /// `sample_rate`.
    #[inline]
    pub const fn new(
        origin_frame: Frame,
        origin_beat: Beat,
        tempo: Bpm,
        sample_rate: SampleRate,
    ) -> Self {
        Self {
            origin_frame,
            origin_beat,
            tempo,
            sample_rate,
        }
    }

    /// Frames per beat, when the tempo and rate are both finite and positive
    /// (`None` otherwise: the segment does not move).
    ///
    /// Raw `f64`, on purpose: frames an hour into a session do not fit
    /// `Seconds` (`f32`), CLAUDE.md's "where the types stop".
    #[inline]
    pub fn frames_per_beat(&self) -> Option<f64> {
        let (tempo, rate) = (self.tempo.get(), self.sample_rate.get());
        // `is_finite` and `> 0.0` together also refuse a NaN.
        let usable = tempo.is_finite() && tempo > 0.0 && rate.is_finite() && rate > 0.0;
        usable.then(|| rate * 60.0 / tempo)
    }

    /// The beat at `frame`, in closed form: `origin_beat + n × tempo /
    /// (60 × rate)` for the `n` frames from the origin (negative before it).
    ///
    /// Never accumulated, so exact whenever the true beat is representable,
    /// and bit-identical for the same segment and frame however the caller
    /// got there (frame by frame or a block at a time).
    #[inline]
    pub fn beat_at(&self, frame: Frame) -> Beat {
        let n = match frame.since(self.origin_frame) {
            Some(d) => d.get() as f64,
            // Before the origin (or a distance past `usize`): signed, in
            // `f64`, which is exact below 2^53 frames.
            None => frame.get() as f64 - self.origin_frame.get() as f64,
        };
        self.origin_beat + BeatDuration((n * self.tempo.get()) / (60.0 * self.sample_rate.get()))
    }

    /// The frame on which playback along this segment reaches `beat`: the
    /// first frame at or after it ([`first_frame_at_or_after`]).
    ///
    /// `None` when the segment does not move ([`frames_per_beat`] is `None`),
    /// or when that frame would be before [`Frame::ZERO`] on the owner's
    /// clock. A frame before [`origin_frame`](Self::origin_frame) is
    /// returned as it is: the segment extrapolates backwards, and whether a
    /// point behind the playhead counts is the caller's rule.
    ///
    /// [`frames_per_beat`]: Self::frames_per_beat
    #[inline]
    pub fn frame_of(&self, beat: Beat) -> Option<Frame> {
        let k = self.offset_of(beat)?;
        let at = i128::from(self.origin_frame.get()) + i128::from(k);
        u64::try_from(at).ok().map(Frame)
    }

    /// Whether playback along this segment has reached `beat` by `frame`:
    /// the beat's frame ([`frame_of`](Self::frame_of)) is at or before it.
    /// `false` when the segment does not move.
    #[inline]
    pub fn reached_by(&self, frame: Frame, beat: Beat) -> bool {
        let Some(k) = self.offset_of(beat) else {
            return false;
        };
        i128::from(self.origin_frame.get()) + i128::from(k) <= i128::from(frame.get())
    }

    /// Frames from the origin to `beat`'s frame, signed.
    #[inline]
    fn offset_of(&self, beat: Beat) -> Option<i64> {
        let fpb = self.frames_per_beat()?;
        Some(first_frame_at_or_after(
            (beat - self.origin_beat).get() * fpb,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: SampleRate = SampleRate(48_000.0);

    /// The reviewer's case: at 90 BPM / 48 kHz, frame 96 000 is beat 3 and
    /// frame 32 000 is beat 1, to the bit, and they convert back.
    ///
    /// Mutation (run): `beat_at` as `origin + n * (tempo / 60 / rate)` (a
    /// rounded per-frame step, then scaled) → at 140 BPM frame 144 000 is
    /// not beat 7 → fails.
    #[test]
    fn a_frame_on_a_beat_is_that_beat_exactly() {
        let seg = TimelineSegment::new(Frame::ZERO, Beat(0.0), Bpm(90.0), SR);
        assert_eq!(seg.beat_at(Frame(96_000)), Beat(3.0));
        // Every whole beat, where the step `tempo / 60 / rate` rounded and
        // scaled misses some.
        let fast = TimelineSegment::new(Frame::ZERO, Beat(0.0), Bpm(140.0), SR);
        for k in 1..=2_000u64 {
            let frame = k * 60 * 48_000;
            if frame % 140 == 0 {
                assert_eq!(fast.beat_at(Frame(frame / 140)), Beat(k as f64), "beat {k}");
            }
        }
        assert_eq!(seg.beat_at(Frame(32_000)), Beat(1.0));
        assert_eq!(seg.frame_of(Beat(3.0)), Some(Frame(96_000)));
        assert_eq!(seg.frame_of(Beat(1.0)), Some(Frame(32_000)));
        // From a later origin, the same frames by their distance.
        let later = TimelineSegment::new(Frame(1_000), Beat(0.5), Bpm(90.0), SR);
        assert_eq!(later.beat_at(Frame(1_000 + 80_000)), Beat(3.0));
    }

    /// A beat a hair past a frame lands on it; one more than the tolerance
    /// past lands on the next; a hair before lands on it too.
    ///
    /// Mutation (run): drop the tolerance (`frames_ahead.ceil()`) → the beat an
    /// ulp past frame 32 000 lands on 32 001 → fails.
    #[test]
    fn a_beat_lands_on_the_first_frame_at_or_after_it() {
        let seg = TimelineSegment::new(Frame::ZERO, Beat(0.0), Bpm(90.0), SR);
        let ulp_past = Beat(f64::from_bits(1.0f64.to_bits() + 1));
        let ulp_before = Beat(f64::from_bits(1.0f64.to_bits() - 1));
        assert_eq!(seg.frame_of(ulp_past), Some(Frame(32_000)));
        assert_eq!(seg.frame_of(ulp_before), Some(Frame(32_000)));
        // A tenth of a frame past is the next frame.
        let tenth = Beat(1.0 + 0.1 / 32_000.0);
        assert_eq!(seg.frame_of(tenth), Some(Frame(32_001)));
        assert_eq!(first_frame_at_or_after(0.0), 0);
        assert_eq!(first_frame_at_or_after(1e-7), 0);
        assert_eq!(first_frame_at_or_after(2e-6), 1);
        assert_eq!(first_frame_at_or_after(-0.5), 0);
        assert_eq!(first_frame_at_or_after(-1.0), -1);
    }

    /// Tiled blocks place every beat in exactly one of them, including beats
    /// between a block's last frame and its end.
    ///
    /// Mutation (run): `ceil` → `floor` in `first_frame_at_or_after` → a beat at
    /// 63.5 frames lands in both the first block (63) and, from the second
    /// block's origin, at -1 (before it) → the count is wrong → fails.
    #[test]
    fn tiled_blocks_place_every_beat_once() {
        let fpb = 32_000.0;
        for &frames in &[0.0, 0.3, 63.0, 63.5, 63.9999999, 64.0, 64.0000001, 100.25] {
            let beat = Beat(frames / fpb);
            let mut hits = 0;
            for block in 0..4u64 {
                let start = block * 64;
                let seg = TimelineSegment::new(
                    Frame::ZERO,
                    TimelineSegment::new(Frame::ZERO, Beat(0.0), Bpm(90.0), SR)
                        .beat_at(Frame(start)),
                    Bpm(90.0),
                    SR,
                );
                if let Some(f) = seg.frame_of(beat) {
                    if f.get() < 64 {
                        hits += 1;
                    }
                }
                // `frame_of` from a later block's origin is `None` (before
                // its frame zero) for an earlier beat: never counted twice.
            }
            assert_eq!(hits, 1, "beat at {frames} frames");
        }
    }

    /// Mutation (run): accept any tempo in `frames_per_beat` → a zero tempo
    /// is infinitely many frames a beat, and beat 2 "lands" on a frame →
    /// fails.
    #[test]
    fn a_segment_that_does_not_move_places_nothing() {
        let seg = TimelineSegment::new(Frame::ZERO, Beat(1.0), Bpm(0.0), SR);
        assert_eq!(seg.frame_of(Beat(2.0)), None);
        assert!(!seg.reached_by(Frame(10), Beat(0.5)));
        assert_eq!(seg.beat_at(Frame(1_000)), Beat(1.0));
    }

    /// Mutation (run): drop the tolerance in `first_frame_at_or_after` → the
    /// start an ulp past the origin's beat is not reached there → fails.
    #[test]
    fn reached_by_is_the_frame_rule() {
        let seg = TimelineSegment::new(Frame(100), Beat(3.0), Bpm(90.0), SR);
        // Behind the origin: reached.
        assert!(seg.reached_by(Frame(100), Beat(2.5)));
        // On it, and an ulp past it: reached.
        assert!(seg.reached_by(Frame(100), Beat(3.0)));
        assert!(seg.reached_by(Frame(100), Beat(f64::from_bits(3.0f64.to_bits() + 1))));
        // A frame on: not yet at the origin, reached a frame later.
        let next = Beat(3.0 + 1.0 / 32_000.0);
        assert!(!seg.reached_by(Frame(100), next));
        assert!(seg.reached_by(Frame(101), next));
    }
}
