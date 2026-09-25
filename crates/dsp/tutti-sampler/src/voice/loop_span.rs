//! A loop, as the sequence of file frames a looped voice plays.
//!
//! Shared by every reader that indexes a file directly — `MemorySource` (its
//! free-running loop and a placed voice's) and the offline disk reader a forked
//! disk voice plays through — so the same loop sounds the same on each. Pure
//! arithmetic: no allocation, safe on the audio thread.
//!
//! # The loop is a sequence of frames, and the interpolator reads that sequence
//!
//! A looped voice plays file frames `0, 1, …, end - 1`, then `start, …, end -
//! 1` again, forever. Two things follow, and both used to be wrong:
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
//!   the one the loop just played. This is the sequence the butler's ring holds
//!   (its refill wraps at `end`), so the ring's own four-tap history reads the
//!   same frames.
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
//! # A fade longer than the material before `start` is clamped
//!
//! The lead-in is the `fade` frames before `start`, so a loop starting at frame
//! 100 can fade over at most 100 frames, and one starting at frame 0 loops hard.
//! A fade is also never longer than the loop.

use super::LoopSetting;
use tutti_core::SamplePosition;

/// A loop over whole file frames `[start, end)`, with a crossfade of `fade`
/// frames into it. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoopSpan {
    start: usize,
    end: usize,
    /// Clamped to `start` (the lead-in must exist) and to the loop's length.
    fade: usize,
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
    /// The loop `[start, end)` in whole file frames, fading over
    /// `crossfade_frames` (clamped; see the module docs). `None` for an
    /// empty or inverted range, which does not loop.
    pub(crate) fn new(start: usize, end: usize, crossfade_frames: usize) -> Option<Self> {
        (end > start).then(|| Self {
            start,
            end,
            fade: crossfade_frames.min(start).min(end - start),
        })
    }

    /// The loop a [`LoopSetting`] describes, its points taken to whole frames
    /// as the butler takes them (`Commands::send`'s `Command::Loop`).
    pub(crate) fn from_setting(setting: LoopSetting) -> Option<Self> {
        match setting {
            LoopSetting::On {
                start,
                end,
                crossfade_frames,
            } => Self::new(whole_frame(start), whole_frame(end), crossfade_frames),
            LoopSetting::Off => None,
        }
    }

    /// The loop's first frame.
    pub(crate) fn start(&self) -> usize {
        self.start
    }

    /// One past the loop's last frame.
    pub(crate) fn end(&self) -> usize {
        self.end
    }

    /// The crossfade length in use, after the clamp.
    pub(crate) fn fade(&self) -> usize {
        self.fade
    }

    /// Place a forward read at `pos` (file frames, counted as if the file
    /// played straight on) on the loop: the position inside `[start, end)` it
    /// plays, once it has reached `end`, and whether it has been round.
    ///
    /// Modulo, so an overshoot longer than the loop still lands inside it.
    pub(crate) fn place(&self, pos: f64) -> (f64, bool) {
        let (start, end) = (self.start as f64, self.end as f64);
        if pos < end {
            return (pos, false);
        }
        (start + (pos - start).rem_euclid(end - start), true)
    }

    /// The crossfade at file frame `frame`: the lead-in frame it blends toward
    /// and the lead-in's weight, or `None` outside the fade.
    #[inline]
    pub(crate) fn fade_at(&self, frame: usize) -> Option<(usize, f32)> {
        let fade_start = self.end - self.fade;
        if frame < fade_start || frame >= self.end {
            return None;
        }
        let k = frame - fade_start;
        Some((
            self.start - self.fade + k,
            (k + 1) as f32 / (self.fade + 1) as f32,
        ))
    }

    /// The four frames of the looped sequence the kernel interpolates `pos`
    /// from, and the fraction between the second and third, in a file `len`
    /// frames long (non-zero).
    ///
    /// `pos` is placed on the loop already (below `end`; see
    /// [`place`](Self::place)), and `looped` says whether the read has been
    /// round it: then the frame behind `start` is `end - 1`, the one the loop
    /// just played. Ahead, a tap at or past `end` wraps through `start`. Each
    /// tap is clamped to the file, as [`tap_indices`](super::interp::tap_indices)
    /// clamps an unlooped read.
    #[inline]
    pub(crate) fn taps(&self, len: usize, pos: f64, looped: bool) -> ([LoopTap; 4], f32) {
        let (idx, frac) = super::interp::split_position(pos);
        let loop_len = self.end - self.start;
        let last = len - 1;
        let tap = |offset: isize| {
            let mut at = idx as isize + offset;
            if looped && at < self.start as isize {
                at += loop_len as isize;
            }
            let mut at = at.max(0) as usize;
            if at >= self.end {
                at = self.start + (at - self.start) % loop_len;
            }
            let frame = at.min(last);
            LoopTap {
                frame,
                // A lead-in is before `start`, so inside the file whenever the
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

    /// **The fade leads into `start`.** A 4-frame fade on `[10, 20)` blends
    /// frames 16..20 toward 6..10, weighted 1/5 … 4/5, and nothing else.
    ///
    /// Mutation (run): the lead-in `start + k` (the old head replay) → frame 16
    /// blends toward 10 → fails.
    #[test]
    fn the_fade_blends_the_tail_toward_what_leads_into_start() {
        let span = LoopSpan::new(10, 20, 4).expect("a loop");
        assert_eq!(span.fade_at(15), None);
        assert_eq!(span.fade_at(16), Some((6, 0.2)));
        assert_eq!(span.fade_at(19), Some((9, 0.8)));
        assert_eq!(span.fade_at(20), None);
    }

    /// **A fade is clamped to the material before `start` and to the loop.**
    ///
    /// Mutation (run): the `min(start)` clamp removed → a 100-frame fade on a
    /// loop at frame 10, whose lead-in would start before the file → fails.
    #[test]
    fn a_fade_is_clamped_to_its_lead_in_and_its_loop() {
        assert_eq!(LoopSpan::new(10, 1_000, 100).map(|s| s.fade()), Some(10));
        assert_eq!(LoopSpan::new(0, 1_000, 100).map(|s| s.fade()), Some(0));
        assert_eq!(LoopSpan::new(500, 520, 100).map(|s| s.fade()), Some(20));
        assert_eq!(LoopSpan::new(20, 20, 4), None);
        let span = LoopSpan::new(10, 1_000, 100).expect("a loop");
        assert_eq!(span.fade_at(990), Some((0, 1.0 / 11.0)));
    }

    /// **Taps wrap through the loop**: ahead of a position near `end` they read
    /// `start`, `start + 1`; behind one on `start`, after a wrap, `end - 1`.
    ///
    /// Mutation (run): the forward wrap removed (taps clamped to the file) →
    /// `[18, 19, 20, 21]` → fails. Mutation (run): the `looped` back-tap
    /// removed → `[9, 10, 11, 12]` after a wrap → fails.
    #[test]
    fn taps_wrap_through_the_loop() {
        let span = LoopSpan::new(10, 20, 0).expect("a loop");
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
