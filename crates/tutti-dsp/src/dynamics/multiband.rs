//! [`MultibandFamily`]: the multiband compressor as a [`Family`].
//!
//! A Linkwitz-Riley crossover (the tree of [`CrossoverFamily`], one
//! per channel) splits the signal into 2 to 4 bands; each band is compressed by its own
//! compressor (the frame step of [`CompressorFamily`](super::CompressorFamily), one
//! copy of it) with a gain **linked** across channels, from the loudest channel of the
//! band; and the bands are summed. The detector is the band itself: there is no
//! side-chain.
//!
//! A band may be **bypassed** (its gain goes to unity, the makeup with it, while its
//! compressor keeps running so a release finds it warm) and **soloed** (while any band is
//! soloed the output is the sum of the soloed bands, the others muted but still
//! metering). Both are weights of 0 or 1 that glide over [`GAIN_GLIDE`], like a makeup: a
//! step in either multiplies the output directly, so it would click.
//!
//! The crossover lands whole on the stream grid ([`Family::CADENCE`]), as
//! [`CrossoverFamily`]'s does, and so do the detectors; only the
//! makeups and the two weights glide. Each band's threshold is a per-frame lane of the
//! [`Feed`] (lane `b` is band `b`'s).
//!
//! The deepest reduction each band reached since the last read is in the state
//! ([`BandReductions`], the recurrence's [`Readout`]): a reading for a meter, which the node publishes.

use core::ops::Range;

use tutti_types::{BusTopology, Db, Hz, SampleRate, Samples};

use super::GAIN_GLIDE;
use super::compressor::{CompressorControls, CompressorMember, Detector};
use crate::control::COEFF_STREAM;
use crate::filter::CrossoverState;
use crate::{
    Approach, Cadence, ChannelRecurrence, Channels, CrossoverControls, CrossoverError,
    CrossoverFamily, CrossoverModel, CrossoverTree, Effort, Family, Feed, Glide, Readout,
    Recurrence, Ring, Setup, Start, Steps, System,
};

/// The most bands.
const MAX_BANDS: usize = 4;

/// The bands a crossover tree makes at the most, the width of one frame's split.
const TREE_BANDS: usize = CrossoverFamily::MAX_BANDS;

/// The controls of one band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandSettings {
    /// The band's compressor.
    pub compressor: CompressorControls,
    /// Passes the band unreduced and without makeup.
    pub bypass: bool,
    /// Solos the band: while any band is soloed, only the soloed ones sound.
    pub solo: bool,
}

impl BandSettings {
    /// A band that does nothing: [`CompressorControls::DEFAULT`], neither bypassed nor
    /// soloed.
    pub const DEFAULT: Self = Self {
        compressor: CompressorControls::DEFAULT,
        bypass: false,
        solo: false,
    };
}

impl Default for BandSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls of a [`MultibandFamily`]: the split frequencies and a [`BandSettings`]
/// each band. Entries past the family's bands (and past its splits) are ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultibandSettings {
    /// The crossover's split frequencies, lowest first.
    pub crossover: CrossoverControls,
    /// The bands, lowest first.
    pub bands: [BandSettings; MAX_BANDS],
}

impl MultibandSettings {
    /// A multiband that does nothing but split and sum: every band at
    /// [`BandSettings::DEFAULT`], the splits at 200 Hz, 1 kHz and 5 kHz.
    pub const DEFAULT: Self = Self {
        crossover: CrossoverControls {
            frequencies: [Hz(200.0), Hz(1_000.0), Hz(5_000.0), Hz(0.0), Hz(0.0)],
        },
        bands: [BandSettings::DEFAULT; MAX_BANDS],
    };
}

impl Default for MultibandSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A multiband compressor at one set of controls and one rate: the crossover's design, a
/// [`CompressorMember`] a band, and which bands are bypassed and soloed. No latency (the
/// bands are not delayed) and a tail of the crossover's ring: the splits' sections ring
/// after the input stops, and the compressors only scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultibandMember {
    crossover: CrossoverModel,
    bands: [CompressorMember; MAX_BANDS],
    count: usize,
    bypass: [bool; MAX_BANDS],
    solo: [bool; MAX_BANDS],
}

impl MultibandMember {
    /// How many bands it has.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.count
    }

    /// The crossover's design.
    #[must_use]
    pub fn crossover(&self) -> &CrossoverModel {
        &self.crossover
    }

    /// Band `band`'s compressor, or `None` past the last band.
    #[must_use]
    fn compressor(&self, band: usize) -> Option<&CompressorMember> {
        self.bands[..self.count].get(band)
    }

    /// Whether band `band` is heard: nothing is soloed, or it is. `false` past the last
    /// band.
    #[must_use]
    pub fn is_audible(&self, band: usize) -> bool {
        band < self.count && (!self.any_solo() || self.solo[band])
    }

    /// The reduction, in dB and positive, that a steady `level` in band `band` asks
    /// for (the compressor's static curve; a bypassed band still asks for it, and
    /// passes none of it). `None` past the last band.
    #[must_use]
    pub fn reduction_db(&self, band: usize, level: Db) -> Option<Db> {
        self.compressor(band).map(|c| c.reduction_db(level))
    }

    fn any_solo(&self) -> bool {
        self.solo[..self.count].iter().any(|&s| s)
    }
}

impl System for MultibandMember {
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn tail(&self, floor: Db) -> Ring {
        self.crossover.tail(floor)
    }
}

/// The [`Family`] of the multiband compressor: `bands` bands (2 to 4) over `input`, one
/// output bus where the input is.
#[derive(Clone, Copy, Debug)]
pub struct MultibandFamily {
    input: BusTopology,
    tree: CrossoverTree,
}

impl MultibandFamily {
    /// The fewest bands.
    pub const MIN_BANDS: usize = 2;
    /// The most bands.
    pub const MAX_BANDS: usize = MAX_BANDS;

    /// `bands` bands on the bus `input`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Bands`] if `bands` is not 2 to 4 and
    /// [`CrossoverError::Channels`] for no channels.
    pub fn new(input: BusTopology, bands: usize) -> Result<Self, CrossoverError> {
        if !(Self::MIN_BANDS..=Self::MAX_BANDS).contains(&bands) {
            return Err(CrossoverError::Bands {
                bands,
                min: Self::MIN_BANDS,
                max: Self::MAX_BANDS,
            });
        }
        if input.layout().count() == 0 {
            return Err(CrossoverError::Channels { channels: 0 });
        }
        Ok(Self {
            input,
            tree: CrossoverTree::new(bands)?,
        })
    }

    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.tree.bands()
    }
}

impl Family for MultibandFamily {
    type Controls = MultibandSettings;
    type Member = MultibandMember;
    type Recurrence = MultibandRecurrence;

    /// Solved on the stream's grid of 16 frames, as the crossover's and the compressor's.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        Channels::same(self.input)
    }

    fn blank(&self, rate: SampleRate) -> MultibandMember {
        MultibandMember {
            crossover: self.tree.blank(rate),
            bands: [CompressorMember::designed(&CompressorControls::DEFAULT, rate); MAX_BANDS],
            count: self.bands(),
            bypass: [false; MAX_BANDS],
            solo: [false; MAX_BANDS],
        }
    }

    /// The controls, made runnable: the crossover as [`CrossoverTree`] makes it (total: a
    /// set out of order is solved with its splits no lower than the one before), and each
    /// band as [`CompressorFamily`](super::CompressorFamily) does.
    fn member_into(&self, c: &MultibandSettings, rate: SampleRate, out: &mut MultibandMember) {
        self.tree
            .member_into(&c.crossover, rate, &mut out.crossover);
        out.count = self.bands();
        for (b, band) in c.bands.iter().enumerate() {
            out.bands[b] = CompressorMember::designed(&band.compressor, rate);
            out.bypass[b] = band.bypass;
            out.solo[b] = band.solo;
        }
    }

    fn recurrence(&self, _: SampleRate) -> MultibandRecurrence {
        MultibandRecurrence {
            width: usize::from(self.input.layout().count()),
            bands: self.bands(),
        }
    }
}

/// The coefficient point of a multiband: the crossover and the detectors, which land
/// whole, and the figures that glide, each band's makeup (dB) and its two weights.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultibandTarget {
    crossover: CrossoverModel,
    count: usize,
    detectors: [Detector; MAX_BANDS],
    makeup: [f32; MAX_BANDS],
    /// 1 while the band is bypassed, else 0.
    bypass: [f32; MAX_BANDS],
    /// 1 while the band is heard, else 0.
    audible: [f32; MAX_BANDS],
}

/// The coefficients in flight: the crossover's and the detectors', landed, and each band's
/// makeup, bypass weight and audible weight gliding over [`GAIN_GLIDE`].
///
/// As for the compressor, a generic glide of the whole target would run for every frame of
/// the glide when only a ratio moved: only what multiplies the output unsmoothed takes
/// steps. The crossover cannot glide at all (a blend of a split's sections is not one).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultibandCoeffs {
    crossover: CrossoverModel,
    count: usize,
    detectors: [Detector; MAX_BANDS],
    makeup: [Glide<f32>; MAX_BANDS],
    bypass: [Glide<f32>; MAX_BANDS],
    audible: [Glide<f32>; MAX_BANDS],
    /// The steps a glide takes: [`GAIN_GLIDE`] at the prepared rate.
    steps: u32,
}

impl Approach for MultibandCoeffs {
    type Target = MultibandTarget;

    fn aim(&mut self, t: &MultibandTarget) {
        self.crossover = t.crossover;
        self.count = t.count;
        self.detectors = t.detectors;
        let steps = Steps::Fixed(self.steps);
        for b in 0..MAX_BANDS {
            self.makeup[b].aim_by(t.makeup[b], steps);
            self.bypass[b].aim_by(t.bypass[b], steps);
            self.audible[b].aim_by(t.audible[b], steps);
        }
    }

    #[inline]
    fn step(&mut self) -> bool {
        let mut moved = false;
        for b in 0..MAX_BANDS {
            moved |= self.makeup[b].step();
            moved |= self.bypass[b].step();
            moved |= self.audible[b].step();
        }
        moved
    }

    fn settle(&mut self) {
        for b in 0..MAX_BANDS {
            self.makeup[b].settle();
            self.bypass[b].settle();
            self.audible[b].settle();
        }
    }
}

/// What a block changes: each channel's crossover sections, each band's follower and the
/// deepest reduction each band reached since the last [`Readout`] read, and the
/// frame's scratch (one split and one sum per channel, sized to the width with the
/// recurrence).
#[derive(Clone, Debug)]
pub struct MultibandState {
    filters: Vec<CrossoverState>,
    split: Vec<[f32; TREE_BANDS]>,
    sum: Vec<f32>,
    /// Each band's smoothed reduction, in dB and positive.
    reductions: [f32; MAX_BANDS],
    /// Each band's largest reduction since the last read, in dB.
    deepest: [f32; MAX_BANDS],
}

/// Each band's largest reduction since the last read, in dB and positive: a
/// **measurement**, for a reduction meter, and the multiband's [`Readout`]. It counts only
/// the frames a band is not fully bypassed on (a bypassed band passes none of it). It
/// carries the family's band count, so a reader cannot ask about a band that is not there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandReductions {
    deepest: [Db; MAX_BANDS],
    bands: usize,
}

impl BandReductions {
    /// Band `band`'s deepest reduction ([`Db::UNITY`], 0 dB, is none), or `None` past the
    /// last band.
    #[must_use]
    pub fn band(&self, band: usize) -> Option<Db> {
        self.deepest[..self.bands].get(band).copied()
    }

    /// Every band's deepest reduction, in band order: exactly the family's bands.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Db> + '_ {
        self.deepest[..self.bands].iter().copied()
    }
}

impl Readout for MultibandRecurrence {
    type Reading = BandReductions;

    fn take_reading(&self, state: &mut MultibandState) -> BandReductions {
        BandReductions {
            deepest: core::mem::take(&mut state.deepest).map(Db),
            bands: self.bands,
        }
    }
}

/// The [`Recurrence`] of a [`MultibandFamily`].
///
/// K, per frame: each channel through its crossover tree; then, per band in order, the
/// band's level (the loudest channel of the band, a NaN reading as quiet), its compressor
/// (the frame step of [`CompressorRecurrence`](super::CompressorRecurrence), at the
/// threshold of lane `b` of the [`Feed`] when it is finite, else the member's), the bypass
/// blend `g (1 - w) + w` (skipped at `w = 0`), the audible multiply (skipped at 1, and the
/// band left out of the sum at 0) and each channel's `x g` added to its sum. The bands
/// are summed lowest first, from 0.
///
/// **Lane `b` is band `b`'s threshold in dB, per frame.**
#[derive(Clone, Copy, Debug)]
pub struct MultibandRecurrence {
    width: usize,
    /// The family's band count, which a [`BandReductions`] reading carries.
    bands: usize,
}

impl Recurrence for MultibandRecurrence {
    type Member = MultibandMember;
    type Capacity = ();
    type Target = MultibandTarget;
    type Coeffs = MultibandCoeffs;
    type State = MultibandState;

    const STEP: usize = 1;

    fn start(&self, member: &MultibandMember) -> Start<Self> {
        let mut target = MultibandTarget {
            crossover: member.crossover,
            count: member.count,
            detectors: [Detector::NEUTRAL; MAX_BANDS],
            makeup: [0.0; MAX_BANDS],
            bypass: [0.0; MAX_BANDS],
            audible: [1.0; MAX_BANDS],
        };
        self.coefficients_into(member, &mut target);
        Start {
            coeffs: MultibandCoeffs {
                crossover: target.crossover,
                count: target.count,
                detectors: target.detectors,
                makeup: target.makeup.map(Glide::landed),
                bypass: target.bypass.map(Glide::landed),
                audible: target.audible.map(Glide::landed),
                steps: 0,
            },
            state: MultibandState {
                filters: vec![CrossoverState::default(); self.width],
                split: vec![[0.0; TREE_BANDS]; self.width],
                sum: vec![0.0; self.width],
                reductions: [0.0; MAX_BANDS],
                deepest: [0.0; MAX_BANDS],
            },
            target,
        }
    }

    fn prepare(
        &mut self,
        _: &(),
        setup: &Setup,
        coeffs: &mut MultibandCoeffs,
        _: &mut MultibandState,
        _: &mut MultibandTarget,
    ) {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates
        // rather than panic, since a prepare must not.
        coeffs.steps =
            u32::try_from(GAIN_GLIDE.to_samples_ceil(setup.rate).get()).unwrap_or(u32::MAX);
    }

    fn coefficients_into(&self, m: &MultibandMember, target: &mut MultibandTarget) -> bool {
        target.crossover = m.crossover;
        target.count = m.count;
        for b in 0..MAX_BANDS {
            target.detectors[b] = Detector::of(&m.bands[b]);
            target.makeup[b] = m.bands[b].makeup().get();
            target.bypass[b] = f32::from(m.bypass[b]);
            target.audible[b] = f32::from(m.is_audible(b) || b >= m.count);
        }
        true
    }

    fn clear(&self, state: &mut MultibandState) {
        state.filters.fill(CrossoverState::default());
        state.reductions = [0.0; MAX_BANDS];
        state.deepest = [0.0; MAX_BANDS];
    }

    fn run(
        &self,
        coeffs: &MultibandCoeffs,
        state: &mut MultibandState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let bands = coeffs.count;
        let MultibandState {
            filters,
            split,
            sum,
            reductions,
            deepest,
        } = state;
        for i in frames {
            for ((filter, bands), input) in filters.iter_mut().zip(split.iter_mut()).zip(feed.audio)
            {
                *bands = filter.tick(&coeffs.crossover, input[i]);
            }
            sum.fill(0.0);
            for b in 0..bands {
                let level = split.iter().fold(0.0f32, |m, ch| m.max(ch[b].abs()));
                let d = &coeffs.detectors[b];
                let threshold = feed.lanes[b]
                    .map(|lane| Db(lane[i]))
                    .filter(|t| t.get().is_finite())
                    .unwrap_or_else(|| d.threshold());
                let mut g = d.frame(&mut reductions[b], level, threshold, coeffs.makeup[b].now());
                let w = coeffs.bypass[b].now();
                if w != 1.0 {
                    deepest[b] = deepest[b].max(reductions[b]);
                }
                if w != 0.0 {
                    // Blend toward unity (makeup included) as the bypass takes hold.
                    g = g * (1.0 - w) + w;
                }
                let a = coeffs.audible[b].now();
                if a == 0.0 {
                    continue;
                }
                if a != 1.0 {
                    g *= a;
                }
                for (s, ch) in sum.iter_mut().zip(split.iter()) {
                    *s += ch[b] * g;
                }
            }
            for (out, &s) in output.iter_mut().zip(sum.iter()) {
                out[i] = s;
            }
        }
    }

    fn effort(&self) -> Effort {
        Effort::Heavy
    }
}

#[cfg(test)]
mod tests;
