//! [`Design`]: the pure half of a reverb algorithm, and [`Bounds`], its worst
//! case.

use tutti_types::{ChannelLayout, Db, SampleRate, Samples, Seconds};

use super::{Kernel, Network, Ring, Stage};

/// The channel widths a [`Design`] is made for: what its node's ports and its
/// kernel's [`Setup`](super::Setup) are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channels {
    /// The input width.
    pub input: ChannelLayout,
    /// The output width.
    pub output: ChannelLayout,
}

/// An algorithm of reverb (design 022 section 4): its controls, the function
/// from controls to a [`Network`], and the [`Kernel`] that runs the sound.
///
/// The pure half is [`design_into`](Self::design_into) and
/// [`bounds`](Self::bounds): no audio, no state and no graph type. The network
/// is the description the kernel is held to ([`Kernel::retarget`] reads it, and
/// the kernel contract measures the kernel against its queries); nothing here
/// forces a runtime shape on the kernel.
pub trait Design: Clone + Send {
    /// Plain values in tutti units, one struct per algorithm.
    type Controls: Copy + PartialEq + Send;

    /// What runs this design's networks.
    type Kernel: Kernel;

    /// The widths of the signal this design takes and gives.
    fn channels(&self) -> Channels;

    /// Writes the network for `controls` at `rate` into `out`, replacing all of
    /// it.
    ///
    /// Pure and allocation-free: the same controls give the same network, with
    /// no clock, no state and no heap. It writes **in place** because a
    /// [`Network`] is about 56 KB, too large to build as a value on an audio
    /// thread (whose stack may be 512 KB); a node designs into a network it
    /// owns, and the implementation pushes into `out` without building a
    /// whole one. Total: a control outside its range is clamped and a
    /// non-finite one takes the algorithm's default, so the network is always
    /// one the algorithm can run.
    fn design_into(&self, controls: &Self::Controls, rate: SampleRate, out: &mut Network);

    /// The network for `controls` at `rate`, as a value: a test, a UI and the
    /// control side call this; an audio thread calls
    /// [`design_into`](Self::design_into).
    fn design(&self, controls: &Self::Controls, rate: SampleRate) -> Network {
        let mut network = Network::new(rate);
        self.design_into(controls, rate, &mut network);
        network
    }

    /// The worst case of [`design_into`](Self::design_into) over the controls'
    /// declared ranges (and the ceilings in `ceiling`): what a kernel allocates
    /// for and what the node declares as its tail ceiling.
    fn bounds(&self, ceiling: &Ceiling) -> Bounds;

    /// An unprepared kernel for this design. Control side; allocates.
    fn kernel(&self) -> Self::Kernel;
}

/// The limits a node builds a reverb to: the rate, the longest decay a control
/// may ask for and the level the tail is measured to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ceiling {
    rate: SampleRate,
    max_decay: Seconds,
    floor: Db,
}

impl Ceiling {
    /// The default longest decay a control may ask for.
    pub const MAX_DECAY: Seconds = Seconds(8.0);
    /// The most [`new`](Self::new) accepts for the longest decay: bounds sized
    /// from it become a kernel's allocations, so it is capped (a 60 s decay at
    /// 192 kHz is already some 11.5 M frames of line to hold).
    pub const MAX_DECAY_CAP: Seconds = Seconds(60.0);
    /// The default tail floor.
    pub const FLOOR: Db = Db(-60.0);

    /// A ceiling, or `None` when the rate is not positive and finite, the
    /// decay is not positive and finite or is above
    /// [`MAX_DECAY_CAP`](Self::MAX_DECAY_CAP), or the floor is not finite and below
    /// 0 dB.
    #[must_use]
    pub fn new(rate: SampleRate, max_decay: Seconds, floor: Db) -> Option<Self> {
        let ok = rate.get().is_finite()
            && rate.get() > 0.0
            && max_decay.get().is_finite()
            && max_decay.get() > 0.0
            && max_decay.get() <= Self::MAX_DECAY_CAP.get()
            && floor.get().is_finite()
            && floor.get() < 0.0;
        ok.then_some(Self {
            rate,
            max_decay,
            floor,
        })
    }

    /// The defaults at `rate` ([`MAX_DECAY`](Self::MAX_DECAY),
    /// [`FLOOR`](Self::FLOOR)), or `None` for a rate that is not positive and
    /// finite.
    #[must_use]
    pub fn at(rate: SampleRate) -> Option<Self> {
        Self::new(rate, Self::MAX_DECAY, Self::FLOOR)
    }

    /// The rate.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The longest decay a control may ask for.
    #[must_use]
    pub fn max_decay(&self) -> Seconds {
        self.max_decay
    }

    /// The level the tail is measured to.
    #[must_use]
    pub fn floor(&self) -> Db {
        self.floor
    }
}

/// The size of a [`Network`], or the worst case over a design's controls: every
/// figure a kernel allocates from.
///
/// [`Bounds::of`] measures one network; [`dominates`](Self::dominates) says one
/// bound holds another.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    /// Delay lines in all.
    pub lines: usize,
    /// Stages.
    pub stages: usize,
    /// Taps in all.
    pub taps: usize,
    /// Output channels of the widest recirculating stage.
    pub outputs: usize,
    /// The longest single line, in frames, with its nested all-pass and its
    /// modulation excursion.
    pub line: f32,
    /// The total delay of every line, all-pass and tap, in frames, with the
    /// modulation excursions.
    pub delay: f32,
    /// The longest FIR.
    pub ir: Samples,
    /// The longest pre-delay.
    pub pre_delay: Samples,
    /// The longest [`tail`](Network::tail) at [`Ceiling::floor`].
    pub tail: Ring,
}

impl Bounds {
    /// The size of `network`, its tail measured to `floor`.
    #[must_use]
    pub fn of(network: &Network, floor: Db) -> Self {
        let mut b = Bounds {
            lines: 0,
            stages: network.stages.len(),
            taps: 0,
            outputs: 0,
            line: 0.0,
            delay: 0.0,
            ir: Samples::ZERO,
            pre_delay: network.pre_delay,
            tail: network.tail(floor),
        };
        for stage in network.stages.iter() {
            match stage {
                Stage::Taps(t) => {
                    b.taps += t.taps.len();
                    b.delay += t.taps.iter().map(|t| t.delay).fold(0.0, f32::max);
                }
                Stage::AllPass(a) => {
                    b.delay += a
                        .iter()
                        .map(|a| a.delay + a.modulation.depth.abs())
                        .sum::<f32>();
                }
                Stage::Fdn(f) => {
                    b.lines += f.lines.len();
                    b.outputs = b.outputs.max(f.output.rows.len());
                    for l in f.lines.iter() {
                        let len = l.effective_length() + l.modulation.depth.abs();
                        b.line = b.line.max(len);
                        b.delay += len;
                    }
                }
                Stage::Fir(s) => b.ir = b.ir.max(s.length),
            }
        }
        b
    }

    /// Whether this bound holds `other` in every figure.
    #[must_use]
    pub fn dominates(&self, other: &Bounds) -> bool {
        self.lines >= other.lines
            && self.stages >= other.stages
            && self.taps >= other.taps
            && self.outputs >= other.outputs
            && self.line >= other.line
            && self.delay >= other.delay
            && self.ir >= other.ir
            && self.pre_delay >= other.pre_delay
            && self.tail.covers(other.tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ceiling refuses what it cannot hold and never clamps. (It has one
    /// constructor and no raw to convert from or to, so
    /// `tutti_fixtures::check_range_contract`, which checks the `new`/`TryFrom`/
    /// `of`/serde shape of a one-raw type, does not apply.)
    ///
    /// Mutations (run): dropping the `floor.get() < 0.0` test accepts a 0 dB
    /// floor and fails the last row; dropping the `MAX_DECAY_CAP` test accepts
    /// 60.001 s and fails the decay row.
    #[test]
    fn a_ceiling_refuses_what_it_cannot_hold() {
        let rate = SampleRate(48_000.0);
        let (decay, floor) = (Seconds(8.0), Db(-60.0));
        let c = Ceiling::new(rate, decay, floor).unwrap();
        assert_eq!((c.rate(), c.max_decay(), c.floor()), (rate, decay, floor));
        assert_eq!(Ceiling::at(rate), Some(c));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(Ceiling::at(SampleRate(bad)).is_none(), "rate {bad}");
        }
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, 60.001, 1e9] {
            assert!(
                Ceiling::new(rate, Seconds::from_f32(bad), floor).is_none(),
                "decay {bad}"
            );
        }
        assert!(Ceiling::new(rate, Ceiling::MAX_DECAY_CAP, floor).is_some());
        for bad in [0.0, 3.0, f32::NAN, f32::NEG_INFINITY] {
            assert!(Ceiling::new(rate, decay, Db(bad)).is_none(), "floor {bad}");
        }
    }
}
