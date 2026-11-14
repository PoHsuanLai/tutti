//! The contract is public under the `contract` feature, and
//! a family outside this crate runs it: a gain, the smallest system with a
//! frequency response, through `family_contract!` and `recurrence_contract!`,
//! against the `Lti` group (the reverb group is for a `Network`).
//!
//! The gain slews to a new value over 64 frames from the first frame since the
//! last reset, so it is the same on every block cut (the stream clock) and
//! lands at once on its first retarget (the `landed` case), as a realization's
//! ramp must.

use std::num::NonZeroU32;
use std::ops::Range;

use tutti_dsp::contract::{FamilyCase, Figure, Hook, LtiCase, RecurrenceCase};
use tutti_dsp::{
    Cadence, Channels, Complex64, Family, Feed, Glide, Lti, Recurrence, Ring, Start, System,
    family_contract, recurrence_contract,
};
use tutti_types::BusTopology;
use tutti_types::{Db, Hz, SampleRate, Samples};

/// The system: `y = g x`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GainModel {
    gain: f32,
}

impl System for GainModel {
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn tail(&self, _: Db) -> Ring {
        Ring::Finite(tutti_types::Seconds::new(0.0))
    }
}

impl Lti for GainModel {
    fn frequency_response(&self, _: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        gain.iter_mut()
            .zip(freqs)
            .for_each(|(g, _)| *g = Complex64::new(f64::from(self.gain), 0.0));
    }

    fn spectral_radius(&self) -> f64 {
        0.0
    }

    fn l1_norm(&self) -> f64 {
        f64::from(self.gain.abs())
    }
}

#[derive(Clone, Copy, Debug)]
struct GainFamily;

/// The recurrence: `y = g x` at the gain as it stands, the gain slewed over 64 frames
/// from the last reset by a [`Glide`] (γ), toward the gain the model asks for (κ).
#[derive(Clone, Debug)]
struct GainRecurrence;

const SLEW: u32 = 64;

impl Recurrence for GainRecurrence {
    type Member = GainModel;
    type Capacity = ();
    type Target = f32;
    type Coeffs = Glide<f32>;
    type State = ();

    const STEP: usize = 1;

    fn start(&self, model: &GainModel) -> Start<Self> {
        Start {
            coeffs: Glide::landed(model.gain).by(SLEW),
            state: (),
            target: model.gain,
        }
    }

    fn coefficients_into(&self, model: &GainModel, target: &mut f32) -> bool {
        *target = model.gain;
        true
    }

    fn clear(&self, _: &mut ()) {}

    fn run(
        &self,
        gain: &Glide<f32>,
        _: &mut (),
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let g = gain.now();
        for i in frames {
            output[0][i] = g * feed.audio[0][i];
        }
    }
}

impl Family for GainFamily {
    type Controls = f32;
    type Member = GainModel;
    type Recurrence = GainRecurrence;
    // The slew counts frames from the last reset: a per-frame stream grid.
    const CADENCE: Cadence = Cadence::stream(NonZeroU32::MIN);

    fn channels(&self) -> Channels {
        Channels::same(BusTopology::MONO)
    }

    fn blank(&self, _: SampleRate) -> GainModel {
        GainModel { gain: 0.0 }
    }

    fn member_into(&self, g: &f32, _: SampleRate, out: &mut GainModel) {
        out.gain = if g.is_finite() {
            g.clamp(0.0, 0.9)
        } else {
            0.5
        };
    }

    fn recurrence(&self, _: SampleRate) -> GainRecurrence {
        GainRecurrence
    }
}

impl FamilyCase for GainFamily {
    fn capacity(&self, _: SampleRate) {}

    fn reachable(&self) -> Vec<f32> {
        vec![0.1, 0.25, 0.5, 0.75, 0.9]
    }

    fn hostile(&self) -> Vec<f32> {
        vec![f32::NAN, -3.0, 40.0]
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[]
    }
}

impl RecurrenceCase for GainFamily {
    fn probes(&self) -> Vec<f32> {
        vec![0.3, 0.8]
    }

    fn extremes(&self) -> Vec<f32> {
        vec![0.9]
    }

    fn steps(&self) -> Vec<(f32, f32)> {
        vec![(0.2, 0.7), (0.9, 0.1)]
    }
}

impl LtiCase for GainFamily {
    /// A gain is its value at every frequency.
    fn figures(&self) -> Vec<Figure<f32>> {
        [0.1, 0.5, 0.9]
            .into_iter()
            .map(|g| Figure {
                controls: g,
                rate: SampleRate::SR_48K,
                output: 0,
                freq: Hz(1_000.0),
                gain: f64::from(g),
                tolerance: 1e-6,
            })
            .collect()
    }
}

mod design {
    use super::*;
    family_contract!(GainFamily, Lti);
}

mod realization {
    use super::*;
    recurrence_contract!(GainFamily, Lti, BlockIndependent);
}
