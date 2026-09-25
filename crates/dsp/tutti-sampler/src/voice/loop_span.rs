//! A loop, as the sequence of file frames a looped voice plays.
//!
//! Shared by every reader that indexes a file directly — `MemorySource` (its
//! free-running loop and a placed voice's) and the offline disk reader a forked
//! disk voice plays through — and by the butler, which writes a live stream's
//! ring by it (`butler::loops::fill_sequence`), so the same loop sounds the
//! same on each. Pure arithmetic: no allocation, safe on the audio thread.
//!
//! # The loop is a sequence of frames, and the interpolator reads that sequence
//!
//! A looped voice plays file frames `0, 1, …, end - 1`, then `start, …, end -
//! 1` again, forever (`resume` rather than `start` when the fade has to go into
//! the loop's head, below). Two things follow, and both used to be wrong:
//!
//! - **The crossfade leads into `start`, it does not replay it.** Frame `end -
//!   fade + k` (for `k` in `0..fade`) is blended toward frame `start - fade +
//!   k`: the material that leads into `start`. The last blended frame is almost
//!   all `start - 1`, and the next frame played is `start`, so the join is the
//!   file's own step from `start - 1` to `start`. The first cut blended toward
//!   `start + k` and then played `start` again: the loop's head was heard twice,
//!   a jump of `fade` frames at every wrap.
//! - **Taps near the end read through the wrap.** The cubic kernel reads two
//!   frames ahead of a position; a position within two frames of `end` reads
//!   `start`, `start + 1` there, which are the frames the loop plays next, not
//!   `end`, `end + 1` from the file past the loop. Behind a position on the
//!   loop's start, once the voice has been round, the frame before is `end - 1`,
//!   the one the loop just played. This is the sequence the butler writes into
//!   a looped stream's ring (fade blended in, wrapping to `resume`), so a live
//!   voice's own four-tap history reads the same frames.
//!
//! # The fade weight
//!
//! Frame `k` of an `n`-frame fade weighs the lead-in by `(k + 1) / (n + 1)`, a
//! linear fade that excludes both endpoints: the frame before the fade is pure
//! tail, the frame after it is pure `start`, and neither join steps by more than
//! `1 / (n + 1)` of the gap between tail and lead-in on top of the material's
//! own step. Linear, not equal-power, as every crossfade in this crate is: on
//! correlated material (a sustained tone either side of the seam) a linear fade
//! holds the level exactly.
//!
//! # When there is too little before `start`: fade into the head instead
//!
//! The lead-in is the `fade` frames before `start`, so a loop starting at frame
//! 0 (or at any frame closer to the file's start than the fade asked for) has
//! none. Then the tail fades into the loop's own head, `[start, start + fade)`,
//! and the wrap resumes at `start + fade`: frame `end - fade + k` blends toward
//! `start + k`, the last blended frame is almost all `start + fade - 1`, and the
//! loop continues at `start + fade` — still the file's own step at the join,
//! with no material before `start` needed. The loop that repeats is then
//! `[start + fade, end)`; its head is heard only on the first pass (and inside
//! every fade). The fade is at most half the loop in that mode, so the head and
//! the tail never overlap.
//!
//! Both modes are one rule with a **resume** frame: the tail blends toward the
//! `fade` frames that lead into `resume`, and the wrap lands on `resume`.
//! `resume` is `start` when the lead-in exists, `start + fade` when it does not.
//! A fade is never longer than the loop, and the loop's end is clamped to the
//! file where its length is known.

use super::LoopSetting;
use tutti_core::SamplePosition;

/// A loop over whole file frames `[start, end)`, with a crossfade of `fade`
/// frames into it, wrapping to `resume`. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoopSpan {
    start: usize,
    end: usize,
    /// The crossfade in use: the length asked for, clamped to the loop (to
    /// half of it when fading into the head).
    fade: usize,
    /// Where the wrap lands and what the fade leads into: `start`, or `start +
    /// fade` when there is too little before `start` for a lead-in.
    resume: usize,
}

/// One of the four frames the kernel interpolates a looped position from: the
/// file frame it reads, and, inside the crossfade, the lead-in frame it blends
/// toward and the lead-in's weight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LoopTap {
    pub(crate) frame: usize,
    pub(crate) fade: Option<(usize, f32)>,
}

impl LoopSpan {
    /// The loop `[start, end)` in whole file frames of a file `len` frames
    /// long (`usize::MAX` where the length is not known; `end` is clamped to
    /// it), fading over `crossfade_frames` (see the module docs for the two
    /// modes). `None` for a range with nothing in it, which does not loop.
    pub(crate) fn new(
        start: usize,
        end: usize,
        crossfade_frames: usize,
        len: usize,
    ) -> Option<Self> {
        let end = end.min(len);
        if end <= start {
            return None;
        }
        let loop_len = end - start;
        let (fade, resume) = if crossfade_frames <= start {
            // A lead-in exists: fade toward what leads into `start`.
            (crossfade_frames.min(loop_len), start)
        } else {
            // Too little before `start`: fade into the head, resume after it.
            let fade = crossfade_frames.min(loop_len / 2);
            (fade, start + fade)
        };
        Some(Self {
            start,
            end,
            fade,
            resume,
        })
    }

    /// The loop a [`LoopSetting`] describes in a file `len` frames long, its
    /// points taken to whole frames as the butler takes them
    /// (`Commands::send`'s `Command::Loop`).
    pub(crate) fn from_setting(setting: LoopSetting, len: usize) -> Option<Self> {
        match setting {
            LoopSetting::On {
                start,
                end,
                crossfade_frames,
            } => Self::new(whole_frame(start), whole_frame(end), crossfade_frames, len),
            LoopSetting::Off => None,
        }
    }

    /// One past the loop's last frame (clamped to the file).
    pub(crate) fn end(&self) -> usize {
        self.end
    }

    /// Where a wrap lands: `start`, or `start + fade` when the fade is into the
    /// loop's head.
    pub(crate) fn resume(&self) -> usize {
        self.resume
    }

    /// The crossfade length in use, after the clamp.
    pub(crate) fn fade(&self) -> usize {
        self.fade
    }

    /// Place a forward read at `pos` (file frames, counted as if the file
    /// played straight on) on the loop: the position it plays, which once it
    /// has reached `end` repeats `[resume, end)`, and whether it has been round.
    ///
    /// Modulo, so an overshoot longer than the loop still lands inside it.
    pub(crate) fn place(&self, pos: f64) -> (f64, bool) {
        let (resume, end) = (self.resume as f64, self.end as f64);
        if pos < end {
            return (pos, false);
        }
        (resume + (pos - end).rem_euclid(end - resume), true)
    }

    /// [`place`](Self::place) for a whole frame: the file frame a forward
    /// stream at `pos` (counted as if the file played straight on) plays.
    ///
    /// The butler's refill writes a looped stream's ring frame by frame
    /// through this, so the ring carries the sequence every other tier reads
    /// (`butler::loops::fill_sequence`).
    #[inline]
    pub(crate) fn place_frame(&self, pos: usize) -> usize {
        if pos < self.end {
            return pos;
        }
        self.resume + (pos - self.end) % (self.end - self.resume)
    }

    /// The crossfade at file frame `frame`: the lead-in frame it blends toward
    /// (one of the `fade` frames leading into `resume`) and the lead-in's
    /// weight, or `None` outside the fade.
    #[inline]
    pub(crate) fn fade_at(&self, frame: usize) -> Option<(usize, f32)> {
        let fade_start = self.end - self.fade;
        if frame < fade_start || frame >= self.end {
            return None;
        }
        let k = frame - fade_start;
        Some((
            self.resume - self.fade + k,
            (k + 1) as f32 / (self.fade + 1) as f32,
        ))
    }

    /// The four frames of the looped sequence the kernel interpolates `pos`
    /// from, and the fraction between the second and third, in a file `len`
    /// frames long (non-zero).
    ///
    /// `pos` is placed on the loop already (below `end`; see
    /// [`place`](Self::place)), and `looped` says whether the read has been
    /// round it. Ahead, a tap at or past `end` wraps through `resume`. Behind,
    /// a tap before `resume` is the loop's last frames — but only when the
    /// position itself is on the repeating loop (at or after `resume`) and has
    /// been round it: a position before `resume` reads the file behind it,
    /// whatever `looped` says (a loop moved under a cursor that had been round
    /// the old one leaves it there). Each tap is clamped to the file, as
    /// [`tap_indices`](super::interp::tap_indices) clamps an unlooped read.
    #[inline]
    pub(crate) fn taps(&self, len: usize, pos: f64, looped: bool) -> ([LoopTap; 4], f32) {
        let (idx, frac) = super::interp::split_position(pos);
        let cycle = self.end - self.resume;
        let last = len - 1;
        let back_wraps = looped && idx >= self.resume;
        let tap = |offset: isize| {
            let mut at = idx as isize + offset;
            if back_wraps && at < self.resume as isize {
                at += cycle as isize;
            }
            let mut at = at.max(0) as usize;
            if at >= self.end {
                at = self.resume + (at - self.end) % cycle;
            }
            let frame = at.min(last);
            LoopTap {
                frame,
                // A lead-in is before `resume`, so inside the file whenever the
                // tail is.
                fade: self.fade_at(frame),
            }
        };
        ([tap(-1), tap(0), tap(1), tap(2)], frac)
    }
}

/// Blend a tail sample `tail` toward its lead-in `lead` by the lead-in's
/// weight `t`. One expression, so every reader blends the same way, bit for bit.
#[inline]
pub(crate) fn blend(tail: f32, lead: f32, t: f32) -> f32 {
    tail * (1.0 - t) + lead * t
}

/// A loop point as a whole frame: truncated, and never below 0, as the
/// butler's `SetStreamLoop` takes it.
fn whole_frame(pos: SamplePosition) -> usize {
    pos.get().max(0.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANY: usize = usize::MAX;

    /// **The fade leads into `start`.** A 4-frame fade on `[10, 20)` blends
    /// frames 16..20 toward 6..10, weighted 1/5 … 4/5, and nothing else; the
    /// wrap lands on 10.
    ///
    /// Mutation (run): the lead-in `resume + k` (the old head replay) → frame
    /// 16 blends toward 10 → fails. Mutation (run): the weight `k / fade` →
    /// frame 16 weighs 0 → fails.
    #[test]
    fn the_fade_blends_the_tail_toward_what_leads_into_start() {
        let span = LoopSpan::new(10, 20, 4, ANY).expect("a loop");
        assert_eq!(span.fade_at(15), None);
        assert_eq!(span.fade_at(16), Some((6, 0.2)));
        assert_eq!(span.fade_at(19), Some((9, 0.8)));
        assert_eq!(span.fade_at(20), None);
        assert_eq!(span.resume(), 10);
        assert_eq!(span.place(20.0), (10.0, true));
    }

    /// **With too little before `start`, the fade goes into the loop's head
    /// and the wrap resumes after it.** A 4-frame fade on `[2, 20)` (two
    /// frames before the start) blends frames 16..20 toward the head 2..6 and
    /// wraps to 6, so the last blended frame (almost all 5) is followed by 6.
    /// A loop from frame 0 fades too; the fade is at most half the loop there.
    ///
    /// Mutation (run): the head mode removed (the fade clamped to `start`,
    /// the first cut) → a 2-frame fade → fails. Mutation (run): `resume` left
    /// at `start` in head mode → the wrap lands on 2, replaying the head →
    /// fails.
    #[test]
    fn with_too_little_before_start_the_fade_goes_into_the_head() {
        let span = LoopSpan::new(2, 20, 4, ANY).expect("a loop");
        assert_eq!((span.fade(), span.resume()), (4, 6));
        assert_eq!(span.fade_at(16), Some((2, 0.2)));
        assert_eq!(span.fade_at(19), Some((5, 0.8)));
        assert_eq!(span.place(20.0), (6.0, true));
        assert_eq!(span.place(34.0), (6.0, true), "the loop repeats [6, 20)");
        // The whole-frame twin the butler writes its ring by agrees.
        // Mutation (run): `place_frame` wrapping to `start` → 2 → fails.
        for pos in [0, 5, 19, 20, 33, 34, 47, 1_000] {
            assert_eq!(span.place_frame(pos) as f64, span.place(pos as f64).0);
        }
        let whole = LoopSpan::new(0, 1_000, 256, ANY).expect("a loop");
        assert_eq!((whole.fade(), whole.resume()), (256, 256));
        let short = LoopSpan::new(0, 100, 256, ANY).expect("a loop");
        assert_eq!((short.fade(), short.resume()), (50, 50));
    }

    /// **A fade is clamped to the loop, and the loop to the file.**
    ///
    /// Mutation (run): the `min(len)` clamp on `end` removed → a loop past a
    /// 1 000-frame file keeps its end at 5 000 → fails.
    #[test]
    fn a_fade_is_clamped_to_its_loop_and_the_loop_to_its_file() {
        assert_eq!(
            LoopSpan::new(500, 520, 100, ANY).map(|s| s.fade()),
            Some(20)
        );
        assert_eq!(LoopSpan::new(20, 20, 4, ANY), None);
        let clamped = LoopSpan::new(100, 5_000, 0, 1_000).expect("a loop");
        assert_eq!(clamped.end(), 1_000);
        assert_eq!(LoopSpan::new(2_000, 5_000, 0, 1_000), None);
    }

    /// **A position before the loop reads the file behind it, even from a
    /// cursor that has been round a loop** — the cursor a loop change leaves
    /// behind the new loop's start. Every tap of 1 500.5 is the file's own
    /// 1 499..1 502 on a loop `[2000, 4000)`, looped or not.
    ///
    /// Mutation (run): the back wrap for every tap behind `resume` whenever
    /// `looped` (the review's B1) → 3 499.. → fails.
    #[test]
    fn a_position_before_the_loop_reads_the_file_behind_it() {
        let span = LoopSpan::new(2_000, 4_000, 0, ANY).expect("a loop");
        for looped in [false, true] {
            let (taps, _) = span.taps(10_000, 1_500.5, looped);
            assert_eq!(taps.map(|t| t.frame), [1_499, 1_500, 1_501, 1_502]);
        }
    }

    /// **Taps wrap through the loop**: ahead of a position near `end` they read
    /// `start`, `start + 1`; behind one on `start`, after a wrap, `end - 1`.
    ///
    /// Mutation (run): the forward wrap removed (taps clamped to the file) →
    /// `[18, 19, 20, 21]` → fails. Mutation (run): the `looped` back-tap
    /// removed → `[9, 10, 11, 12]` after a wrap → fails.
    #[test]
    fn taps_wrap_through_the_loop() {
        let span = LoopSpan::new(10, 20, 0, ANY).expect("a loop");
        let frames = |pos, looped| {
            let (taps, _) = span.taps(100, pos, looped);
            taps.map(|t| t.frame)
        };
        assert_eq!(frames(18.5, false), [17, 18, 19, 10]);
        assert_eq!(frames(19.5, false), [18, 19, 10, 11]);
        assert_eq!(frames(10.5, true), [19, 10, 11, 12]);
        // The first pass reaches `start` from the file before it.
        assert_eq!(frames(10.5, false), [9, 10, 11, 12]);
        assert_eq!(span.place(25.0), (15.0, true));
        assert_eq!(span.place(19.0), (19.0, false));
    }
}
