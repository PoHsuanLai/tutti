//! [`BrickwallFamily`]: the hard clipper as a [`Family`] (design 024 section 3, row 8).
//!
//! A brickwall clamps every sample to `+-c` and has nothing else: no state, no
//! envelope, no lookahead and so no latency and no tail. It is the one family whose
//! only control is a **fast lane**: the ceiling is read per frame from the
//! [`Feed`] when the graph drives it (design 024 section 4.5, "Where cadence stops"),
//! and the member's ceiling is the base for a frame nobody drives.
//!
//! The ceiling is in dB and the clamp bound is its amplitude. `+inf` dB passes
//! everything and `-inf` dB mutes (both are ceilings); only a NaN has no level, and it
//! is ignored wherever it comes from (the node keeps the last ceiling for a NaN
//! cell, and a NaN frame on the lane clamps at the member's ceiling).

use core::ops::Range;

use tutti_types::{BusTopology, Db, SampleRate, Samples, Seconds};

use crate::{Channels, Effort, Family, Feed, Glide, Recurrence, Ring, Start, System};

/// The controls of a [`BrickwallFamily`]: the ceiling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallControls {
    /// The level samples are clamped to, in dB (`+-inf` are ceilings; a NaN has no
    /// level and falls back to the default, 0 dB).
    pub ceiling: Db,
}

impl BrickwallControls {
    /// 0 dB: the full scale.
    pub const DEFAULT: Self = Self { ceiling: Db(0.0) };
}

impl Default for BrickwallControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The amplitude of `ceiling`, or `None` for a NaN, which has none.
#[inline]
fn amplitude(ceiling: Db) -> Option<f32> {
    let linear = ceiling.to_amplitude().get();
    (!linear.is_nan()).then_some(linear)
}

/// `x` clamped to `+-ceiling`. `ceiling` is never NaN (`clamp` panics on one) and
/// never negative.
#[inline]
fn clip(x: f32, ceiling: f32) -> f32 {
    x.clamp(-ceiling, ceiling)
}

/// A brickwall at one ceiling: `y = clamp(x, -c, c)`, memoryless, so no latency and no
/// tail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallMember {
    ceiling: f32,
}

impl BrickwallMember {
    /// The clamp bound, as an amplitude (never NaN).
    #[must_use]
    pub fn ceiling(&self) -> f32 {
        self.ceiling
    }

    /// `x` through the clip: `clamp(x, -c, c)`.
    #[must_use]
    pub fn apply(&self, x: f32) -> f32 {
        clip(x, self.ceiling)
    }
}

impl System for BrickwallMember {
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn tail(&self, _: Db) -> Ring {
        Ring::Finite(Seconds(0.0))
    }
}

/// The [`Family`] of the hard clipper, as wide as its input bus. Every channel is
/// clamped to the same ceiling.
#[derive(Clone, Copy, Debug)]
pub struct BrickwallFamily {
    input: BusTopology,
}

impl BrickwallFamily {
    /// A brickwall on the bus `input`, one output bus where it is.
    #[must_use]
    pub const fn new(input: BusTopology) -> Self {
        Self { input }
    }
}

impl Family for BrickwallFamily {
    type Controls = BrickwallControls;
    type Member = BrickwallMember;
    type Recurrence = BrickwallRecurrence;

    fn channels(&self) -> Channels {
        Channels::same(self.input)
    }

    fn blank(&self, _: SampleRate) -> BrickwallMember {
        BrickwallMember { ceiling: 1.0 }
    }

    fn member_into(&self, c: &BrickwallControls, _: SampleRate, out: &mut BrickwallMember) {
        out.ceiling = amplitude(c.ceiling).unwrap_or(1.0);
    }

    fn recurrence(&self) -> BrickwallRecurrence {
        BrickwallRecurrence
    }
}

/// The [`Recurrence`] of a [`BrickwallFamily`]: the clamp at the ceiling the member
/// asks for, or at each frame's own when lane 0 of the [`Feed`] drives it.
///
/// **Lane 0 is the ceiling in dB, per frame.** A NaN frame clamps at the member's
/// ceiling. The ceiling lands whole (a glided bound would let a sample past the new
/// one), and the clip is memoryless, so the render does not depend on how the input
/// is cut into blocks.
#[derive(Clone, Copy, Debug)]
pub struct BrickwallRecurrence;

impl Recurrence for BrickwallRecurrence {
    type Member = BrickwallMember;
    type Capacity = ();
    type Target = f32;
    type Coeffs = Glide<f32>;
    type State = ();

    const STEP: usize = 1;

    fn start(&self, member: &BrickwallMember) -> Start<Self> {
        Start {
            coeffs: Glide::landed(member.ceiling).by(0),
            state: (),
            target: member.ceiling,
        }
    }

    fn coefficients_into(&self, member: &BrickwallMember, target: &mut f32) -> bool {
        *target = member.ceiling;
        true
    }

    fn clear(&self, _: &mut ()) {}

    fn run(
        &self,
        coeffs: &Glide<f32>,
        _: &mut (),
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let base = coeffs.now();
        match feed.lanes[0] {
            // Each frame's ceiling is converted once, for every channel.
            Some(lane) => {
                for i in frames {
                    let ceiling = amplitude(Db(lane[i])).unwrap_or(base);
                    for (out, input) in output.iter_mut().zip(feed.audio) {
                        out[i] = clip(input[i], ceiling);
                    }
                }
            }
            None => {
                for (out, input) in output.iter_mut().zip(feed.audio) {
                    for (o, &x) in out[frames.clone()].iter_mut().zip(&input[frames.clone()]) {
                        *o = clip(x, base);
                    }
                }
            }
        }
    }

    fn effort(&self) -> Effort {
        Effort::Light
    }
}

#[cfg(test)]
mod tests;
