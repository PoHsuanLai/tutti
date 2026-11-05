//! [`MultibandCompressorNode`]: a compressor per band of a Linkwitz-Riley
//! crossover, summed.

use core::sync::atomic::{AtomicBool, Ordering};
use std::fmt;

use tutti_core::{Arc, AtomicF32, CompressionRatio, Db, Hz, Param, SampleRate, Seconds, Tail};
use tutti_graph::{Cost, Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::ChannelLayout;

use super::compressor::CompressorCore;
use super::params::AttackRelease;
use crate::filter::{Crossover, CrossoverError};
use crate::fresh::{Fresh, fresh_parts};
use crate::ramp::LastGood;

/// Crossover frequencies a band count starts with, in Hz.
fn default_crossovers(bands: usize) -> &'static [f32] {
    match bands {
        2 => &[1_000.0],
        3 => &[200.0, 3_000.0],
        _ => &[120.0, 1_000.0, 6_000.0],
    }
}

/// A band's settings at construction.
const DEFAULT_THRESHOLD: Db = Db(-18.0);
const DEFAULT_RATIO: f32 = 2.0;
const DEFAULT_ATTACK: Seconds = Seconds(0.010);
const DEFAULT_RELEASE: Seconds = Seconds(0.100);

/// The handles on one band's settings. Clones share the cells.
#[derive(Clone, Debug)]
pub struct BandControls {
    threshold: Param<Db>,
    knee: Param<Db>,
    ratio: Param<CompressionRatio>,
    timing: AttackRelease,
    makeup: Param<Db>,
    bypass: Arc<AtomicBool>,
    solo: Arc<AtomicBool>,
    reduction: Arc<AtomicF32>,
}

impl BandControls {
    /// Sets the threshold in [`Db`], unclamped.
    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.threshold.store(db.into());
    }

    /// Sets the knee width in [`Db`], floored at 0 (a hard knee).
    pub fn set_knee(&self, db: impl Into<Db>) {
        self.knee.store(Db(db.into().get().max(0.0)));
    }

    /// Sets the [`CompressionRatio`], clamped to at least `1.0` (no
    /// expansion). `1.0` leaves the band alone.
    pub fn set_ratio(&self, ratio: impl Into<CompressionRatio>) {
        self.ratio
            .store(CompressionRatio::new_clamped(ratio.into().get()));
    }

    /// Sets the attack time in [`Seconds`], floored at 0.
    pub fn set_attack(&self, seconds: impl Into<Seconds>) {
        self.timing
            .attack
            .store(Seconds(AttackRelease::clamp_time(seconds.into().get())));
    }

    /// Sets the release time in [`Seconds`], floored at 0.
    pub fn set_release(&self, seconds: impl Into<Seconds>) {
        self.timing
            .release
            .store(Seconds(AttackRelease::clamp_time(seconds.into().get())));
    }

    /// Sets the makeup gain in [`Db`], applied after reduction.
    pub fn set_makeup(&self, db: impl Into<Db>) {
        self.makeup.store(db.into());
    }

    /// Bypasses the band's compressor: the band passes unreduced and without
    /// makeup, and still sums into the output. Its detector is held cleared.
    pub fn set_bypass(&self, on: bool) {
        self.bypass.store(on, Ordering::Release);
    }

    /// Solos the band. While any band is soloed the output is the sum of the
    /// soloed bands only; the others are muted (ramped across a block) but
    /// keep metering.
    pub fn set_solo(&self, on: bool) {
        self.solo.store(on, Ordering::Release);
    }

    /// The threshold in [`Db`].
    #[must_use]
    pub fn threshold(&self) -> Db {
        self.threshold.load()
    }

    /// The knee width in [`Db`].
    #[must_use]
    pub fn knee(&self) -> Db {
        self.knee.load()
    }

    /// The [`CompressionRatio`].
    #[must_use]
    pub fn ratio(&self) -> CompressionRatio {
        self.ratio.load()
    }

    /// The attack time in [`Seconds`].
    #[must_use]
    pub fn attack(&self) -> Seconds {
        self.timing.attack.load()
    }

    /// The release time in [`Seconds`].
    #[must_use]
    pub fn release(&self) -> Seconds {
        self.timing.release.load()
    }

    /// The makeup gain in [`Db`].
    #[must_use]
    pub fn makeup(&self) -> Db {
        self.makeup.load()
    }

    /// Whether the band is bypassed.
    #[must_use]
    pub fn bypassed(&self) -> bool {
        self.bypass.load(Ordering::Acquire)
    }

    /// Whether the band is soloed.
    #[must_use]
    pub fn soloed(&self) -> bool {
        self.solo.load(Ordering::Acquire)
    }

    /// The gain reduction the band applied over the last block, in [`Db`]: a
    /// **measurement**, for a reduction meter. Larger is more reduction;
    /// [`Db::UNITY`] is none (also while the band is bypassed).
    ///
    /// The node writes it once per block with a relaxed store, so reading it
    /// from any thread is wait-free.
    #[must_use]
    pub fn gain_reduction_db(&self) -> Db {
        Db(self.reduction.load(Ordering::Relaxed))
    }
}

/// The handles on a [`MultibandCompressorNode`]: its bands' settings and its
/// crossover frequencies. Clones share the cells.
#[derive(Clone, Debug)]
pub struct MultibandControls {
    bands: Vec<BandControls>,
    crossovers: Vec<Param<Hz>>,
}

impl MultibandControls {
    /// How many bands there are.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands.len()
    }

    /// Band `i`'s settings (band 0 is the lowest), or `None` past the last.
    #[must_use]
    pub fn band(&self, i: usize) -> Option<&BandControls> {
        self.bands.get(i)
    }

    /// Crossover `i`'s frequency (between band `i` and band `i + 1`), or
    /// `None` past the last.
    #[must_use]
    pub fn crossover(&self, i: usize) -> Option<Hz> {
        self.crossovers.get(i).map(Param::load)
    }

    /// Moves crossover `i` to `hz`; the node ramps to it across its next
    /// block.
    ///
    /// Checked against the neighbouring crossovers now, and against Nyquist
    /// by the node (a set it cannot use reads as unchanged).
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Index`] past the last crossover,
    /// [`CrossoverError::Frequency`] if `hz` is not finite and above 0,
    /// [`CrossoverError::Order`] if it is not between its neighbours.
    /// Nothing is written then.
    pub fn set_crossover(&self, i: usize, hz: Hz) -> Result<(), CrossoverError> {
        let cell = self.crossovers.get(i).ok_or(CrossoverError::Index {
            index: i,
            len: self.crossovers.len(),
        })?;
        crate::filter::check_crossover_neighbours(
            i,
            hz,
            |k| self.crossovers[k].load(),
            self.crossovers.len(),
        )?;
        cell.store(hz);
        Ok(())
    }
}

/// One band's compressor and its running state.
#[derive(Clone, Debug)]
struct Band {
    core: CompressorCore,
    bypass: Arc<AtomicBool>,
    solo: Arc<AtomicBool>,
    reduction: Arc<AtomicF32>,
    /// The audibility (1 sounding, 0 muted by another band's solo) the
    /// previous block ended on; `None` until the first block.
    audible: Option<f32>,
}

/// A multiband compressor: a Linkwitz-Riley 4th-order crossover splits the
/// input into 2 to 4 bands, each band is compressed on its own, and the
/// bands are summed.
///
/// `channels` in, `channels` out. Detection in a band is **linked** across
/// channels (one reduction from the loudest channel of the band, applied to
/// all of them), as [`CompressorNode`](crate::CompressorNode)'s is. The
/// detector is the band itself: there is no external sidechain.
///
/// # Crossover and phase
///
/// The split is IIR and **not linear-phase**: the bands have different phase
/// responses, and only their sum is allpass (flat magnitude, phase rotating
/// through each crossover). With every band at unity ratio and 0 dB makeup the
/// output has the input's magnitude at every frequency. The node adds no
/// latency.
///
/// # Controls
///
/// Per-band threshold, knee, ratio, attack, release, makeup, bypass and
/// solo, and the crossover frequencies, through [`MultibandControls`]
/// ([`controls`](Self::controls), or what insertion hands back). They are
/// read once per block; the makeup, the crossover frequencies and a solo's
/// mute are ramped across it. A non-finite write reads as unchanged.
///
/// The graph's param ports address a parameter by a [`UnitParam`] alone, with
/// no band index, so this node declares none: its per-band settings are
/// reached through [`MultibandControls`].
///
/// [`UnitParam`]: tutti_types::UnitParam
pub struct MultibandCompressorNode {
    layout: ChannelLayout,
    bands: Vec<Band>,
    crossovers: Vec<Param<Hz>>,
    good: Vec<LastGood>,
    filters: Vec<Crossover>,
    /// Every channel's bands as [`Crossover::split_block`] writes them,
    /// `max_block * bands` per channel.
    split: Vec<f32>,
    /// The per-frame level, then gain, of the band being run.
    gains: Vec<f32>,
    max_block: usize,
}

impl fmt::Debug for MultibandCompressorNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultibandCompressorNode")
            .field("layout", &self.layout)
            .field("bands", &self.bands.len())
            .finish_non_exhaustive()
    }
}

impl MultibandCompressorNode {
    /// The fewest bands.
    pub const MIN_BANDS: usize = 2;
    /// The most bands.
    pub const MAX_BANDS: usize = 4;

    /// A compressor of `bands` bands over `layout`.
    ///
    /// The crossovers start at 1 kHz (2 bands), 200 Hz and 3 kHz (3), or
    /// 120 Hz, 1 kHz and 6 kHz (4). Each band starts at a -18 dB threshold,
    /// 2:1, 10 ms attack, 100 ms release, hard knee, 0 dB makeup, neither
    /// bypassed nor soloed.
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Bands`] if `bands` is not 2 to 4;
    /// [`CrossoverError::Channels`] if `layout` has no channels or more than
    /// the graph has ports for.
    pub fn new(layout: ChannelLayout, bands: usize) -> Result<Self, CrossoverError> {
        if !(Self::MIN_BANDS..=Self::MAX_BANDS).contains(&bands) {
            return Err(CrossoverError::Bands {
                bands,
                min: Self::MIN_BANDS,
                max: Self::MAX_BANDS,
            });
        }
        let channels = usize::from(layout.count());
        if channels == 0 || channels > tutti_graph::MAX_PORTS {
            return Err(CrossoverError::Channels { channels });
        }
        let defaults = default_crossovers(bands);
        let hz: Vec<Hz> = defaults.iter().map(|&f| Hz(f)).collect();
        let proto = Crossover::new(&hz, SampleRate::DEFAULT)?;
        Ok(Self {
            layout,
            bands: (0..bands)
                .map(|_| Band {
                    core: CompressorCore::new(
                        DEFAULT_THRESHOLD,
                        CompressionRatio(DEFAULT_RATIO),
                        DEFAULT_ATTACK,
                        DEFAULT_RELEASE,
                    ),
                    bypass: Arc::new(AtomicBool::new(false)),
                    solo: Arc::new(AtomicBool::new(false)),
                    reduction: Arc::new(AtomicF32::new(0.0)),
                    audible: None,
                })
                .collect(),
            crossovers: hz.iter().map(|&f| Param::new(f)).collect(),
            good: defaults.iter().map(|&f| LastGood::new(f)).collect(),
            filters: vec![proto; channels],
            split: Vec::new(),
            gains: Vec::new(),
            max_block: 0,
        })
    }

    /// The handles on the bands' settings and the crossovers.
    #[must_use]
    pub fn controls(&self) -> MultibandControls {
        MultibandControls {
            bands: self
                .bands
                .iter()
                .map(|b| BandControls {
                    threshold: b.core.threshold.threshold.handle(),
                    knee: b.core.threshold.knee.handle(),
                    ratio: b.core.ratio.handle(),
                    timing: b.core.timing.clone(),
                    makeup: b.core.makeup_db.handle(),
                    bypass: Arc::clone(&b.bypass),
                    solo: Arc::clone(&b.solo),
                    reduction: Arc::clone(&b.reduction),
                })
                .collect(),
            crossovers: self.crossovers.clone(),
        }
    }

    /// How many bands there are.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands.len()
    }

    /// The channel layout, the width of both input and output.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Band `band`'s gain reduction as of the last block, in [`Db`] (see
    /// [`BandControls::gain_reduction_db`]); [`Db::UNITY`] past the last
    /// band.
    #[must_use]
    pub fn gain_reduction_db(&self, band: usize) -> Db {
        self.bands
            .get(band)
            .map_or(Db::UNITY, |b| Db(b.reduction.load(Ordering::Relaxed)))
    }
}

impl Node for MultibandCompressorNode {
    /// `channels` in, `channels` out; zero latency; no param ports.
    ///
    /// Heavy (the default cost): a split into up to 4 bands and a compressor
    /// for each, per channel. The tail is [`Tail::Unknown`], as the other IIR
    /// filters': the crossover rings after the input stops.
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, self.layout)
            .with_cost(Cost::Heavy)
            .with_tail(Tail::Unknown)
    }

    fn prepare(&mut self, p: &Prepare) {
        for f in &mut self.filters {
            f.set_sample_rate(p.sample_rate());
        }
        for b in &mut self.bands {
            b.core.set_sample_rate(p.sample_rate());
        }
        self.max_block = p.max_block().get();
        self.gains = vec![0.0; self.max_block];
        self.split = vec![0.0; self.filters.len() * self.bands.len() * self.max_block];
        Node::reset(self);
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let ch = self.filters.len();
        let nb = self.bands.len();
        let mut targets = [Hz(0.0); 3];
        for ((t, cell), good) in targets.iter_mut().zip(&self.crossovers).zip(&mut self.good) {
            *t = Hz(good.read(cell.load().get()));
        }
        let (inputs, mut outputs) = io.split();

        let stride = nb * self.max_block;
        for (c, filter) in self.filters.iter_mut().enumerate() {
            // An unusable set (out of order, at Nyquist) keeps the last one.
            let _ = filter.set_frequencies(&targets[..nb - 1]);
            filter.split_block(&inputs.get(c)[..n], &mut self.split[c * stride..][..nb * n]);
        }

        let any_solo = self.bands.iter().any(|b| b.solo.load(Ordering::Acquire));
        for c in 0..ch {
            outputs.get(c)[..n].fill(0.0);
        }
        let gains = &mut self.gains[..n];
        for (b, band) in self.bands.iter_mut().enumerate() {
            let audible_to = if any_solo && !band.solo.load(Ordering::Acquire) {
                0.0
            } else {
                1.0
            };
            let audible_from = band.audible.replace(audible_to).unwrap_or(audible_to);

            // One reduction per frame from the band's loudest channel.
            if band.bypass.load(Ordering::Acquire) {
                band.core.reset();
                gains.fill(1.0);
            } else {
                gains.fill(0.0);
                for c in 0..ch {
                    let x = &self.split[c * stride + b * n..][..n];
                    for (level, &s) in gains.iter_mut().zip(x) {
                        *level = level.max(s.abs());
                    }
                }
                band.core.update_coefficients([None, None]);
                let block = band.core.begin_block([None, None]);
                for (i, g) in gains.iter_mut().enumerate() {
                    *g = band.core.gain(&block, i, n, *g, None);
                }
            }
            band.reduction
                .store(band.core.gain_reduction_db().get(), Ordering::Relaxed);

            if audible_from == 0.0 && audible_to == 0.0 {
                continue;
            }
            if audible_from != audible_to {
                for (i, g) in gains.iter_mut().enumerate() {
                    let t = (i + 1) as f32 / n as f32;
                    *g *= audible_from + (audible_to - audible_from) * t;
                }
            }
            for c in 0..ch {
                let x = &self.split[c * stride + b * n..][..n];
                for ((y, &x), &g) in outputs.get(c)[..n].iter_mut().zip(x).zip(gains.iter()) {
                    *y += x * g;
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        for f in &mut self.filters {
            f.reset();
        }
        for b in &mut self.bands {
            b.core.reset();
            b.audible = None;
            b.reduction.store(0.0, Ordering::Relaxed);
        }
    }
}

/// Shares the control cells (the template a fork is taken from); the filter
/// and detector state is copied.
impl Clone for MultibandCompressorNode {
    fn clone(&self) -> Self {
        Self {
            layout: self.layout,
            bands: self.bands.clone(),
            crossovers: self.crossovers.clone(),
            good: self.good.clone(),
            filters: self.filters.clone(),
            split: self.split.clone(),
            gains: self.gains.clone(),
            max_block: self.max_block,
        }
    }
}

impl Fresh for MultibandCompressorNode {
    /// A clone with every cell detached at its value (the meters too) and
    /// its state cleared.
    fn fresh(&self) -> Self {
        let mut fork = self.clone();
        for c in &mut fork.crossovers {
            c.detach();
        }
        for b in &mut fork.bands {
            b.core.threshold.detach();
            b.core.ratio.detach();
            b.core.timing.detach();
            b.core.makeup_db.detach();
            b.bypass = Arc::new(AtomicBool::new(b.bypass.load(Ordering::Acquire)));
            b.solo = Arc::new(AtomicBool::new(b.solo.load(Ordering::Acquire)));
            b.reduction = Arc::new(AtomicF32::new(0.0));
        }
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`MultibandControls`] as its handles; a fork starts from
/// the values last set.
impl IntoNode for MultibandCompressorNode {
    type Controls = MultibandControls;

    fn into_parts(self) -> NodeParts<MultibandControls> {
        let controls = self.controls();
        fresh_parts(self, controls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, amplitude_at, mean, prepared, render_chunked, sine};
    use core::f64::consts::PI;
    use tutti_graph::contract::drive;

    const FS: f64 = 48_000.0;
    /// Whole periods of every 5 Hz-grid tone, and of 3 kHz (16 samples).
    const WINDOW: usize = 9_600;
    const SETTLE: usize = 24_000;
    /// The crossover's own flatness tolerance (f32 rounding, summed over up
    /// to four bands).
    const TOL: f64 = 1e-6;

    fn node(bands: usize) -> (MultibandCompressorNode, MultibandControls) {
        let n = MultibandCompressorNode::new(ChannelLayout::MONO, bands).unwrap();
        let c = n.controls();
        (n, c)
    }

    /// Every band at unity ratio and no makeup: nothing is reduced.
    fn unity(c: &MultibandControls) {
        for b in 0..c.bands() {
            c.band(b).unwrap().set_ratio(1.0);
        }
    }

    /// The warped frequency ratio of the bilinear transform.
    fn warp(f: f64, fc: f64) -> f64 {
        libm::tan(PI * f / FS) / libm::tan(PI * fc / FS)
    }

    /// A unit sine through `node`, steady-state amplitude at `hz`.
    fn gain_at(node: &mut MultibandCompressorNode, hz: f64) -> f64 {
        let x = sine(hz, FS, SETTLE + WINDOW);
        let out = render_chunked(node, &[x], 512);
        amplitude_at(&out[0][SETTLE..], hz, FS)
    }

    /// Unity-ratio, zero-makeup bands reproduce the crossover's sum, which is
    /// allpass: unit magnitude at every frequency, for 2, 3 and 4 bands.
    ///
    /// Mutation (run): `*y += x * g` as `*y = x * g` in `process` (the last
    /// band overwrites the sum) → fails.
    #[test]
    fn unity_ratio_reproduces_the_allpass_sum() {
        for bands in 2..=4 {
            let (n, c) = node(bands);
            unity(&c);
            let mut n = prepared(n);
            for i in 0..16 {
                let hz =
                    (20.0 * (16_000.0f64 / 20.0).powf(f64::from(i) / 15.0) / 5.0).round() * 5.0;
                n.reset();
                let a = gain_at(&mut n, hz);
                assert!((a - 1.0).abs() < TOL, "{bands} bands at {hz} Hz: {a}");
            }
        }
    }

    /// The signal of the static-curve tests: a loud DC level (the band-0
    /// content with an exact detector reading) and a quiet 3 kHz tone (band 2).
    fn dc_and_tone(dc: f32, tone: f32) -> Vec<f32> {
        sine(3_000.0, FS, SETTLE + WINDOW)
            .into_iter()
            .map(|s| dc + tone * s)
            .collect()
    }

    /// Loud low content compresses its own band by the compressor's static
    /// curve and leaves a quiet tone in band 2 alone.
    ///
    /// The loud content is a constant 0.5 (-6.02 dB): it is all in band 0 once
    /// the crossover settles, and a constant is the one input whose detector
    /// reading is exact, so the settled reduction is the curve's value and not
    /// a ripple. With a -20 dB threshold at 4:1 the reduction is
    /// `(-6.02 + 20) * 0.75` dB, and a 2 dB makeup rides on it.
    ///
    /// The follower is an `f32` one-pole, which stalls once its step falls
    /// under half an ulp of the 10.5 dB reduction: within `ulp / (2 * coeff)`
    /// of the curve, `coeff = 0.021` at 1 ms and 48 kHz, so 2.3e-5 dB or
    /// 2.7e-6 of the level: the tolerance is 5e-6.
    ///
    /// The tone is 0.01 (-40 dB), under every band's -18 dB threshold. Band
    /// 0 carries `1 / (1 + 25^4) = 2.6e-6` of it, so it is unchanged to far
    /// less than the `f32` floor: the input, band 0's output and the sum
    /// each round the 0.5 background to a grid of half-ulp `3e-8`, and
    /// that rounding repeats at the tone's 16-sample period, so the tone's
    /// bin is good to six half-ulps, `2e-7`.
    ///
    /// Mutation (run): the loop applying one gain to every band (band 0's
    /// `g` kept for the others) → the tone comes out near 0.003 → fails;
    /// ignoring `makeup` (`begin_block`'s `makeup_to` taken as 0) → the DC is
    /// 2 dB low → fails.
    #[test]
    fn a_loud_low_band_follows_the_static_curve_and_a_quiet_band_is_left_alone() {
        let (n, c) = node(4);
        let b0 = c.band(0).unwrap();
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        b0.set_makeup(2.0);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.01)], 512);
        let y = &out[0][SETTLE..];

        let level_db = 20.0 * libm::log10(0.5);
        let reduction = (level_db + 20.0) * (1.0 - 1.0 / 4.0);
        let expected = 0.5 * libm::pow(10.0, (2.0 - reduction) / 20.0);
        let dc = mean(y);
        assert!(
            (dc - expected).abs() < 5e-6 * expected,
            "band 0 settled at {dc}, the curve says {expected}"
        );
        let tone = amplitude_at(y, 3_000.0, FS);
        assert!((tone - 0.01).abs() < 2e-7, "the quiet tone is {tone}");

        // The meter reads the reduction the curve gives, and only on band 0.
        let meter = f64::from(c.band(0).unwrap().gain_reduction_db().get());
        assert!(
            (meter - reduction).abs() < 1e-3,
            "meter {meter} dB vs {reduction}"
        );
        assert_eq!(
            n.gain_reduction_db(0),
            c.band(0).unwrap().gain_reduction_db()
        );
        // The DC's onset is a step through the crossover, which rings the
        // upper bands over their threshold for a moment; their 100 ms release
        // has taken all but a hair of it off by the end.
        for b in 1..4 {
            let held = c.band(b).unwrap().gain_reduction_db();
            assert!(held < Db(1e-3), "band {b} still reduces {held:?}");
        }
        assert_eq!(n.gain_reduction_db(9), Db::UNITY);
    }

    /// Makeup is applied per band, after reduction, on every band.
    ///
    /// Two bands split at 1 kHz: DC is all band 0, and 10 kHz is
    /// `x^4 / (1 + x^4)` band 1 with `x` the warped ratio, the rest
    /// (`1e-4`) band 0. Band 0 -3 dB and band 1 +6 dB.
    ///
    /// Mutation (run): drop `makeup` from `CompressorCore::begin_block`'s
    /// `makeup_to` → both levels are unity → fails.
    #[test]
    fn makeup_applies_per_band() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).unwrap().set_makeup(-3.0);
        c.band(1).unwrap().set_makeup(6.0);
        let mut n = prepared(n);
        let tone: Vec<f32> = sine(10_000.0, FS, SETTLE + WINDOW)
            .into_iter()
            .map(|s| 0.5 + 0.1 * s)
            .collect();
        let out = render_chunked(&mut n, &[tone], 512);
        let y = &out[0][SETTLE..];
        let dc = mean(y);
        let want_dc = 0.5 * libm::pow(10.0, -3.0 / 20.0);
        assert!((dc - want_dc).abs() < 1e-6, "{dc} vs {want_dc}");
        let g1 = libm::pow(10.0, 6.0 / 20.0);
        let leak = 1.0 / (1.0 + warp(10_000.0, 1_000.0).powi(4));
        // Band 0's `leak` of the tone runs at -3 dB instead of +6 dB.
        let tol = 0.1 * leak * (g1 - 0.7) + 1e-6;
        let amp = amplitude_at(y, 10_000.0, FS);
        assert!((amp - 0.1 * g1).abs() < tol, "{amp} vs {}", 0.1 * g1);
    }

    /// A bypassed band passes unreduced and without makeup; releasing the
    /// bypass compresses again.
    ///
    /// Mutation (run): ignore `bypass` in `process` → the DC is reduced →
    /// fails.
    #[test]
    fn bypass_passes_the_band_unprocessed() {
        let (n, c) = node(4);
        let b0 = c.band(0).unwrap();
        b0.set_threshold(-30.0);
        b0.set_ratio(8.0);
        b0.set_makeup(6.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        b0.set_bypass(true);
        let mut n = prepared(n);
        let x = vec![0.5f32; SETTLE + WINDOW];
        let out = render_chunked(&mut n, std::slice::from_ref(&x), 512);
        assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < TOL);
        assert_eq!(b0.gain_reduction_db(), Db::UNITY);
        b0.set_bypass(false);
        let out = render_chunked(&mut n, &[x], 512);
        let dc = mean(&out[0][SETTLE..]);
        assert!(dc < 0.4 || dc > 0.6, "compressing again: {dc}");
        assert!(b0.gain_reduction_db() > Db(1.0));
    }

    /// A soloed band is all that is heard: the output is that band's
    /// analytic share of the input.
    ///
    /// Band 2 of four (1 kHz to 6 kHz) of a 3 kHz tone is
    /// `HP(120) * HP(1 kHz) * LP(6 kHz)`, each `x^4/(1+x^4)` or `1/(1+x^4)`
    /// with the warped ratio; a constant is in band 0 and must vanish.
    ///
    /// Mutation (run): ignore `solo` in `process` → the DC is heard → fails.
    #[test]
    fn solo_plays_only_the_soloed_band() {
        let (n, c) = node(4);
        unity(&c);
        c.band(2).unwrap().set_solo(true);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        let y = &out[0][SETTLE..];
        let hp = |fc: f64| {
            let x4 = warp(3_000.0, fc).powi(4);
            x4 / (1.0 + x4)
        };
        let lp = 1.0 / (1.0 + warp(3_000.0, 6_000.0).powi(4));
        let want = 0.2 * hp(120.0) * hp(1_000.0) * lp;
        let amp = amplitude_at(y, 3_000.0, FS);
        assert!((amp - want).abs() < 1e-6, "{amp} vs {want}");
        assert!(
            mean(y).abs() < 1e-6,
            "the unsoloed DC is heard: {}",
            mean(y)
        );
        // Un-soloing restores the whole sum.
        c.band(2).unwrap().set_solo(false);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < TOL);
    }

    /// Soloing mutes the other bands across the block, not at its edge.
    ///
    /// DC through band 0, then band 1 soloed: the next 64-frame block falls
    /// from the old level to 0 in 64 equal steps, so its first frame is
    /// within a step and a half of 0.5 and its last is 0.
    ///
    /// Mutation (run): `audible_to` taken whole (`audible_from = audible_to`)
    /// → the first frame is 0 → fails.
    #[test]
    fn a_solo_ramps_the_mute() {
        let (n, c) = node(2);
        unity(&c);
        let mut n = prepared(n);
        let settle = vec![0.5f32; SETTLE];
        render_chunked(&mut n, &[settle], 512);
        c.band(1).unwrap().set_solo(true);
        let y = render_chunked(&mut n, &[vec![0.5f32; 64]], 64).remove(0);
        assert!((y[0] - 0.5).abs() <= 1.5 * 0.5 / 64.0, "first {}", y[0]);
        assert!(y[63].abs() < 1e-6, "last {}", y[63]);
        assert!(y.windows(2).all(|w| w[1] <= w[0]), "monotone fall");
    }

    /// Detection is linked across channels within a band: a quiet channel is
    /// reduced by what the loud one drives.
    ///
    /// L = 0.05 and R = 0.5 as DC through band 0 at -20 dB, 4:1. Unlinked,
    /// L (-26 dB) is under the threshold and untouched; linked, both are
    /// scaled by the one gain the loud channel sets.
    ///
    /// Mutation (run): the level loop run over channel 0 only → L is
    /// untouched → fails; one gain per channel from its own level → fails.
    #[test]
    fn detection_is_linked_across_channels() {
        let n = MultibandCompressorNode::new(ChannelLayout::STEREO, 2).unwrap();
        let c = n.controls();
        let b0 = c.band(0).unwrap();
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.005);
        b0.set_release(0.005);
        let mut n = prepared(n);
        let len = 12_000;
        let out = render_chunked(&mut n, &[vec![0.05; len], vec![0.5; len]], 512);
        let (l, r) = (mean(&out[0][len - 2_000..]), mean(&out[1][len - 2_000..]));
        let gain = r / 0.5;
        let want = libm::pow(10.0, -(20.0 * libm::log10(0.5) + 20.0) * 0.75 / 20.0);
        assert!((gain - want).abs() < 1e-5, "{gain} vs {want}");
        assert!(
            (l / 0.05 - gain).abs() < 1e-5,
            "L gain {} vs R {gain}",
            l / 0.05
        );
    }

    /// A band count outside 2 to 4, or a layout with no ports, is refused.
    ///
    /// Mutation (run): `MAX_BANDS` of 5 → five bands build → fails.
    #[test]
    fn the_band_count_and_layout_are_validated() {
        for bands in [0, 1, 5, 6, 100] {
            assert_eq!(
                MultibandCompressorNode::new(ChannelLayout::STEREO, bands).unwrap_err(),
                CrossoverError::Bands {
                    bands,
                    min: 2,
                    max: 4
                }
            );
        }
        for bands in 2..=4 {
            let n = MultibandCompressorNode::new(ChannelLayout::STEREO, bands).unwrap();
            assert_eq!((n.bands(), n.controls().bands()), (bands, bands));
        }
        assert_eq!(
            MultibandCompressorNode::new(ChannelLayout::from_count(0), 3).unwrap_err(),
            CrossoverError::Channels { channels: 0 }
        );
        assert_eq!(
            MultibandCompressorNode::new(ChannelLayout::from_count(65), 3).unwrap_err(),
            CrossoverError::Channels { channels: 65 }
        );
    }

    /// The defaults for each band count, and the crossover setter's checks.
    ///
    /// Mutation (run): `hz >= at(i + 1)` to `>` in `check_neighbours` → the
    /// equal pair is accepted → fails.
    #[test]
    fn crossover_defaults_and_setter_validation() {
        let (_, c) = node(4);
        assert_eq!(
            [
                c.crossover(0),
                c.crossover(1),
                c.crossover(2),
                c.crossover(3)
            ],
            [Some(Hz(120.0)), Some(Hz(1_000.0)), Some(Hz(6_000.0)), None]
        );
        assert_eq!(node(3).1.crossover(1), Some(Hz(3_000.0)));
        assert_eq!(node(2).1.crossover(0), Some(Hz(1_000.0)));
        for (i, hz) in [
            (0, f32::NAN),
            (0, f32::INFINITY),
            (0, f32::NEG_INFINITY),
            (0, 0.0),
            (1, 120.0),
            (1, 6_000.0),
            (0, 1_000.0),
            (2, 1_000.0),
        ] {
            assert!(c.set_crossover(i, Hz(hz)).is_err(), "{i} {hz}");
        }
        assert_eq!(
            c.set_crossover(3, Hz(9_000.0)),
            Err(CrossoverError::Index { index: 3, len: 3 })
        );
        assert_eq!(c.crossover(1), Some(Hz(1_000.0)));
        c.set_crossover(1, Hz(2_000.0)).unwrap();
        assert_eq!(c.crossover(1), Some(Hz(2_000.0)));
    }

    /// The per-band setters clamp as `CompressorNode`'s do, and the getters
    /// read back.
    ///
    /// Mutation (run): drop the clamp in `set_ratio` → 0.25 reads back → fails.
    #[test]
    fn setters_clamp_and_read_back() {
        let (_, c) = node(2);
        let b = c.band(1).unwrap();
        b.set_ratio(0.25);
        assert_eq!(b.ratio(), CompressionRatio(1.0));
        b.set_attack(-1.0);
        b.set_release(-1.0);
        b.set_knee(-6.0);
        assert_eq!(
            (b.attack(), b.release(), b.knee()),
            (Seconds(0.0), Seconds(0.0), Db(0.0))
        );
        b.set_threshold(-30.0);
        b.set_makeup(4.0);
        b.set_bypass(true);
        b.set_solo(true);
        assert_eq!(
            (b.threshold(), b.makeup(), b.bypassed(), b.soloed()),
            (Db(-30.0), Db(4.0), true, true)
        );
        assert!(c.band(2).is_none());
    }

    /// A non-finite threshold or makeup written mid-stream reads as unchanged,
    /// and so does a non-finite crossover cell (poked in directly: the handle
    /// refuses to write one).
    ///
    /// Mutation (run): drop the `LastGood` on the makeup in
    /// `CompressorCore::begin_block` → the output goes NaN → fails. The
    /// crossover cells have two guards (`LastGood` and `set_frequencies`'s
    /// own check), so dropping either alone passes by design.
    #[test]
    fn non_finite_writes_never_reach_the_state() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let (n, c) = node(3);
            let mut n = prepared(n);
            let x = sine(440.0, FS, 2_048);
            let finite = |n: &mut MultibandCompressorNode| {
                render_chunked(n, std::slice::from_ref(&x), 512)[0]
                    .iter()
                    .all(|s| s.is_finite())
            };
            assert!(finite(&mut n));
            c.band(1).unwrap().set_threshold(bad);
            c.band(1).unwrap().set_makeup(bad);
            c.band(2).unwrap().set_makeup(bad);
            n.crossovers[0].store(Hz(bad));
            n.crossovers[1].store(Hz(bad));
            assert!(finite(&mut n), "{bad}");
            c.band(1).unwrap().set_threshold(-30.0);
            c.band(1).unwrap().set_makeup(0.0);
            c.band(2).unwrap().set_makeup(0.0);
            assert!(finite(&mut n), "{bad} then finite");
        }
    }

    /// A fork starts from the values set and shares no cell, meter included.
    ///
    /// Mutation (run): drop the `reduction` replacement in `fresh` → the
    /// fork's meter is the live node's → fails; drop
    /// `b.core.makeup_db.detach()` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        let (n, live) = node(3);
        live.band(1).unwrap().set_threshold(-33.0);
        live.band(1).unwrap().set_makeup(5.0);
        live.band(2).unwrap().set_bypass(true);
        live.band(0).unwrap().set_solo(true);
        live.set_crossover(1, Hz(4_000.0)).unwrap();
        let fork = n.fresh();
        let theirs = fork.controls();
        assert_eq!(theirs.band(1).unwrap().threshold(), Db(-33.0));
        assert_eq!(theirs.band(1).unwrap().makeup(), Db(5.0));
        assert!(theirs.band(2).unwrap().bypassed() && theirs.band(0).unwrap().soloed());
        assert_eq!(theirs.crossover(1), Some(Hz(4_000.0)));

        live.band(1).unwrap().set_threshold(-1.0);
        live.band(1).unwrap().set_makeup(-1.0);
        live.band(2).unwrap().set_bypass(false);
        live.band(0).unwrap().set_solo(false);
        live.set_crossover(1, Hz(5_000.0)).unwrap();
        assert_eq!(theirs.band(1).unwrap().threshold(), Db(-33.0));
        assert_eq!(theirs.band(1).unwrap().makeup(), Db(5.0));
        assert!(theirs.band(2).unwrap().bypassed() && theirs.band(0).unwrap().soloed());
        assert_eq!(theirs.crossover(1), Some(Hz(4_000.0)));
        theirs
            .band(1)
            .unwrap()
            .reduction
            .store(7.0, Ordering::Relaxed);
        assert_eq!(live.band(1).unwrap().gain_reduction_db(), Db::UNITY);
    }

    /// Zero latency, no param ports, `Tail::Unknown`, heavy.
    ///
    /// Mutation (run): `with_latency` of one frame → fails.
    #[test]
    fn the_shape_is_zero_latency_with_no_ports() {
        let n = MultibandCompressorNode::new(ChannelLayout::STEREO, 4).unwrap();
        let s = n.shape();
        assert_eq!((s.audio_in.count(), s.audio_out.count()), (2, 2));
        assert_eq!(s.latency, tutti_types::Latency::default());
        assert_eq!(s.tail, Tail::Unknown);
        assert_eq!(s.cost, Cost::Heavy);
        assert!(s.params.is_empty());
    }

    /// A one-frame block and a full 1024-frame block both render.
    #[test]
    fn any_block_up_to_the_prepared_maximum_renders() {
        let (n, c) = node(4);
        c.set_crossover(0, Hz(200.0)).unwrap();
        let mut n = prepared(n);
        let x = sine(1_000.0, FS, 1_024);
        let big = drive(&mut n, RATE, &[&x], &[]).remove(0);
        assert!(big.iter().all(|s| s.is_finite()) && big[1_023].abs() > 0.0);
        for s in &x[..8] {
            let one = drive(&mut n, RATE, &[std::slice::from_ref(s)], &[]).remove(0);
            assert!(one[0].is_finite());
        }
    }
}
