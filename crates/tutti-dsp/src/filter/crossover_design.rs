//! [`CrossoverDesign`]: the Linkwitz-Riley crossover as a [`Design`] (design 024
//! step 2b).
//!
//! The controls are [`CrossoverControls`] (the split frequencies), the model
//! [`CrossoverModel`] (a multi-output [`Lti`](crate::Lti): one output a band) and
//! the realization [`CrossoverRealization`]: the tree's sections, one set of words
//! per channel, run with the model's own coefficients.
//!
//! Like the SVF and the ladder it is designed on a 16-frame grid counted on the
//! stream clock from the last reset ([`CrossoverDesign::CADENCE`]), so a control is
//! in the render at most 16 frames after it was sampled and the render does not
//! depend on how audio is split into blocks. **Unlike them its coefficients do not
//! ramp over the segment**: a retarget lands whole and the segment runs on the set
//! it solved, stair by stair. That is what the crossover has always done (it held
//! its coefficients per 16-chunk), and the interpolation is better left out for
//! what the tree is: the bands sum to an all-pass only while each split's low-pass,
//! high-pass and all-pass share one frequency, which a coefficient-wise blend of the
//! three does not keep. The glide that keeps a moving split from clicking is the
//! node's, in the frequency (in `Hz`, linear across the block, as the old render
//! ramped them) before the model is solved from it; in theta (`tan(pi f / rate)`)
//! the same glide would be a different curve, and nothing asks for it.

use tutti_types::{ChannelLayout, Hz, SampleRate};

use super::CrossoverModel;
use super::crossover::check_bands;
use super::crossover::{CrossoverError, CrossoverState, MAX_SPLITS, SOLVE_CEILING, validate};
use super::sizes::FilterSizes;
use crate::control::COEFF_STREAM;
use crate::{Cadence, Ceiling, Channels, Design, Effect, Effort, Realization, Setup};

/// The frequencies of a crossover's splits, lowest first; the entries past the
/// design's last split are 0 and ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossoverControls {
    /// Split `i` divides band `i` from band `i + 1`.
    pub frequencies: [Hz; MAX_SPLITS],
}

impl CrossoverControls {
    /// The set `frequencies` at `rate`: one split each, so `frequencies.len() + 1`
    /// bands.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] as [`Crossover::new`](crate::Crossover::new): a band
    /// count outside 2 to 6, a rate that is not finite and positive, or
    /// frequencies that are not finite, above 0, strictly increasing and below
    /// Nyquist.
    pub fn new(frequencies: &[Hz], rate: SampleRate) -> Result<Self, CrossoverError> {
        validate(frequencies, rate)?;
        let mut set = [Hz(0.0); MAX_SPLITS];
        set[..frequencies.len()].copy_from_slice(frequencies);
        Ok(Self { frequencies: set })
    }
}

/// The [`Design`] of the Linkwitz-Riley crossover: `bands` bands over `channels`
/// channels, the input's width in and `bands * channels` out, band-major (output
/// `band * channels + channel`).
#[derive(Clone, Copy, Debug)]
pub struct CrossoverDesign {
    channels: ChannelLayout,
    bands: usize,
}

impl CrossoverDesign {
    /// A crossover of `bands` bands over `channels`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Bands`] outside 2 to 6, and [`CrossoverError::Channels`]
    /// for no channels.
    pub fn new(channels: ChannelLayout, bands: usize) -> Result<Self, CrossoverError> {
        check_bands(bands)?;
        match channels.count() {
            0 => Err(CrossoverError::Channels { channels: 0 }),
            _ => Ok(Self { channels, bands }),
        }
    }

    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// The input width; each band is this wide.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.channels
    }
}

impl Design for CrossoverDesign {
    type Controls = CrossoverControls;
    type Model = CrossoverModel;
    type Bounds = FilterSizes<CrossoverModel>;
    type Range = ();
    type Realization = CrossoverRealization;

    /// Solved every 16 frames on the stream clock (doc 024 Q27), at the first frame
    /// of each segment, and held over it.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        Channels {
            input: self.channels,
            output: ChannelLayout::from_count(self.channels.count() * self.bands as u16),
        }
    }

    /// A model at splits an octave apart from 0.2% of the rate up: increasing and
    /// under Nyquist at any rate.
    fn blank(&self, rate: SampleRate) -> CrossoverModel {
        let mut model = CrossoverModel::unsolved(self.bands, rate.get());
        let frequencies =
            core::array::from_fn(|i| Hz((rate.get() * 0.002 * (1 << i) as f64) as f32));
        self.design_into(&CrossoverControls { frequencies }, rate, &mut model);
        model
    }

    /// Total: a frequency is held between 1 Hz and what the coefficients can be
    /// solved at ([`SOLVE_CEILING`] of the rate), a NaN reading as the floor. A set
    /// that is not increasing is solved as it stands (each split is stable alone);
    /// [`CrossoverControls::new`] is what refuses one.
    fn design_into(
        &self,
        controls: &CrossoverControls,
        rate: SampleRate,
        out: &mut CrossoverModel,
    ) {
        out.rate = rate.get();
        out.bands = self.bands;
        let cap = SOLVE_CEILING * rate.get();
        for (f, hz) in out.freqs.iter_mut().zip(&controls.frequencies) {
            *f = f64::from(hz.get()).max(1.0).min(cap);
        }
        out.solve();
    }

    fn bounds(&self, _: &Ceiling<()>) -> FilterSizes<CrossoverModel> {
        FilterSizes::NONE
    }

    fn realization(&self) -> CrossoverRealization {
        CrossoverRealization {
            model: self.blank(SampleRate::DEFAULT),
            state: vec![CrossoverState::default(); usize::from(self.channels.count()).max(1)],
        }
    }
}

/// The realization of a [`CrossoverModel`]: the model's coefficients, held, and the
/// sections' words of each channel.
#[derive(Clone, Debug)]
pub struct CrossoverRealization {
    model: CrossoverModel,
    /// One tree per channel; `len()` is the input width. Built with the design and
    /// never resized (no allocation on the audio thread).
    state: Vec<CrossoverState>,
}

impl Realization<CrossoverModel> for CrossoverRealization {
    type Bounds = FilterSizes<CrossoverModel>;

    fn prepare(&mut self, _: &FilterSizes<CrossoverModel>, _: &Setup) {}

    /// Takes `model`'s coefficients whole, from the next frame.
    fn retarget(&mut self, model: &CrossoverModel) {
        self.model = *model;
    }

    fn reset(&mut self) {
        self.state.fill(CrossoverState::default());
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }

    fn fresh(&self) -> Self {
        let mut fresh = self.clone();
        fresh.reset();
        fresh
    }
}

impl Effect<CrossoverModel> for CrossoverRealization {
    /// Each channel through its tree, band `b` of channel `c` written to output
    /// `b * channels + c`.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let width = self.state.len();
        for (c, tree) in self.state.iter_mut().enumerate() {
            for (i, &x) in input[c].iter().enumerate() {
                let bands = tree.tick(&self.model, x);
                for (b, &y) in bands[..self.model.bands].iter().enumerate() {
                    output[b * width + c][i] = y;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::*;
    use crate::contract::{Anchor, DesignCase, LtiCase, RealizationCase};

    /// Mono, three bands: output `b` is band `b`, the model's own outputs.
    fn design() -> CrossoverDesign {
        CrossoverDesign::new(ChannelLayout::MONO, 3).unwrap()
    }

    fn controls(low: f32, high: f32) -> CrossoverControls {
        CrossoverControls {
            frequencies: [Hz(low), Hz(high), Hz(0.0), Hz(0.0), Hz(0.0)],
        }
    }

    impl DesignCase for CrossoverDesign {
        fn reachable(&self) -> Vec<CrossoverControls> {
            let mut all = Vec::new();
            for low in [20.0, 100.0, 400.0, 1_500.0] {
                for ratio in [1.2, 3.0, 20.0] {
                    all.push(controls(low, (low * ratio).min(20_000.0)));
                }
            }
            let mut rng = StdRng::seed_from_u64(24);
            all.extend((0..100).map(|_| {
                let low = rng.gen_range(20.0..8_000.0f32);
                controls(low, low * rng.gen_range(1.1..2.5))
            }));
            all
        }

        fn hostile(&self) -> Vec<CrossoverControls> {
            [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 0.0, 1e30]
                .into_iter()
                .flat_map(|v| [controls(v, 1_000.0), controls(200.0, v), controls(v, v)])
                .collect()
        }
    }

    /// The closed forms of Linkwitz and Riley, at 48 kHz: band 0 is `LP^2` of the
    /// lower split and all-pass after it, so it is 1 at DC and a half at that split
    /// (each branch of a split is -6.02 dB there); the top band is `HP^2` of both
    /// and is 1 at Nyquist.
    impl LtiCase for CrossoverDesign {
        fn anchors(&self) -> Vec<Anchor<CrossoverControls>> {
            let rate = SampleRate(48_000.0);
            let c = controls(500.0, 4_000.0);
            let at = |output, freq, gain| Anchor {
                controls: c,
                rate,
                output,
                freq: Hz(freq),
                gain,
                tolerance: 1e-9,
            };
            vec![at(0, 0.0, 1.0), at(0, 500.0, 0.5), at(2, 24_000.0, 1.0)]
        }

        /// The sections are squared, so every pole is double and the energy goes as
        /// `n^2 r^(2n)`: at the slowest splits the polynomial factor is a large part of
        /// the slope over the 4096-frame windows of `decay` (a 2 Hz split measures 0.68
        /// of the radius's rate), where a single pole would sit at 1. The case still
        /// catches a realization that rings at less than 0.6 of the bound.
        fn decay_tolerance(&self) -> f64 {
            0.4
        }

        /// The response's loudest window is the top band's pulse, and the slowest
        /// poles carry only a small part of it, so the time the response takes to fall
        /// to -60 dB of that pulse is shorter than the poles' ring: by 5 at the 2 Hz
        /// probe, which `decay` needs to see six windows of.
        fn tail_slack(&self) -> f64 {
            8.0
        }
    }

    impl RealizationCase for CrossoverDesign {
        fn probes(&self) -> Vec<CrossoverControls> {
            vec![
                controls(200.0, 2_000.0),
                controls(800.0, 3_000.0),
                controls(60.0, 9_000.0),
                // Slow enough for `decay` to fit a line over six windows of 4096
                // frames between -30 and -80 dB of its loudest.
                controls(2.0, 400.0),
            ]
        }

        fn extremes(&self) -> Vec<CrossoverControls> {
            vec![controls(30.0, 100.0), controls(8_000.0, 18_000.0)]
        }

        /// A step a grid point apart: the realization takes a set whole, so a jump is
        /// as large as the move between two solves, which a glide (the node's, in
        /// hertz) keeps to a few percent. A split that jumps by an octave between two
        /// solves is heard (the node's lanes can do it), and is not what these
        /// figures hold the realization to.
        fn steps(&self) -> Vec<(CrossoverControls, CrossoverControls)> {
            vec![
                (controls(300.0, 3_000.0), controls(315.0, 3_150.0)),
                (controls(200.0, 2_000.0), controls(200.0, 2_100.0)),
            ]
        }
    }

    crate::design_contract!(design(), Lti);

    // The `Lti` group on the realization: the crossover is linear and shift invariant
    // at a set of controls, and its model is multi-output (one a band), so the group's
    // cases read output `b` of the realization against output `b` of the model. The
    // design is mono so that the realization's output channel `b` is band `b`.
    // Solved on the stream's 16-frame grid, so the render does not depend on how audio
    // is split: `BlockIndependent`.
    crate::realization_contract!(design(), BlockIndependent, Lti);

    /// The first sample after a retarget is the new set's, whole: an impulse into a
    /// realization with no state answers with the product of the first coefficients of
    /// the band's sections (the low-pass twice, then the all-pass of the later split),
    /// from the model it was last retargeted to, and not a blend with the one before.
    ///
    /// Mutation (run): `retarget` ignoring the model (the body emptied) answers with
    /// the first set's figure and fails.
    #[test]
    fn a_retarget_lands_whole() {
        let d = design();
        let rate = SampleRate(48_000.0);
        let first = d.design(&controls(300.0, 3_000.0), rate);
        let second = d.design(&controls(900.0, 5_000.0), rate);
        let mut r = d.realization();
        r.retarget(&first);
        r.process(
            &[&[0.0; 40]],
            &mut [&mut [0.0; 40], &mut [0.0; 40], &mut [0.0; 40]],
        );
        r.retarget(&second);
        let mut out = [[0.0f32; 1]; 3];
        let [a, b, c] = &mut out;
        r.process(&[&[1.0]], &mut [a, b, c]);

        let (low, all_pass) = (second.coeffs[0].low.b0, second.coeffs[1].all_pass.b0);
        assert_eq!(out[0][0], (low * low * all_pass) as f32);
    }

    /// `CrossoverControls::new` is `Crossover::new`'s own check: it refuses what a
    /// crossover cannot run, and nothing else.
    ///
    /// Mutation (run): dropping the `validate` call accepts the disordered set.
    #[test]
    fn controls_refuse_what_a_crossover_cannot_run() {
        let rate = SampleRate(48_000.0);
        assert!(CrossoverControls::new(&[Hz(100.0), Hz(1_000.0)], rate).is_ok());
        assert!(CrossoverControls::new(&[Hz(1_000.0), Hz(100.0)], rate).is_err());
        assert!(CrossoverControls::new(&[Hz(f32::NAN)], rate).is_err());
        assert!(CrossoverControls::new(&[Hz(30_000.0)], rate).is_err());
        assert!(CrossoverControls::new(&[], rate).is_err());
    }

    /// The design refuses a band count the tree does not have and an empty layout.
    ///
    /// Mutation (run): dropping the `check_bands` call builds a one-band crossover.
    #[test]
    fn the_design_refuses_what_it_cannot_make() {
        assert!(CrossoverDesign::new(ChannelLayout::MONO, 1).is_err());
        assert!(CrossoverDesign::new(ChannelLayout::MONO, 7).is_err());
        assert!(CrossoverDesign::new(ChannelLayout::from_count(0), 2).is_err());
        let wide = CrossoverDesign::new(ChannelLayout::STEREO, 4).unwrap();
        assert_eq!(wide.channels().output, ChannelLayout::from_count(8));
    }
}
