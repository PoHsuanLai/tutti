//! [`CrossoverDesign`]: the Linkwitz-Riley crossover as a [`Design`](crate::Design) (design 024
//! step 2b).
//!
//! The controls are [`CrossoverControls`] (the split frequencies), the model
//! [`CrossoverModel`] (a multi-output [`Lti`](crate::Lti): one output a band) and
//! the kernel [`CrossoverTree`]: the tree's sections, one set of words per channel, run
//! with the model's own coefficients.
//!
//! Like the SVF and the ladder it is designed on a 16-frame grid counted on the
//! stream clock from the last reset ([`Design::CADENCE`](crate::Design::CADENCE)), so a control is
//! in the render at most 16 frames after it was sampled and the render does not
//! depend on how audio is split into blocks. **Unlike them its coefficients do not
//! ramp over the segment**: a retarget lands whole (`STEPS = 0`) and the segment runs on the set
//! it solved, stair by stair. That is what the crossover has always done (it held
//! its coefficients per 16-chunk), and the interpolation is better left out for
//! what the tree is: the bands sum to an all-pass only while each split's low-pass,
//! high-pass and all-pass share one frequency, which a coefficient-wise blend of the
//! three does not keep. The glide that keeps a moving split from clicking is the
//! node's, in the frequency (in `Hz`, linear across the block, as the old render
//! ramped them) before the model is solved from it; in theta (`tan(pi f / rate)`)
//! the same glide would be a different curve, and nothing asks for it.

use tutti_types::{BusTopology, Hz, SampleRate};

use super::crossover::check_bands;
use super::crossover::{CrossoverError, CrossoverState, MAX_SPLITS, SOLVE_CEILING, validate};
use super::{ChannelKernel, Crossover, CrossoverModel, Filter};
use crate::Lerp;

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

/// The [`Design`](crate::Design) of the Linkwitz-Riley crossover: `bands` bands over
/// `channels` channels, the input's width in and `bands * channels` out, band-major
/// (output `band * channels + channel`).
pub type CrossoverDesign = Filter<CrossoverTree>;

/// The crossover as a [`ChannelKernel`]: a tree of `bands` bands per channel, run with
/// the model's own coefficients.
#[derive(Clone, Copy, Debug)]
pub struct CrossoverTree {
    bands: usize,
}

impl CrossoverDesign {
    /// A crossover of `bands` bands on the bus `input`, each band where it is.
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Bands`] outside 2 to 6, and [`CrossoverError::Channels`]
    /// for no channels.
    pub fn new(input: BusTopology, bands: usize) -> Result<Self, CrossoverError> {
        check_bands(bands)?;
        match input.layout().count() {
            0 => Err(CrossoverError::Channels { channels: 0 }),
            _ => Ok(Self::over(CrossoverTree { bands }, input)),
        }
    }

    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.kernel().bands()
    }
}

impl CrossoverTree {
    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }
}

/// A crossover's coefficients do not blend: its glide has no steps ([`ChannelKernel::STEPS`]),
/// so this is only ever the target.
impl Lerp for CrossoverModel {
    fn lerp(&self, to: &Self, _: u32, _: u32) -> Self {
        *to
    }
}

impl ChannelKernel for CrossoverTree {
    type Controls = CrossoverControls;
    type Model = CrossoverModel;
    type Coeffs = CrossoverModel;
    type State = CrossoverState;
    type Frame = [f32; Crossover::MAX_BANDS];

    const STEPS: u32 = 0;

    fn outputs(&self) -> usize {
        self.bands
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

    /// Total: a frequency is held between the split below it (1 Hz for the first)
    /// and what the coefficients can be solved at ([`SOLVE_CEILING`] of the rate), a
    /// NaN reading as the floor, so a set that is not increasing is solved with its
    /// splits no lower than the one before. [`CrossoverControls::new`] is what
    /// refuses one.
    fn design_into(
        &self,
        controls: &CrossoverControls,
        rate: SampleRate,
        out: &mut CrossoverModel,
    ) {
        out.rate = rate.get();
        out.bands = self.bands;
        let cap = SOLVE_CEILING * rate.get();
        let mut floor = 1.0;
        for (f, hz) in out.freqs.iter_mut().zip(&controls.frequencies) {
            floor = f64::from(hz.get()).max(floor).min(cap);
            *f = floor;
        }
        out.solve();
    }

    fn coeffs(&self, model: &CrossoverModel) -> CrossoverModel {
        *model
    }

    #[inline]
    fn tick(c: &CrossoverModel, s: &mut CrossoverState, x: f32) -> [f32; Crossover::MAX_BANDS] {
        s.tick(c, x)
    }
}

#[cfg(test)]
mod tests;
