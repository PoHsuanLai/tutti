use tutti_dsp::{SpeakerLayout, VbapGains, VbapKernel};
use tutti_types::{Direction, SampleRate, Samples, Spread, StereoWidth};

use crate::spatial::AngleSmoother;

/// VBAP panner internals. Use `VbapPannerNode` instead.
///
/// Holds no control state: the node reads its cells once per block and
/// passes the target, spread and width in, already normalized (bearing
/// wrapped, height and spread clamped).
#[derive(Clone, Debug)]
pub(crate) struct VbapPanner {
    kernel: VbapKernel,
    smoother: AngleSmoother,
    /// The target bits and block length the smoother has reached a fixed
    /// point for: advancing it that many frames toward this target returns
    /// its own state unchanged, bit for bit, so the advance is skipped.
    /// Cleared by anything that moves the state or the coefficient (a
    /// reseed, a new sample rate).
    settled_at: Option<([u32; 2], usize)>,
}

impl VbapPanner {
    pub(crate) fn new(layout: SpeakerLayout) -> Self {
        let sample_rate = SampleRate(48000.0);
        Self {
            kernel: VbapKernel::new(layout),
            smoother: AngleSmoother::new(sample_rate, Samples(1)),
            settled_at: None,
        }
    }

    /// Retune the position smoothers so the 50ms de-zipper ramp holds at any
    /// sample rate (the smoothers are built at 48kHz in `new`).
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.smoother.set_sample_rate(sample_rate);
        // A new coefficient can move a state the old one held still.
        self.settled_at = None;
    }

    /// Drop the in-flight de-zipper ramp, seating it on the commanded
    /// direction.
    ///
    /// The smoother is this panner's only per-block history; the commanded
    /// position is the caller's, which is why it is passed in.
    pub(crate) fn reset_state(&mut self, target: Direction) {
        self.smoother.reset_to_target(target);
        self.settled_at = None;
    }

    /// The gains at the smoother's current position, without advancing
    /// it — the starting point of the first ramp after a construction or a
    /// reset.
    pub(crate) fn gains_now(&mut self, width: StereoWidth, spread: Spread) -> VbapGains {
        let aim = self.smoother.current();
        self.kernel.gains(aim.azimuth, aim.elevation, width, spread)
    }

    /// Advance the de-zipper `frames` steps toward the target and solve the
    /// gains once, at the block's last frame.
    ///
    /// The smoother advances `frames` steps in one analytic move
    /// (`AngleSmoother::advance`),
    /// so the 50 ms ramp keeps its timing. The frames in between are not
    /// solved: the node ramps linearly towards this set instead, which saves
    /// two VBAP solves per frame.
    ///
    /// An advance is a pure function of the smoother's state, the target and
    /// the block length, so once one returns the state unchanged the same
    /// advance would again: a held source skips it.
    pub(crate) fn solve_block(
        &mut self,
        target: Direction,
        width: StereoWidth,
        spread: Spread,
        frames: usize,
    ) -> VbapGains {
        let settled = (
            [
                target.azimuth.get().to_bits(),
                target.elevation.get().to_bits(),
            ],
            frames,
        );
        if self.settled_at != Some(settled) {
            let before = self.smoother.current();
            let after = self.smoother.advance(target, frames);
            self.settled_at = same_bits(after, before).then_some(settled);
        }
        let aim = self.smoother.current();
        self.kernel.gains(aim.azimuth, aim.elevation, width, spread)
    }
}

/// Whether two positions are the same bits (not merely `==`: `-0.0` and
/// `0.0` are different smoother states).
#[inline]
fn same_bits(a: Direction, b: Direction) -> bool {
    a.azimuth.get().to_bits() == b.azimuth.get().to_bits()
        && a.elevation.get().to_bits() == b.elevation.get().to_bits()
}

#[cfg(test)]
mod tests;
