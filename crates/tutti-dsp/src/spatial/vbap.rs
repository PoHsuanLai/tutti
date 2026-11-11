//! The VBAP block kernel: one block's speaker gains at the two ends of a
//! ramp, and one output channel rendered between them.
//!
//! The caller smooths the position and reads its controls; it hands
//! [`VbapKernel::gains`] the bearing, height, width and spread of the block's
//! last frame, keeps the result of the previous block, and renders each
//! speaker's channel with [`VbapGains::render_channel`].

use tutti_types::{ArcDegrees, Azimuth, Elevation, Fold, Spread, StereoWidth};

mod error;
mod layout;
mod math;
mod select;

pub use error::SpeakerGeometryError;
pub use layout::{MAX_SPEAKERS, PanningMode, SpeakerLayout, Tuple};

/// A width below this folds the stereo pair to mono and pans one source.
const MONO_WIDTH: f32 = 0.001;

/// How far each virtual source sits from the bearing at width 1.
const WIDTH_HALF_ANGLE: f32 = 15.0;

/// One frame's speaker gains, as the start or end point of a block's ramp.
///
/// `mono` records which branch produced them: a mono set is one gain vector
/// (`a`) applied to the folded pair, a stereo set is two: `a` for the left
/// virtual source, `b` for the right. Entries past the speaker count are zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VbapGains {
    mono: bool,
    a: [f32; MAX_SPEAKERS],
    b: [f32; MAX_SPEAKERS],
}

impl VbapGains {
    /// Speaker `s`'s gains as a (left, right) pair, whatever the branch.
    ///
    /// A mono set weighs each channel by half its gain (`fold(l, r) * g` is
    /// `l * g/2 + r * g/2` up to rounding), which is what lets a ramp cross
    /// from one branch to the other without a step.
    #[inline]
    pub fn pair(&self, s: usize) -> (f32, f32) {
        if self.mono {
            let half = self.a[s] * 0.5;
            (half, half)
        } else {
            (self.a[s], self.b[s])
        }
    }

    /// Render one output channel for one block: speaker `speaker`'s gain
    /// ramped linearly from `from` to `to`, applied to the stereo pair
    /// (`left`, `right`) into `out`. No speaker (the LFE) is silence.
    ///
    /// Frame `i` of `n` uses `to - (to - from) * (n - 1 - i) / n`, so the ramp
    /// starts one step past `from` (which the previous block already played)
    /// and its last frame is exactly `to`: written as a subtraction of a
    /// zero term, which is exact for any `from`, rather than
    /// `from + (to - from) * 1.0`, which is exact only when the two are
    /// within a factor of two. That is what makes a one-frame block the
    /// per-frame solve bit for bit, and a block edge the exact solve.
    ///
    /// When both ends came from the mono-fold branch the pair is folded
    /// (`mono * g`, `mono` the stereo-to-mono `fold` of each frame);
    /// otherwise each end is expressed as a (left, right) gain pair, so a
    /// width crossing the mono-fold threshold between blocks crossfades
    /// instead of stepping.
    #[inline]
    pub fn render_channel(
        from: &VbapGains,
        to: &VbapGains,
        speaker: Option<usize>,
        fold: &Fold,
        left: &[f32],
        right: &[f32],
        out: &mut [f32],
    ) {
        let Some(s) = speaker else {
            out.fill(0.0);
            return;
        };
        let n = out.len();
        let inv_n = 1.0 / n as f32;
        let remaining = |i: usize| (n - 1 - i) as f32 * inv_n;
        // A delta of exactly `+0.0` makes every ramp gain `g_to - (+0.0 * t)`,
        // which is `g_to` bit for bit (`t` is finite and non-negative; `-0.0`
        // is excluded because `-0.0 - -0.0` is `+0.0`), so a held gain skips
        // the ramp arithmetic with the same bits.
        if from.mono && to.mono {
            let (g_from, g_to) = (from.a[s], to.a[s]);
            let delta = g_to - g_from;
            let mono = |l: f32, r: f32| {
                let mut m = [0.0];
                fold.frame(&[l, r], &mut m);
                m[0]
            };
            if delta.to_bits() == 0 {
                for ((o, &l), &r) in out.iter_mut().zip(left).zip(right) {
                    *o = mono(l, r) * g_to;
                }
                return;
            }
            for (i, ((o, &l), &r)) in out.iter_mut().zip(left).zip(right).enumerate() {
                *o = mono(l, r) * (g_to - delta * remaining(i));
            }
        } else {
            let (a_from, b_from) = from.pair(s);
            let (a_to, b_to) = to.pair(s);
            let (da, db) = (a_to - a_from, b_to - b_from);
            if da.to_bits() == 0 && db.to_bits() == 0 {
                for ((o, &l), &r) in out.iter_mut().zip(left).zip(right) {
                    *o = l * a_to + r * b_to;
                }
                return;
            }
            for (i, ((o, &l), &r)) in out.iter_mut().zip(left).zip(right).enumerate() {
                let t = remaining(i);
                *o = l * (a_to - da * t) + r * (b_to - db * t);
            }
        }
    }
}

/// A speaker layout and the last solve over it.
#[derive(Clone, Debug)]
pub struct VbapKernel {
    layout: SpeakerLayout,
    /// The last [`gains`](Self::gains) result and the input bits it was solved
    /// for (bearing, height, width, spread). The solve is a pure function of
    /// those four, so a held source reuses it instead of running two VBAP
    /// solves per block.
    last_solve: Option<([u32; 4], VbapGains)>,
}

impl VbapKernel {
    /// A kernel over `layout`, with nothing solved yet.
    pub fn new(layout: SpeakerLayout) -> Self {
        Self {
            layout,
            last_solve: None,
        }
    }

    /// The gains for one frame at `(azimuth, elevation)`, already normalized
    /// (bearing wrapped, height and spread clamped).
    ///
    /// `width < 0.001` folds the pair to mono and pans one source; otherwise
    /// the two channels become two virtual sources one `width`-scaled offset
    /// either side of the bearing.
    ///
    /// # Spread applies to both virtual sources
    ///
    /// The `width > 0` branch must reach spread (`SpeakerLayout::gains_into`)
    /// too, or a spread set on a node fed any non-zero width would silently do
    /// nothing. `width` and `spread` are orthogonal: width says how far apart
    /// the two virtual sources sit, spread how far each one is smeared across
    /// the speaker field, so neither may suppress the other. Both go through
    /// [`SpeakerLayout::gains_into`], which also gives each virtual source
    /// unit energy on its own; without that, the width branch would
    /// reintroduce the rear-arc silence the mono branch avoids.
    pub fn gains(
        &mut self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
        spread: Spread,
    ) -> VbapGains {
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
        let gains = self.solve(azimuth, elevation, width, spread);
        self.last_solve = Some((key, gains));
        gains
    }

    /// [`gains`](Self::gains) without the reuse.
    fn solve(
        &self,
        azimuth: Azimuth,
        elevation: Elevation,
        width: StereoWidth,
        spread: Spread,
    ) -> VbapGains {
        // Unwrapped once: the angle offset below scales the scalar.
        let width = width.get().max(0.0);
        let count = self.layout.speaker_count();
        if width < MONO_WIDTH {
            let mut a = [0.0; MAX_SPEAKERS];
            self.layout
                .gains_into(azimuth, elevation, spread, &mut a[..count]);
            return VbapGains {
                mono: true,
                a,
                b: [0.0; MAX_SPEAKERS],
            };
        }
        let (left, right) = virtual_sources(azimuth, width);
        let (mut a, mut b) = ([0.0; MAX_SPEAKERS], [0.0; MAX_SPEAKERS]);
        self.layout
            .gains_into(left, elevation, spread, &mut a[..count]);
        self.layout
            .gains_into(right, elevation, spread, &mut b[..count]);
        VbapGains { mono: false, a, b }
    }
}

/// The two virtual sources, one `width`-scaled offset either side of the
/// bearing. `rotate_by`, not `+`: the sum crosses the seam. At azimuth 170 with
/// full width the left source is at 185, which *is* -175, the same wraparound
/// the node normalizes the target to, re-introduced here by adding to the
/// already-wrapped value.
fn virtual_sources(azimuth: Azimuth, width: f32) -> (Azimuth, Azimuth) {
    let offset = ArcDegrees(WIDTH_HALF_ANGLE * width);
    (azimuth.rotate_by(offset), azimuth.rotate_by(-offset))
}

#[cfg(test)]
mod tests;
