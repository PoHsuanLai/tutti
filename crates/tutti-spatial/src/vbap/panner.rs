use super::error::Result;
use tutti_dsp::SpeakerLayout;
use tutti_types::{ArcDegrees, Azimuth, Elevation, SampleRate, Samples, Spread, StereoWidth};

use crate::AngleSmoother;

/// Maximum number of speakers supported (Atmos 7.1.4).
pub(crate) const MAX_SPEAKERS: usize = tutti_dsp::MAX_SPEAKERS;

/// VBAP panner internals. Use `VbapPannerNode` instead.
///
/// Holds no control state: the node reads its cells once per block and
/// passes the target, spread and width in, already normalized (bearing
/// wrapped, height and spread clamped).
#[derive(Clone)]
pub(crate) struct VbapPanner {
    layout: SpeakerLayout,
    smoother: AngleSmoother,
    /// The target bits and block length the smoother has reached a fixed
    /// point for: advancing it that many frames toward this target returns
    /// its own state unchanged, bit for bit, so the advance is skipped.
    /// Cleared by anything that moves the state or the coefficient (a
    /// reseed, a new sample rate).
    settled_at: Option<([u32; 2], usize)>,
    /// The last [`frame_gains`](Self::frame_gains) result and the input bits
    /// it was solved for (bearing, height, width, spread). The solve is a
    /// pure function of those four, so a held source reuses it instead of
    /// running two VBAP solves per block.
    last_solve: Option<([u32; 4], BlockGains)>,
}

impl std::fmt::Debug for VbapPanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VbapPanner")
            .field("layout", &self.layout)
            .field("smoother", &self.smoother)
            .field("settled_at", &self.settled_at)
            .field("last_solve", &self.last_solve)
            .finish_non_exhaustive()
    }
}

impl VbapPanner {
    fn new_with_layout(layout: SpeakerLayout) -> Self {
        let sample_rate = SampleRate(48000.0);
        Self {
            layout,
            smoother: AngleSmoother::new(sample_rate, Samples(1)),
            settled_at: None,
            last_solve: None,
        }
    }

    pub(crate) fn stereo() -> Result<Self> {
        Ok(Self::new_with_layout(SpeakerLayout::stereo()?))
    }

    pub(crate) fn quad() -> Result<Self> {
        Ok(Self::new_with_layout(SpeakerLayout::quad()?))
    }

    pub(crate) fn surround_5_1() -> Result<Self> {
        Ok(Self::new_with_layout(SpeakerLayout::surround_5_1()?))
    }

    pub(crate) fn surround_7_1() -> Result<Self> {
        Ok(Self::new_with_layout(SpeakerLayout::surround_7_1()?))
    }

    pub(crate) fn atmos_7_1_4() -> Result<Self> {
        Ok(Self::new_with_layout(SpeakerLayout::atmos_7_1_4()?))
    }

    /// Retune the position smoothers so the 50ms de-zipper ramp holds at any
    /// sample rate (the smoothers are built at 48kHz in `new_with_layout`).
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.smoother.set_sample_rate(sample_rate);
        // A new coefficient can move a state the old one held still.
        self.settled_at = None;
    }

    /// Drop the in-flight de-zipper ramp, seating it on the commanded
    /// `(azimuth, elevation)`.
    ///
    /// The smoother is this panner's only per-block history; the commanded
    /// position is the caller's, which is why it is passed in.
    pub(crate) fn reset_state(&mut self, azimuth: Azimuth, elevation: Elevation) {
        self.smoother.reset_to_target(azimuth, elevation);
        self.settled_at = None;
    }

    /// The gains for one frame at the smoothed position `(azimuth, elevation)`.
    ///
    /// `width < 0.001` folds the pair to mono and pans one source; otherwise
    /// the two channels become two virtual sources one `width`-scaled offset
    /// either side of the bearing.
    ///
    /// # Spread applies to both virtual sources
    ///
    /// The `width > 0` branch must reach spread (`SpeakerLayout::gains`)
    /// too, or a spread set on a node fed any non-zero width would silently do
    /// nothing. `width` and
    /// `spread` are orthogonal: width says how far apart the two virtual
    /// sources sit, spread how far each one is smeared across the speaker
    /// field, so neither may suppress the other. Both go through
    /// [`SpeakerLayout::gains`], which also gives each virtual
    /// source unit energy on its own — without that, the width branch would
    /// reintroduce the rear-arc silence the mono branch avoids.
    /// Covered by `tests/vbap_energy_sweep.rs::spread_reaches_the_stereo_width_path`.
    fn frame_gains(
        &mut self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
        spread: Spread,
    ) -> BlockGains {
        let key = [
            azimuth.get().to_bits(),
            elevation.get().to_bits(),
            width.get().to_bits(),
            spread.get().to_bits(),
        ];
        if let Some((solved_for, gains)) = self.last_solve
            && solved_for == key
        {
            return gains;
        }
        let gains = self.solve_frame(azimuth, elevation, width, spread);
        self.last_solve = Some((key, gains));
        gains
    }

    /// [`frame_gains`](Self::frame_gains) without the reuse.
    fn solve_frame(
        &mut self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
        spread: Spread,
    ) -> BlockGains {
        // Unwrapped once: the angle offset below scales the scalar.
        let width = width.get().max(0.0);
        if width < 0.001 {
            return BlockGains {
                mono: true,
                a: self.layout.gains(azimuth, elevation, spread),
                b: [0.0; MAX_SPEAKERS],
            };
        }
        // The two virtual sources sit one offset either side of the bearing.
        // `rotate_by`, not `+`: the sum crosses the seam. At azimuth 170 with
        // full width the left source is at 185, which *is* -175 — the same
        // wraparound the node normalizes the target to, re-introduced here by
        // adding to the already-wrapped value.
        let angle_offset = ArcDegrees(15.0 * width);
        BlockGains {
            mono: false,
            a: self
                .layout
                .gains(azimuth.rotate_by(angle_offset), elevation, spread),
            b: self
                .layout
                .gains(azimuth.rotate_by(-angle_offset), elevation, spread),
        }
    }

    /// The gains at the smoother's current position, without advancing
    /// it — the starting point of the first ramp after a construction or a
    /// reset.
    pub(crate) fn gains_now(&mut self, width: StereoWidth, spread: Spread) -> BlockGains {
        let (azimuth, elevation) = self.smoother.current();
        self.frame_gains(azimuth, elevation, width, spread)
    }

    /// Advance the de-zipper `frames` steps toward the target and solve the
    /// gains once, at the block's last frame.
    ///
    /// The smoother advances `frames` steps in one analytic move
    /// ([`ExponentialSmoother::advance`](crate::smoothing::ExponentialSmoother::advance)),
    /// so the 50 ms ramp keeps its timing. The frames in between are not
    /// solved: the node ramps linearly towards this set instead, which saves
    /// two VBAP solves per frame.
    ///
    /// An advance is a pure function of the smoother's state, the target and
    /// the block length, so once one returns the state unchanged the same
    /// advance would again: a held source skips it.
    pub(crate) fn solve_block(
        &mut self,
        target: (Azimuth, Elevation),
        width: StereoWidth,
        spread: Spread,
        frames: usize,
    ) -> BlockGains {
        let settled = ([target.0.get().to_bits(), target.1.get().to_bits()], frames);
        if self.settled_at != Some(settled) {
            let before = self.smoother.current();
            let after = self.smoother.advance(target.0, target.1, frames);
            self.settled_at = same_bits(after, before).then_some(settled);
        }
        let (azimuth, elevation) = self.smoother.current();
        self.frame_gains(azimuth, elevation, width, spread)
    }
}

/// Whether two positions are the same bits (not merely `==`: `-0.0` and
/// `0.0` are different smoother states).
#[inline]
fn same_bits(a: (Azimuth, Elevation), b: (Azimuth, Elevation)) -> bool {
    a.0.get().to_bits() == b.0.get().to_bits() && a.1.get().to_bits() == b.1.get().to_bits()
}

/// One frame's speaker gains, as the start or end point of a block's ramp.
///
/// `mono` records which branch produced them: a mono set is one gain vector
/// (`a`) applied to the folded pair, a stereo set is two — `a` for the left
/// virtual source, `b` for the right. Entries past the speaker count are zero.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockGains {
    pub(crate) mono: bool,
    pub(crate) a: [f32; MAX_SPEAKERS],
    pub(crate) b: [f32; MAX_SPEAKERS],
}

impl BlockGains {
    /// Speaker `s`'s gains as a (left, right) pair, whatever the branch.
    ///
    /// A mono set weighs each channel by half its gain — `fold(l, r) * g` is
    /// `l * g/2 + r * g/2` up to rounding — which is what lets a ramp cross
    /// from one branch to the other without a step.
    #[inline]
    pub(crate) fn pair(&self, s: usize) -> (f32, f32) {
        if self.mono {
            let half = self.a[s] * 0.5;
            (half, half)
        } else {
            (self.a[s], self.b[s])
        }
    }
}

#[cfg(test)]
mod tests;
