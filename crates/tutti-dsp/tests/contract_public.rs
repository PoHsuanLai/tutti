//! The contract is public under the `contract` feature (design 024 section 6), and
//! a family outside this crate runs it: a gain, the smallest system with a
//! frequency response, through `design_contract!` and `realization_contract!`,
//! against the `Lti` group (the reverb group is for a `Network`).
//!
//! The gain slews to a new value over 64 frames from the first frame since the
//! last reset, so it is the same on every block cut (the stream clock) and
//! lands at once on its first retarget (the `landed` case), as a realization's
//! ramp must.
//!
//! Mutations (run): a first `retarget` that slews from 0 instead of landing
//! (`from` 0 and `done` 0 for the first call) fails `realization_contract_landed`,
//! `reset`, `fresh`, `response` and `linear`;
//! a `process` that restarts the slew at every block (`done` clamped to 1 on
//! entry) fails `realization_contract_stream_clock` and `click_free`; a
//! `hinf_norm` of half the l1 norm is under the response's peak and fails
//! `contract_norms`; a response of `2 g` fails `contract_anchors` (and norms).

use tutti_dsp::contract::{Anchor, DesignCase, Hook, LtiCase, RealizationCase};
use tutti_dsp::{
    Cadence, Ceiling, ChannelLayout, Channels, Complex64, Design, Effect, Lti, Realization, Ring,
    Setup, Sizes, System, design_contract, realization_contract,
};
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

/// A gain has no sizes to allocate.
#[derive(Clone, Copy, Debug)]
struct NoSizes;

impl Sizes for NoSizes {
    type Source = GainModel;

    fn measure(_: &GainModel, _: Db) -> Self {
        NoSizes
    }

    fn tail(&self) -> Ring {
        Ring::Finite(tutti_types::Seconds::new(0.0))
    }

    fn dominates(&self, _: &Self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug)]
struct GainDesign;

/// The realization: the gain, slewed over 64 frames from the last reset.
#[derive(Clone, Debug)]
struct GainRealization {
    from: f32,
    to: f32,
    done: usize,
    landed: bool,
}

const SLEW: usize = 64;

impl Realization<GainModel> for GainRealization {
    type Bounds = NoSizes;

    fn prepare(&mut self, _: &NoSizes, _: &Setup) {
        *self = Self::new();
    }

    fn retarget(&mut self, model: &GainModel) {
        let now = self.current();
        self.from = if self.landed { now } else { model.gain };
        self.to = model.gain;
        self.done = if self.landed { 0 } else { SLEW };
        self.landed = true;
    }

    fn reset(&mut self) {
        self.from = self.to;
        self.done = SLEW;
    }

    fn fresh(&self) -> Self {
        let mut fresh = self.clone();
        fresh.reset();
        fresh
    }
}

impl GainRealization {
    fn new() -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            done: SLEW,
            landed: false,
        }
    }

    fn current(&self) -> f32 {
        let t = self.done as f32 / SLEW as f32;
        self.from + (self.to - self.from) * t.min(1.0)
    }
}

impl Effect<GainModel> for GainRealization {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        for i in 0..input[0].len() {
            self.done = (self.done + 1).min(SLEW);
            let g = self.current();
            output[0][i] = g * input[0][i];
        }
    }
}

impl Design for GainDesign {
    type Controls = f32;
    type Model = GainModel;
    type Bounds = NoSizes;
    type Range = ();
    type Realization = GainRealization;
    const CADENCE: Cadence = Cadence::Block;

    fn channels(&self) -> Channels {
        Channels {
            input: ChannelLayout::MONO,
            output: ChannelLayout::MONO,
        }
    }

    fn blank(&self, _: SampleRate) -> GainModel {
        GainModel { gain: 0.0 }
    }

    fn design_into(&self, g: &f32, _: SampleRate, out: &mut GainModel) {
        out.gain = if g.is_finite() {
            g.clamp(0.0, 0.9)
        } else {
            0.5
        };
    }

    fn bounds(&self, _: &Ceiling<()>) -> NoSizes {
        NoSizes
    }

    fn realization(&self) -> GainRealization {
        GainRealization::new()
    }
}

impl DesignCase for GainDesign {
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

impl RealizationCase for GainDesign {
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

impl LtiCase for GainDesign {
    /// A gain is its value at every frequency.
    fn anchors(&self) -> Vec<Anchor<f32>> {
        [0.1, 0.5, 0.9]
            .into_iter()
            .map(|g| Anchor {
                controls: g,
                rate: SampleRate(48_000.0),
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
    design_contract!(GainDesign, Lti);
}

mod realization {
    use super::*;
    realization_contract!(GainDesign, Lti);
}
