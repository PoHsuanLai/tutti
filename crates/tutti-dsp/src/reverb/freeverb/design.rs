//! [`FreeverbDesign`]: [`Freeverb`] as a [`Design`].
//!
//! The network describes the linear filters of the reverb in `kernel.rs`: per
//! channel eight parallel damped combs (a diagonal [`Fdn`] of one-pole lines,
//! the right channel's 23 frames longer) feeding four series all-passes in
//! [`AllPassForm::Freeverb`] (Freeverb's own, not the unit-magnitude lattice),
//! at Freeverb's tunings and constants. What it leaves out is the realization's
//! flush of any value under 1e-6 to zero, a nonlinearity far below the tail's
//! floor that no query models. The comb loop gain is `room * 0.28 + 0.7` and the
//! damping `damp * 0.4`, computed in the same `f32` operations as
//! [`Freeverb`]'s constants. The realization is [`Realized`] over
//! [`FreeverbKernel`], which reads those two numbers from the
//! network.

use core::num::NonZeroU32;

use tutti_types::{Amplitude, BusTopology, Mix, SampleRate};

use crate::reverb::fdn::kernel::CHUNK;
use crate::reverb::freeverb::kernel::{Freeverb, FreeverbKernel};
use crate::reverb::network::{
    Absorption, AllPass, AllPassForm, Fdn, Gains, Inline, Line, MAX_CHANNELS, MAX_LINES, Matrix,
    Modulation, Network, NetworkSizes, OutputMap, Stage,
};
use crate::{Cadence, Ceiling, Channels, Design, MaxDecay, Realized, Sizes};

const COMBS_LEFT: [usize; 8] = [
    Freeverb::CF_TUNING_L1,
    Freeverb::CF_TUNING_L2,
    Freeverb::CF_TUNING_L3,
    Freeverb::CF_TUNING_L4,
    Freeverb::CF_TUNING_L5,
    Freeverb::CF_TUNING_L6,
    Freeverb::CF_TUNING_L7,
    Freeverb::CF_TUNING_L8,
];
const COMBS_RIGHT: [usize; 8] = [
    Freeverb::CF_TUNING_R1,
    Freeverb::CF_TUNING_R2,
    Freeverb::CF_TUNING_R3,
    Freeverb::CF_TUNING_R4,
    Freeverb::CF_TUNING_R5,
    Freeverb::CF_TUNING_R6,
    Freeverb::CF_TUNING_R7,
    Freeverb::CF_TUNING_R8,
];
const ALL_PASSES: [[usize; 2]; 4] = [
    [Freeverb::APF_TUNING_L1, Freeverb::APF_TUNING_R1],
    [Freeverb::APF_TUNING_L2, Freeverb::APF_TUNING_R2],
    [Freeverb::APF_TUNING_L3, Freeverb::APF_TUNING_R3],
    [Freeverb::APF_TUNING_L4, Freeverb::APF_TUNING_R4],
];

// The network is built by pushing into fixed lists; these are what makes the
// pushes infallible.
const _: () = assert!(COMBS_LEFT.len() + COMBS_RIGHT.len() <= MAX_LINES);
const _: () = assert!(2 <= MAX_CHANNELS);

/// Freeverb's two controls, as positions on `0..1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreeverbControls {
    /// The room size: scales the comb loop gain, `room * 0.28 + 0.7`.
    pub room: Mix,
    /// The damping: the comb's one-pole, `damp * 0.4`.
    pub damp: Mix,
}

impl FreeverbControls {
    /// The General MIDI reverb: room 0.5, damping 0.5.
    pub const GENERAL_MIDI: FreeverbControls = FreeverbControls {
        room: Mix(0.5),
        damp: Mix(0.5),
    };
}

impl Default for FreeverbControls {
    fn default() -> Self {
        Self::GENERAL_MIDI
    }
}

/// The [`Design`] of [`Freeverb`]: mono in, stereo out, two controls.
///
/// A control outside `0..1` is clamped, and a non-finite one takes the General
/// MIDI value, so every control gives a stable network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeverbDesign;

fn position(mix: Mix, default: f32) -> f32 {
    let v = mix.get();
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        default
    }
}

impl Design for FreeverbDesign {
    type Controls = FreeverbControls;
    type Model = Network;
    type Bounds = NetworkSizes;
    type Range = MaxDecay;
    type Realization = Realized<FreeverbKernel>;

    // The comb feedback and damping glide in whole chunks counted from the last
    // reset, so the render is a function of the stream whatever the blocks (the
    // design is still sampled per block, as for the FDN, doc 024 Q27).
    const CADENCE: Cadence = Cadence::stream(match NonZeroU32::new(CHUNK as u32) {
        Some(n) => n,
        None => unreachable!(),
    });

    fn channels(&self) -> Channels {
        Channels::new(BusTopology::MONO, BusTopology::STEREO)
    }

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    /// A [`Freeverb`] at the default rate; the node's `prepare` re-tunes it.
    fn realization(&self) -> Realized<FreeverbKernel> {
        Realized::new(FreeverbKernel::new(), &self.blank(SampleRate::DEFAULT))
    }

    fn design_into(&self, controls: &FreeverbControls, rate: SampleRate, out: &mut Network) {
        // `Freeverb::new` rounds the rate to whole hertz and scales the tunings
        // from it, so the lines are those of a reverb built at `rate`.
        let rate = SampleRate(rate.get().round());
        let room = position(controls.room, FreeverbControls::GENERAL_MIDI.room.get());
        let damp = position(controls.damp, FreeverbControls::GENERAL_MIDI.damp.get());
        // The same operations, in the same order, as `Freeverb::ROOM_FEEDBACK`
        // and `Freeverb::DAMPING`.
        let absorption = Absorption::OnePole {
            gain: Amplitude(room * 0.28 + 0.7),
            damp: damp * 0.4,
        };

        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for &tuning in COMBS_LEFT.iter().chain(&COMBS_RIGHT) {
            let line = Line {
                length: Freeverb::scale_tuning(rate, tuning) as f32,
                modulation: Modulation::NONE,
                absorption,
                diffuser: None,
            };
            let _ = lines.push(line);
        }
        let n = lines.len();

        // Left row: the first eight lines; right row: the next eight.
        let mut left = Gains::filled(0.0, n).unwrap_or_default();
        let mut right = Gains::filled(0.0, n).unwrap_or_default();
        for (i, (l, r)) in left
            .as_mut_slice()
            .iter_mut()
            .zip(right.as_mut_slice())
            .enumerate()
        {
            if i < COMBS_LEFT.len() {
                *l = 1.0;
            } else {
                *r = 1.0;
            }
        }
        let mut rows = Inline::new();
        let _ = rows.push(left);
        let _ = rows.push(right);

        out.clear(rate);
        let _ = out.stages.push(Stage::Fdn(Fdn {
            lines,
            matrix: Matrix::Diagonal,
            input: Gains::filled(Freeverb::FIXED_GAIN, n).unwrap_or_default(),
            output: OutputMap { rows },
        }));
        for [l, r] in ALL_PASSES {
            let all_pass = |tuning| AllPass {
                delay: Freeverb::scale_tuning(rate, tuning) as f32,
                gain: Freeverb::ALLPASS_FEEDBACK,
                modulation: Modulation::NONE,
                form: AllPassForm::Freeverb,
            };
            let mut channels = Inline::new();
            let _ = channels.push(all_pass(l));
            let _ = channels.push(all_pass(r));
            let _ = out.stages.push(Stage::AllPass(channels));
        }
    }

    /// Freeverb's lengths and stages are constants and its decay is longest at
    /// room 1 (the low band's loop gain is the room's alone), so the worst case
    /// is that design. `max_decay` does not apply: Freeverb has no decay
    /// control, and its longest RT60 is what room 1 makes it.
    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> NetworkSizes {
        let worst = FreeverbControls {
            room: Mix(1.0),
            damp: Mix(0.0),
        };
        NetworkSizes::measure(&self.design(&worst, ceiling.rate()), ceiling.floor())
    }
}

#[cfg(test)]
mod tests;

/// The realization contract over Freeverb: the realization is `Freeverb` itself, held to
/// its network's analytic figures.
#[cfg(test)]
mod realization_tests;
