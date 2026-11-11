//! [`MultibandCompressorNode`]: a compressor per band of a Linkwitz-Riley
//! crossover, summed.

use std::fmt;

use std::sync::Arc;
use tutti_graph::{
    Cost, Cx, IntoNode, Io, MAX_PARAM_PORTS, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape,
    Status,
};
use tutti_types::{AtomicF32, CompressionRatio, Db, Hz, Param, SampleRate, Seconds, Tail};
use tutti_types::{ChannelLayout, UnitAddr, UnitParam};

use super::compressor::CompressorCore;
use crate::filter::Splits;
use crate::ramp::block_driven;
use tutti_dsp::{Crossover, CrossoverError};

/// The crossover frequencies a band count starts with.
fn default_crossovers(bands: usize) -> &'static [Hz] {
    match bands {
        2 => &[Hz(1_000.0)],
        3 => &[Hz(200.0), Hz(3_000.0)],
        _ => &[Hz(120.0), Hz(1_000.0), Hz(6_000.0)],
    }
}

/// The threshold a band starts at.
const DEFAULT_THRESHOLD: Db = Db(-18.0);
/// The ratio a band starts at.
const DEFAULT_RATIO: CompressionRatio = CompressionRatio(2.0);
/// The attack a band starts at.
const DEFAULT_ATTACK: Seconds = Seconds(0.010);
/// The release a band starts at.
const DEFAULT_RELEASE: Seconds = Seconds(0.100);

/// The most crossovers: one fewer than the bands.
const MAX_CROSSOVERS: usize = MultibandCompressorNode::MAX_BANDS - 1;

/// The params of one band, in port order: the band is the address's index.
/// The make-up gain is [`UnitParam::GainDb`], as [`CompressorNode`]'s is
/// (one cell, one address).
///
/// [`CompressorNode`]: crate::CompressorNode
pub const BAND_PARAMS: [UnitParam; 6] = [
    UnitParam::Threshold,
    UnitParam::Knee,
    UnitParam::Ratio,
    UnitParam::Attack,
    UnitParam::Release,
    UnitParam::GainDb,
];

/// Param ports per band.
const PER_BAND: usize = BAND_PARAMS.len();
/// Param ports at the most bands: the band params, then the crossovers.
const MAX_PORTS: usize = MultibandCompressorNode::MAX_BANDS * PER_BAND + MAX_CROSSOVERS;
const _: () = assert!(MAX_PORTS <= MAX_PARAM_PORTS);

/// The make-up gain's place in [`BAND_PARAMS`].
const MAKEUP: usize = 5;

/// A toggle cell's value: `>= 0.5` is on (so a NaN is off), as
/// [`UnitParam::Bypass`] encodes it.
#[inline]
fn is_on(cell: &AtomicF32) -> bool {
    cell.load(core::sync::atomic::Ordering::Acquire) >= 0.5
}

/// The handles on a [`MultibandCompressorNode`] once inserted: its
/// [`ParamSet`] and the band meters. Clones share the cells.
///
/// The settings are the set's; the meters are readings, not controls, so
/// they are not in it.
#[derive(Clone, Debug)]
pub struct MultibandControls {
    params: ParamSet,
    reductions: Vec<Param<Db>>,
}

impl MultibandControls {
    /// Bypasses band `band` (see [`UnitParam::Bypass`]); `false` past the last
    /// band, and nothing is written then.
    pub fn set_bypass(&self, band: usize, on: bool) -> bool {
        self.toggle(UnitParam::Bypass, band, on)
    }

    /// Whether band `band` is bypassed; `false` past the last band.
    #[must_use]
    pub fn is_bypassed(&self, band: usize) -> bool {
        self.is_toggled(UnitParam::Bypass, band)
    }

    /// Solos band `band` (see [`UnitParam::Solo`]); `false` past the last
    /// band, and nothing is written then.
    pub fn set_solo(&self, band: usize, on: bool) -> bool {
        self.toggle(UnitParam::Solo, band, on)
    }

    /// Whether band `band` is soloed; `false` past the last band.
    #[must_use]
    pub fn is_soloed(&self, band: usize) -> bool {
        self.is_toggled(UnitParam::Solo, band)
    }

    fn toggle(&self, kind: UnitParam, band: usize, on: bool) -> bool {
        u8::try_from(band).is_ok_and(|b| self.params.set(UnitAddr::new(kind, b), f32::from(on)))
    }

    fn is_toggled(&self, kind: UnitParam, band: usize) -> bool {
        u8::try_from(band)
            .ok()
            .and_then(|b| self.params.get(UnitAddr::new(kind, b)))
            .is_some_and(|v| v >= 0.5)
    }

    /// The node's params by address (see [`MultibandCompressorNode`]).
    #[must_use]
    pub fn params(&self) -> &ParamSet {
        &self.params
    }

    /// How many bands there are.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.reductions.len()
    }

    /// Band `band`'s largest gain reduction in the last block, in [`Db`]: a
    /// **measurement**, for a reduction meter. Larger is more reduction;
    /// [`Db::UNITY`] is none (also while the band is fully bypassed, and past
    /// the last band).
    ///
    /// The node writes it once per block, so reading it from any thread is
    /// wait-free.
    #[must_use]
    pub fn gain_reduction_db(&self, band: usize) -> Db {
        self.reductions.get(band).map_or(Db::UNITY, Param::load)
    }
}

/// One band's compressor and its running state.
#[derive(Clone, Debug)]
struct Band {
    core: CompressorCore,
    /// `>= 0.5` bypasses the band ([`UnitParam::Bypass`]).
    bypass: Arc<AtomicF32>,
    /// `>= 0.5` solos the band ([`UnitParam::Solo`]).
    solo: Arc<AtomicF32>,
    reduction: Param<Db>,
    /// The bypass weight (1 bypassed, 0 compressing) the previous block ended
    /// on; `None` until the first block.
    bypassed: Option<f32>,
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
/// The split is IIR and **not linear-phase**: the bands share one phase
/// response (see [`Crossover`]), which rotates with frequency, and their sum
/// is allpass (flat magnitude, phase rotating through each crossover). With every band at unity ratio and 0 dB makeup the
/// output has the input's magnitude at every frequency. The node adds no
/// latency.
///
/// # Params
///
/// Every setting is a param on the node's [`ParamSet`], addressed by a kind
/// and an index (the band, or the crossover):
///
/// | Address | Unit | Meaning |
/// |---|---|---|
/// | `Threshold`, band | [`Db`] | level above which the band is reduced |
/// | `Knee`, band | [`Db`] | knee width, floored at 0 (hard) |
/// | `Ratio`, band | [`CompressionRatio`] | clamped to at least 1 |
/// | `Attack`, band | [`Seconds`] | floored at 0 |
/// | `Release`, band | [`Seconds`] | floored at 0 |
/// | `GainDb`, band | [`Db`] | make-up after reduction |
/// | `Cutoff`, crossover | [`Hz`] | between band `i` and band `i + 1` |
/// | `Bypass`, band | toggle | `>= 0.5` passes the band unreduced |
/// | `Solo`, band | toggle | `>= 0.5` solos the band |
///
/// That is `bands * 6 + (bands - 1)` param ports the graph can modulate (27
/// at 4 bands, under `MAX_PARAM_PORTS`), in the order of [`BAND_PARAMS`] band
/// by band and then the crossovers. The threshold is read per sample; the
/// rest once per block, with the make-up, the crossover frequencies and a
/// solo's mute ramped across it. A non-finite write reads as unchanged. The
/// crossovers follow [`CrossoverNode`](crate::CrossoverNode)'s rules: a set
/// not strictly increasing is refused as a whole, after each frequency is
/// fitted under the rate (so `[30000, 25000]` at 48 kHz is clamped into order
/// and accepted).
///
/// The toggles are cells with no port, as [`BusStripNode`](crate::BusStripNode)'s
/// mute is: they are a block-rate on/off a lane would only flap, and with
/// them the ports would pass the graph's 32. A bypass blends the band to
/// unity across a block and the compressor keeps running meanwhile, so
/// releasing it resumes from a warm detector. While any band is soloed the
/// output is the sum of the soloed bands only; the others are muted (ramped
/// across a block) but keep metering.
///
/// The gain reduction of each band is a measurement:
/// [`gain_reduction_db`](Self::gain_reduction_db), or
/// [`MultibandControls::gain_reduction_db`] once inserted.
pub struct MultibandCompressorNode {
    layout: ChannelLayout,
    bands: Vec<Band>,
    splits: Splits,
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
        let proto = Crossover::new(defaults, SampleRate::DEFAULT)?;
        Ok(Self {
            layout,
            bands: (0..bands)
                .map(|_| Band {
                    core: CompressorCore::new(
                        DEFAULT_THRESHOLD,
                        DEFAULT_RATIO,
                        DEFAULT_ATTACK,
                        DEFAULT_RELEASE,
                    ),
                    bypass: Arc::new(AtomicF32::new(0.0)),
                    solo: Arc::new(AtomicF32::new(0.0)),
                    reduction: Param::new(Db::UNITY),
                    bypassed: None,
                    audible: None,
                })
                .collect(),
            splits: Splits::new(defaults),
            filters: vec![proto; channels],
            split: Vec::new(),
            gains: Vec::new(),
            max_block: 0,
        })
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

    /// Band `band`'s largest gain reduction in the last block, in [`Db`] (see
    /// [`MultibandControls::gain_reduction_db`]); [`Db::UNITY`] past the last
    /// band.
    #[must_use]
    pub fn gain_reduction_db(&self, band: usize) -> Db {
        self.bands
            .get(band)
            .map_or(Db::UNITY, |b| b.reduction.load())
    }

    /// The param ports, in port order, and how many.
    fn port_addrs(&self) -> ([UnitAddr; MAX_PORTS], usize) {
        let mut addrs = [UnitAddr::first(UnitParam::Cutoff); MAX_PORTS];
        let mut n = 0;
        for b in 0..self.bands.len() {
            for kind in BAND_PARAMS {
                addrs[n] = UnitAddr::new(kind, b as u8);
                n += 1;
            }
        }
        for i in 0..self.bands.len() - 1 {
            addrs[n] = Splits::addr(i);
            n += 1;
        }
        (addrs, n)
    }
}

impl Node for MultibandCompressorNode {
    /// `channels` in, `channels` out; zero latency; a param port for each
    /// band parameter and each crossover (see the type's docs).
    ///
    /// Heavy (the default cost): a split into up to 4 bands and a compressor
    /// for each, per channel. The tail is [`Tail::Unknown`], as the other IIR
    /// filters': the crossover rings after the input stops.
    fn shape(&self) -> Shape {
        let (addrs, n) = self.port_addrs();
        Shape::through_unassigned(self.layout)
            .with_cost(Cost::Heavy)
            .with_tail(Tail::Unknown)
            .with_param_addrs(&addrs[..n])
    }

    fn prepare(&mut self, p: &Prepare) {
        let targets = self.splits.read(p.sample_rate(), |_| None);
        let splits = self.bands.len() - 1;
        for f in &mut self.filters {
            f.set_sample_rate(p.sample_rate());
            // Taken whole by the `reset` below, so the first block does not
            // ramp in from the constructor's frequencies.
            let _ = f.set_frequencies(&targets[..splits]);
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
        let targets = self.splits.read(self.filters[0].sample_rate(), |i| {
            block_driven(&io, nb * PER_BAND + i)
        });
        // What the graph drives on each band's ports, taken before the split
        // of `io`: the threshold's frames, and the others' last frame.
        let mut driven = [[None; PER_BAND]; Self::MAX_BANDS];
        let mut threshold_fed = [None; Self::MAX_BANDS];
        for b in 0..nb {
            threshold_fed[b] = io.param(b * PER_BAND).frames();
            for (k, d) in driven[b].iter_mut().enumerate() {
                *d = block_driven(&io, b * PER_BAND + k);
            }
        }
        let (inputs, mut outputs) = io.split();

        let stride = nb * self.max_block;
        for (c, filter) in self.filters.iter_mut().enumerate() {
            // `read` fitted every entry to the rate, so only a set that is
            // out of order or not above 0 is refused, and it keeps the last
            // good one.
            let _ = filter.set_frequencies(&targets[..nb - 1]);
            filter.split_block(&inputs.get(c)[..n], &mut self.split[c * stride..][..nb * n]);
        }

        // Each flag once, so a write mid-block cannot split the bands'
        // view of who is soloed.
        let mut solo = [false; Self::MAX_BANDS];
        let mut bypass = [false; Self::MAX_BANDS];
        for (b, band) in self.bands.iter().enumerate() {
            solo[b] = is_on(&band.solo);
            bypass[b] = is_on(&band.bypass);
        }
        let any_solo = solo.iter().any(|&s| s);
        for c in 0..ch {
            outputs.get(c)[..n].fill(0.0);
        }
        let gains = &mut self.gains[..n];
        for (b, band) in self.bands.iter_mut().enumerate() {
            let audible_to = if any_solo && !solo[b] { 0.0 } else { 1.0 };
            let audible_from = band.audible.replace(audible_to).unwrap_or(audible_to);
            let bypass_to = if bypass[b] { 1.0 } else { 0.0 };
            let bypass_from = band.bypassed.replace(bypass_to).unwrap_or(bypass_to);

            // One reduction per frame from the band's loudest channel. The
            // compressor runs while bypassed too, so a release finds it warm.
            gains.fill(0.0);
            for c in 0..ch {
                let x = &self.split[c * stride + b * n..][..n];
                for (level, &s) in gains.iter_mut().zip(x) {
                    *level = level.max(s.abs());
                }
            }
            let d = driven[b];
            band.core.update_coefficients([d[3], d[4]]);
            let block = band.core.begin_block([d[1], d[2], d[MAKEUP]]);
            let mut deepest = 0.0f32;
            for (i, g) in gains.iter_mut().enumerate() {
                let threshold = threshold_fed[b].map(|v| Db(v[i]));
                *g = band.core.gain(&block, i, n, *g, threshold);
                deepest = deepest.max(band.core.gain_reduction_db().get());
            }
            let held = bypass_from == 1.0 && bypass_to == 1.0;
            band.reduction
                .store(if held { Db::UNITY } else { Db(deepest) });

            if bypass_from != 0.0 || bypass_to != 0.0 {
                // Blend toward unity (makeup included) as the bypass takes hold.
                for (i, g) in gains.iter_mut().enumerate() {
                    let t = (i + 1) as f32 / n as f32;
                    let w = bypass_from + (bypass_to - bypass_from) * t;
                    *g = *g * (1.0 - w) + w;
                }
            }
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
            b.bypassed = None;
            b.reduction.store(Db::UNITY);
        }
    }

    fn param_base(&self, port: usize) -> Option<f32> {
        let Some(band) = self
            .bands
            .get(port / PER_BAND)
            .filter(|_| port < self.bands.len() * PER_BAND)
        else {
            return self.splits.base(port - self.bands.len() * PER_BAND);
        };
        Some(match port % PER_BAND {
            0 => band.core.threshold.threshold.load().get(),
            1 => band.core.threshold.knee.load().get(),
            2 => band.core.ratio.load().get(),
            3 => band.core.timing.attack.load().to_f32(),
            4 => band.core.timing.release.load().to_f32(),
            _ => band.core.makeup_db.load().get(),
        })
    }
}

/// Shares the control cells (the template a fork is taken from); the filter
/// and detector state is copied.
impl Clone for MultibandCompressorNode {
    fn clone(&self) -> Self {
        Self {
            layout: self.layout,
            bands: self.bands.clone(),
            splits: self.splits.clone(),
            filters: self.filters.clone(),
            split: self.split.clone(),
            gains: self.gains.clone(),
            max_block: self.max_block,
        }
    }
}

impl ParamNode for MultibandCompressorNode {
    /// Every address in the type's table, the bands' then the crossovers'.
    fn param_set(&self) -> ParamSet {
        let mut set = ParamSet::builder();
        for (b, band) in self.bands.iter().enumerate() {
            let b = b as u8;
            let core = &band.core;
            set = set
                .param(
                    UnitAddr::new(UnitParam::Threshold, b),
                    core.threshold.threshold.as_atomic(),
                )
                .param(
                    UnitAddr::new(UnitParam::Knee, b),
                    core.threshold.knee.as_atomic(),
                )
                .param(UnitAddr::new(UnitParam::Ratio, b), core.ratio.as_atomic())
                .param(
                    UnitAddr::new(UnitParam::Attack, b),
                    core.timing.attack.as_atomic(),
                )
                .param(
                    UnitAddr::new(UnitParam::Release, b),
                    core.timing.release.as_atomic(),
                )
                .param(
                    UnitAddr::new(UnitParam::GainDb, b),
                    core.makeup_db.as_atomic(),
                )
                .param(
                    UnitAddr::new(UnitParam::Bypass, b),
                    Arc::clone(&band.bypass),
                )
                .param(UnitAddr::new(UnitParam::Solo, b), Arc::clone(&band.solo));
        }
        for i in 0..self.bands.len() - 1 {
            set = set.param(Splits::addr(i), self.splits.cell(i));
        }
        set.build()
    }

    /// A clone with every cell detached at its value (the meters too) and
    /// its state cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.splits.detach();
        for b in &mut fork.bands {
            b.core.detach();
            b.bypass = Arc::new(AtomicF32::new(
                b.bypass.load(core::sync::atomic::Ordering::Acquire),
            ));
            b.solo = Arc::new(AtomicF32::new(
                b.solo.load(core::sync::atomic::Ordering::Acquire),
            ));
            b.reduction = Param::new(Db::UNITY);
        }
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`MultibandControls`] as its controls; a fork starts
/// from the values last set through its params.
impl IntoNode for MultibandCompressorNode {
    type Controls = MultibandControls;

    fn into_parts(self) -> NodeParts<MultibandControls> {
        let reductions = self.bands.iter().map(|b| b.reduction.handle()).collect();
        let NodeParts {
            node,
            controls,
            fork,
        } = tutti_graph::param_parts(self);
        NodeParts {
            node,
            controls: MultibandControls {
                params: controls,
                reductions,
            },
            fork,
        }
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

    /// A node's params by band and crossover, over its [`ParamSet`].
    struct Ctl {
        set: ParamSet,
        bands: usize,
    }

    /// One band's params, under the names the settings have.
    struct Bd<'a>(&'a Ctl, u8);

    impl Ctl {
        fn new(n: &MultibandCompressorNode) -> Self {
            Self {
                set: n.param_set(),
                bands: n.bands(),
            }
        }

        fn band(&self, b: usize) -> Bd<'_> {
            Bd(self, b as u8)
        }

        /// Writes `v` to `kind` at `index`; the node must have it.
        fn put(&self, kind: UnitParam, index: u8, v: f32) {
            assert!(
                self.set.set(UnitAddr::new(kind, index), v),
                "{kind:?} {index}"
            );
        }

        fn set_crossover(&self, i: usize, hz: Hz) {
            self.put(UnitParam::Cutoff, i as u8, hz.get());
        }
    }

    impl Bd<'_> {
        fn put(&self, kind: UnitParam, v: f32) {
            self.0.put(kind, self.1, v);
        }
        fn set_threshold(&self, db: f32) {
            self.put(UnitParam::Threshold, db);
        }
        fn set_knee(&self, db: f32) {
            self.put(UnitParam::Knee, db);
        }
        fn set_ratio(&self, r: f32) {
            self.put(UnitParam::Ratio, r);
        }
        fn set_attack(&self, s: f32) {
            self.put(UnitParam::Attack, s);
        }
        fn set_release(&self, s: f32) {
            self.put(UnitParam::Release, s);
        }
        fn set_makeup(&self, db: f32) {
            self.put(UnitParam::GainDb, db);
        }
        fn set_bypass(&self, on: bool) {
            self.put(UnitParam::Bypass, f32::from(on));
        }
        fn set_solo(&self, on: bool) {
            self.put(UnitParam::Solo, f32::from(on));
        }
    }

    fn node(bands: usize) -> (MultibandCompressorNode, Ctl) {
        let n = MultibandCompressorNode::new(ChannelLayout::MONO, bands).unwrap();
        let c = Ctl::new(&n);
        (n, c)
    }

    /// Every band at unity ratio and no makeup: nothing is reduced.
    fn unity(c: &Ctl) {
        for b in 0..c.bands {
            c.band(b).set_ratio(1.0);
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
    #[test]
    fn a_loud_low_band_follows_the_static_curve_and_a_quiet_band_is_left_alone() {
        let (n, c) = node(4);
        let b0 = c.band(0);
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
        let meter = f64::from(n.gain_reduction_db(0).get());
        assert!(
            (meter - reduction).abs() < 1e-3,
            "meter {meter} dB vs {reduction}"
        );
        // The DC's onset is a step through the crossover, which rings the
        // upper bands over their threshold for a moment; their 100 ms release
        // has taken all but a hair of it off by the end.
        for b in 1..4 {
            let held = n.gain_reduction_db(b);
            assert!(held < Db(1e-3), "band {b} still reduces {held:?}");
        }
        assert_eq!(n.gain_reduction_db(9), Db::UNITY);
    }

    /// Makeup is applied per band, after reduction, on every band.
    ///
    /// Two bands split at 1 kHz: DC is all band 0, and 10 kHz is
    /// `x^4 / (1 + x^4)` band 1 with `x` the warped ratio, the rest
    /// (`1e-4`) band 0. Band 0 -3 dB and band 1 +6 dB.
    #[test]
    fn makeup_applies_per_band() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).set_makeup(-3.0);
        c.band(1).set_makeup(6.0);
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
    #[test]
    fn bypass_passes_the_band_unprocessed() {
        let (n, c) = node(4);
        let b0 = c.band(0);
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
        assert_eq!(n.gain_reduction_db(0), Db::UNITY);
        b0.set_bypass(false);
        let out = render_chunked(&mut n, &[x], 512);
        // Over -30 dB at 8:1 the 0.5 DC (-6.02 dB) is reduced by
        // `(30 - 6.02) * 7/8`, and the 6 dB makeup rides on it.
        let reduction = (30.0 + 20.0 * libm::log10(0.5)) * 7.0 / 8.0;
        let want = 0.5 * libm::pow(10.0, (6.0 - reduction) / 20.0);
        let dc = mean(&out[0][SETTLE..]);
        assert!((dc - want).abs() < 5e-6 * want, "{dc} vs {want}");
        let meter = f64::from(n.gain_reduction_db(0).get());
        assert!((meter - reduction).abs() < 1e-3, "{meter} vs {reduction}");
    }

    /// Toggling bypass on a band reducing 12 dB steps no more than one ramp
    /// step per frame, both ways, and the compressor stays warm.
    ///
    /// DC 0.5 through band 0 at -30 dB, 2:1 is reduced by
    /// `(-6.02 + 30) / 2 = 11.99 dB` to 0.1253. Bypassing ramps the next
    /// 64-frame block up to 0.5 in 64 equal steps of `0.375 / 64`, so its first
    /// frame is within 1.5 steps of 0.1253; releasing it ramps back down to the
    /// *running* detector's gain (0.1253 again, to 1e-3: a detector reset by
    /// the bypass would end near 0.5).
    #[test]
    fn toggling_bypass_ramps_and_keeps_the_compressor_warm() {
        let (n, c) = node(2);
        let b0 = c.band(0);
        b0.set_threshold(-30.0);
        b0.set_ratio(2.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        let reduced = 0.5 * libm::pow(10.0, -(30.0 + 20.0 * libm::log10(0.5)) / 2.0 / 20.0);
        let step = (0.5 - reduced) / 64.0;

        b0.set_bypass(true);
        let up = render_chunked(&mut n, &[vec![0.5f32; 64]], 64).remove(0);
        assert!(
            (up[0] as f64 - reduced).abs() <= 1.5 * step,
            "first {}",
            up[0]
        );
        assert!((up[63] - 0.5).abs() < 1e-6, "last {}", up[63]);
        assert!(up.windows(2).all(|w| w[1] >= w[0]), "monotone rise");
        // Held bypassed for a while, then released.
        render_chunked(&mut n, &[vec![0.5f32; 4_800]], 512);
        assert_eq!(n.gain_reduction_db(0), Db::UNITY);
        b0.set_bypass(false);
        let down = render_chunked(&mut n, &[vec![0.5f32; 64]], 64).remove(0);
        assert!(
            (down[0] - 0.5).abs() as f64 <= 1.5 * step,
            "first {}",
            down[0]
        );
        assert!(
            (down[63] as f64 - reduced).abs() < 1e-3,
            "last {}",
            down[63]
        );
        assert!(down.windows(2).all(|w| w[1] <= w[0]), "monotone fall");
    }

    /// The meter holds the largest reduction of the block, not its last frame.
    ///
    /// DC 0.5 reduced 10.48 dB (-20 dB, 4:1), then a block of silence with a
    /// 1 ms release: band 0's level (the low-pass's slow tail) starts the
    /// block near 0.5 and the follower falls across it, so the first frames
    /// hold nearly the full reduction and the last does not.
    #[test]
    fn the_meter_holds_the_blocks_largest_reduction() {
        let (n, c) = node(2);
        let b0 = c.band(0);
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        render_chunked(&mut n, &[vec![0.0f32; 64]], 64);
        let last = n.bands[0].core.gain_reduction_db();
        let meter = n.gain_reduction_db(0);
        assert!(meter > Db(10.0), "{meter:?}");
        assert!(last < meter - Db(1.0), "last {last:?} vs meter {meter:?}");
    }

    /// Two soloed bands are both heard, and the bands being phase-aligned, their
    /// amplitudes add.
    ///
    /// Bands 1 and 2 of four at 3 kHz: band 1 is `HP(120) * LP(1 kHz)` (and a
    /// unit all-pass), band 2 `HP(120) * HP(1 kHz) * LP(6 kHz)`.
    #[test]
    fn several_solos_are_all_heard() {
        let (n, c) = node(4);
        unity(&c);
        c.band(1).set_solo(true);
        c.band(2).set_solo(true);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        let y = &out[0][SETTLE..];
        let hp = |fc: f64| {
            let x4 = warp(3_000.0, fc).powi(4);
            x4 / (1.0 + x4)
        };
        let lp = |fc: f64| 1.0 / (1.0 + warp(3_000.0, fc).powi(4));
        let want = 0.2 * hp(120.0) * (lp(1_000.0) + hp(1_000.0) * lp(6_000.0));
        let amp = amplitude_at(y, 3_000.0, FS);
        assert!((amp - want).abs() < 1e-6, "{amp} vs {want}");
        assert!(
            mean(y).abs() < 1e-6,
            "the DC (band 0) is heard: {}",
            mean(y)
        );
    }

    /// Releasing a solo brings the muted bands back across the block, rising.
    #[test]
    fn un_soloing_ramps_the_bands_back() {
        let (n, c) = node(2);
        unity(&c);
        c.band(1).set_solo(true);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        c.band(1).set_solo(false);
        let y = render_chunked(&mut n, &[vec![0.5f32; 64]], 64).remove(0);
        assert!(y[0].abs() <= 1.5 * 0.5 / 64.0, "first {}", y[0]);
        assert!((y[63] - 0.5).abs() < 1e-6, "last {}", y[63]);
        assert!(y.windows(2).all(|w| w[1] >= w[0]), "monotone rise");
    }

    /// A band both soloed and bypassed is heard unprocessed; bypassing a band
    /// another band's solo mutes leaves it muted.
    #[test]
    fn solo_and_bypass_combine() {
        let (n, c) = node(2);
        let b0 = c.band(0);
        b0.set_threshold(-30.0);
        b0.set_ratio(8.0);
        b0.set_makeup(6.0);
        b0.set_bypass(true);
        b0.set_solo(true);
        let mut n = prepared(n);
        let x = vec![0.5f32; SETTLE + WINDOW];
        let out = render_chunked(&mut n, std::slice::from_ref(&x), 512);
        assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < TOL);
        b0.set_solo(false);
        c.band(1).set_solo(true);
        let out = render_chunked(&mut n, &[x], 512);
        assert!(
            mean(&out[0][SETTLE..]).abs() < TOL,
            "{}",
            mean(&out[0][SETTLE..])
        );
    }

    /// A crossover above the prepare rate's limit does not freeze the others.
    ///
    /// Four bands at 22.05 kHz with the top crossover at 15 kHz (over
    /// Nyquist): it is held under the rate, and moving the lowest crossover
    /// still reaches the filter: a DC of 0.5 stays in band 0 whatever the split,
    /// so the check is on the filter's target.
    #[test]
    fn a_crossover_above_the_rate_does_not_freeze_the_others() {
        let (n, c) = node(4);
        c.set_crossover(2, Hz(15_000.0));
        let mut n = tutti_graph::contract::prepared(n, SampleRate(22_050.0), 1024);
        c.set_crossover(0, Hz(150.0));
        render_chunked(&mut n, &[vec![0.0f32; 64]], 64);
        let f = &n.filters[0];
        assert_eq!(f.frequency(0), Some(Hz(150.0)));
        let top = f.frequency(2).unwrap().get();
        assert!(
            top > 1_000.0 && f64::from(top) <= 0.499 * 22_050.0 + 1e-2,
            "{top}"
        );
    }

    /// A soloed band is all that is heard: the output is that band's
    /// analytic share of the input.
    ///
    /// Band 2 of four (1 kHz to 6 kHz) of a 3 kHz tone is
    /// `HP(120) * HP(1 kHz) * LP(6 kHz)`, each `x^4/(1+x^4)` or `1/(1+x^4)`
    /// with the warped ratio; a constant is in band 0 and must vanish.
    #[test]
    fn solo_plays_only_the_soloed_band() {
        let (n, c) = node(4);
        unity(&c);
        c.band(2).set_solo(true);
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
        c.band(2).set_solo(false);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < TOL);
    }

    /// Soloing mutes the other bands across the block, not at its edge.
    ///
    /// DC through band 0, then band 1 soloed: the next 64-frame block falls
    /// from the old level to 0 in 64 equal steps, so its first frame is
    /// within a step and a half of 0.5 and its last is 0.
    #[test]
    fn a_solo_ramps_the_mute() {
        let (n, c) = node(2);
        unity(&c);
        let mut n = prepared(n);
        let settle = vec![0.5f32; SETTLE];
        render_chunked(&mut n, &[settle], 512);
        c.band(1).set_solo(true);
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
    #[test]
    fn detection_is_linked_across_channels() {
        let n = MultibandCompressorNode::new(ChannelLayout::STEREO, 2).unwrap();
        let c = Ctl::new(&n);
        let b0 = c.band(0);
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
            assert_eq!(
                (n.bands(), n.clone().into_parts().controls.bands()),
                (bands, bands)
            );
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

    /// The defaults for each band count: the crossovers, and each band's
    /// settings.
    #[test]
    fn the_defaults_are_the_documented_ones() {
        let at = |c: &Ctl, kind, i| c.set.get(UnitAddr::new(kind, i));
        let (_, c) = node(4);
        let cross = [0, 1, 2, 3].map(|i| at(&c, UnitParam::Cutoff, i));
        assert_eq!(cross, [Some(120.0), Some(1_000.0), Some(6_000.0), None]);
        assert_eq!(at(&node(3).1, UnitParam::Cutoff, 1), Some(3_000.0));
        assert_eq!(at(&node(2).1, UnitParam::Cutoff, 0), Some(1_000.0));
        for b in 0..4 {
            let got = BAND_PARAMS.map(|k| at(&c, k, b));
            let want = [-18.0, 0.0, 2.0, 0.010, 0.100, 0.0].map(Some);
            assert_eq!(got, want, "band {b}");
            assert_eq!(at(&c, UnitParam::Bypass, b), Some(0.0));
            assert_eq!(at(&c, UnitParam::Solo, b), Some(0.0));
        }
        assert_eq!(at(&c, UnitParam::Threshold, 4), None);
    }

    /// A non-finite threshold, makeup or crossover written mid-stream reads as
    /// unchanged.
    ///
    /// The crossover cells have two guards (`LastGood` and `set_frequencies`'s own check),
    /// so either alone is enough by design.
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
            c.band(1).set_threshold(bad);
            c.band(1).set_makeup(bad);
            c.band(2).set_makeup(bad);
            c.band(2).set_knee(bad);
            c.band(2).set_ratio(bad);
            c.band(0).set_attack(bad);
            c.band(0).set_release(bad);
            c.set_crossover(0, Hz(bad));
            c.set_crossover(1, Hz(bad));
            assert!(finite(&mut n), "{bad}");
            c.band(1).set_threshold(-30.0);
            c.band(1).set_makeup(0.0);
            c.band(2).set_makeup(0.0);
            assert!(finite(&mut n), "{bad} then finite");
        }
    }

    /// A fork starts from the values set through every address and shares no
    /// cell with the live node, whatever the band.
    #[test]
    fn a_fork_shares_no_cell() {
        tutti_graph::contract::assert_param_fork(
            MultibandCompressorNode::new(ChannelLayout::STEREO, 4).unwrap(),
        );
    }

    /// A fork has its own meter: the live node's reads unity while the fork
    /// reduces.
    #[test]
    fn a_fork_has_its_own_meter() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-20.0);
        c.band(0).set_ratio(4.0);
        let live = n.clone();
        let mut fork = prepared(n.fork_fresh());
        render_chunked(&mut fork, &[vec![0.5f32; 4_096]], 512);
        assert!(fork.gain_reduction_db(0) > Db(5.0));
        assert_eq!(live.gain_reduction_db(0), Db::UNITY);
    }

    /// The fork the graph takes (`into_parts().fork`) renders from the values
    /// set when it was taken, not from later writes, and leaves the live
    /// node's meter alone.
    ///
    /// Band 0 at -20 dB, 4:1 reduces a DC of 0.5 by the analytic 10.48 dB.
    /// After the fork is taken the live node is bypassed and given +20 dB of
    /// makeup: a fork sharing either cell would not render the reduced level.
    #[test]
    fn the_graphs_fork_renders_from_the_values_it_took() {
        use tutti_graph::ForkMode;
        let (n, _) = node(2);
        let parts = n.into_parts();
        let live = Ctl {
            set: parts.controls.params().clone(),
            bands: parts.controls.bands(),
        };
        let b0 = live.band(0);
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        let forked = parts.fork.unwrap().fork(ForkMode::Offline).unwrap();
        b0.set_bypass(true);
        b0.set_makeup(20.0);
        b0.set_solo(false);
        let mut fork = forked.node;
        fork.prepare(&Prepare::new(RATE, tutti_types::Samples(1024)));
        let out = render_chunked(fork.as_mut(), &[vec![0.5f32; SETTLE]], 512);
        let reduction = (20.0 * libm::log10(0.5) + 20.0) * 0.75;
        let want = 0.5 * libm::pow(10.0, -reduction / 20.0);
        let dc = mean(&out[0][SETTLE - WINDOW..]);
        assert!((dc - want).abs() < 5e-6 * want, "{dc} vs {want}");
        assert_eq!(
            parts.controls.gain_reduction_db(0),
            Db::UNITY,
            "the fork wrote the live meter"
        );
    }

    /// The meters are reachable from the controls once inserted, and read
    /// what the node measured: the analytic 10.48 dB on band 0, nothing on
    /// the bands without content, unity past the last.
    #[test]
    fn the_controls_read_the_meters_after_insertion() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-20.0);
        c.band(0).set_ratio(4.0);
        c.band(0).set_attack(0.001);
        c.band(0).set_release(0.001);
        let mut parts = n.into_parts();
        parts
            .node
            .prepare(&Prepare::new(RATE, tutti_types::Samples(1024)));
        render_chunked(parts.node.as_mut(), &[vec![0.5f32; SETTLE]], 512);
        let reduction = (20.0 * libm::log10(0.5) + 20.0) * 0.75;
        let m = f64::from(parts.controls.gain_reduction_db(0).get());
        assert!((m - reduction).abs() < 1e-3, "{m} vs {reduction}");
        assert!(parts.controls.gain_reduction_db(1) < Db(1e-3));
        assert_eq!(parts.controls.gain_reduction_db(2), Db::UNITY);
    }

    /// Zero latency, `Tail::Unknown`, heavy, and one param port per band
    /// parameter then per crossover: 27 at four bands, within the graph's cap.
    #[test]
    fn the_shape_declares_the_indexed_ports() {
        let n = MultibandCompressorNode::new(ChannelLayout::STEREO, 4).unwrap();
        let s = n.shape();
        assert_eq!((s.audio_in().count(), s.audio_out().count()), (2, 2));
        assert_eq!(s.latency, tutti_types::Latency::default());
        assert_eq!(s.tail, Tail::Unknown);
        assert_eq!(s.cost, Cost::Heavy);
        assert_eq!(s.params.len(), 4 * 6 + 3);
        assert!(s.params.len() <= MAX_PARAM_PORTS);
        for (b, band) in s.params.as_slice().chunks(6).take(4).enumerate() {
            let want: Vec<_> = BAND_PARAMS
                .into_iter()
                .map(|k| UnitAddr::new(k, b as u8))
                .collect();
            assert_eq!(band, &want[..], "band {b}");
        }
        let tail: Vec<_> = (0..3)
            .map(|i| UnitAddr::new(UnitParam::Cutoff, i))
            .collect();
        assert_eq!(&s.params.as_slice()[24..], &tail[..]);
        let two = MultibandCompressorNode::new(ChannelLayout::MONO, 2).unwrap();
        assert_eq!(two.shape().params.len(), 2 * 6 + 1);
    }

    /// Port `k`'s base is the cell of the address the shape names for it,
    /// for every port of every band count, and the toggles have a cell but no
    /// port.
    #[test]
    fn each_port_reads_the_cell_of_its_address() {
        for bands in 2..=4 {
            let n = MultibandCompressorNode::new(ChannelLayout::MONO, bands).unwrap();
            let set = n.param_set();
            let ports = n.shape().params;
            for (k, &addr) in ports.as_slice().iter().enumerate() {
                let v = 100.0 + k as f32;
                assert!(set.set(addr, v));
                assert_eq!(
                    n.param_base(k),
                    Some(v),
                    "{bands} bands, port {k}: {addr:?}"
                );
            }
            assert_eq!(n.param_base(ports.len()), None);
            let cells: Vec<_> = set.params().collect();
            assert_eq!(cells.len(), ports.len() + 2 * bands);
            for b in 0..bands as u8 {
                for kind in [UnitParam::Bypass, UnitParam::Solo] {
                    let a = UnitAddr::new(kind, b);
                    assert!(cells.contains(&a) && !ports.as_slice().contains(&a));
                }
            }
        }
    }

    /// A write to one band's address reaches that band and no other: with
    /// the band-1 ratio at 1 the 10 kHz tone it carries is not reduced, with
    /// the band-0 ratio at 1 it still is.
    ///
    /// The tone (0.5, -6 dB) is over the default -18 dB threshold in band 1;
    /// band 0 sees 1e-4 of it, far under. A 2:1 band-1 reduction is about
    /// 6 dB; at ratio 1 the detector's target is exactly zero.
    #[test]
    fn a_write_to_band_1_reaches_band_1_only() {
        let (n, c) = node(2);
        for b in 0..2 {
            c.band(b).set_attack(0.001);
            c.band(b).set_release(0.001);
        }
        let mut n = prepared(n);
        let tone: Vec<f32> = sine(10_000.0, FS, 8_192)
            .into_iter()
            .map(|s| 0.5 * s)
            .collect();
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        assert!(
            n.gain_reduction_db(1) > Db(3.0),
            "{:?}",
            n.gain_reduction_db(1)
        );
        c.band(0).set_ratio(1.0);
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        assert!(
            n.gain_reduction_db(1) > Db(3.0),
            "band 0's write moved band 1"
        );
        c.band(1).set_ratio(1.0);
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        assert!(
            n.gain_reduction_db(1) < Db(1e-4),
            "{:?}",
            n.gain_reduction_db(1)
        );
    }

    /// The knee follows the static curve: DC 0.5 (-6.02 dB) over a -10 dB
    /// threshold, 4:1, 12 dB wide, is inside the knee and reduced by
    /// `slope * (x - (T - W / 2))^2`, `slope = (1 - 1/R) / (2W)`: 3.11 dB,
    /// where the hard knee gives 2.98 dB.
    #[test]
    fn the_knee_follows_the_static_curve() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-10.0);
        c.band(0).set_ratio(4.0);
        c.band(0).set_knee(12.0);
        c.band(0).set_attack(0.001);
        c.band(0).set_release(0.001);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[vec![0.5f32; SETTLE + WINDOW]], 512);
        let x = 20.0 * libm::log10(0.5) - (-10.0 - 6.0);
        let reduction = 0.75 / 24.0 * x * x;
        let want = 0.5 * libm::pow(10.0, -reduction / 20.0);
        let dc = mean(&out[0][SETTLE..]);
        assert!((dc - want).abs() < 5e-6 * want, "{dc} vs {want}");
    }

    /// A toggle is on at 0.5 and above, off below, and a NaN is off.
    ///
    /// DC 0.5 through band 0 at -30 dB, 8:1 reduces by `(-6.02 + 30) * 7/8`;
    /// bypassed it is not reduced at all.
    #[test]
    fn a_toggle_is_on_from_one_half() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-30.0);
        c.band(0).set_ratio(8.0);
        let mut n = prepared(n);
        let x = vec![0.5f32; 4_096];
        for (v, bypassed) in [(0.49, false), (0.5, true), (f32::NAN, false), (1.0, true)] {
            c.put(UnitParam::Bypass, 0, v);
            for _ in 0..4 {
                render_chunked(&mut n, std::slice::from_ref(&x), 512);
            }
            let reduced = n.gain_reduction_db(0) > Db(1.0);
            assert_eq!(!reduced, bypassed, "bypass at {v}");
        }
    }

    /// Raw writes are clamped as the node reads them: a ratio under 1 is 1
    /// (no expansion), negative times are 0.
    ///
    /// A band-0 threshold of -40 dB on a DC of 0.5 with a ratio written 0.25
    /// would expand by 100 dB if the ratio were taken raw; clamped, the DC
    /// passes untouched.
    #[test]
    fn raw_writes_are_clamped_as_the_node_reads_them() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-40.0);
        c.band(0).set_ratio(0.25);
        c.band(0).set_attack(-1.0);
        c.band(0).set_release(-1.0);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[vec![0.5f32; 4_096]], 512);
        assert!(out[0].iter().all(|s| s.is_finite()));
        assert_eq!(
            n.gain_reduction_db(0),
            Db::UNITY,
            "no expansion, no reduction"
        );
        assert!((mean(&out[0][3_000..]) - 0.5).abs() < 1e-5);
    }

    /// The crossover param moves the split: with band 0 soloed at unity, a
    /// 2 kHz tone is `1 / (1 + x^4)` of itself, `x` the warped ratio to the
    /// split, and at a 2 kHz split that is 0.5 (each branch -6.02 dB).
    #[test]
    fn a_crossover_param_moves_the_split() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).set_solo(true);
        let mut n = prepared(n);
        let before = gain_at(&mut n, 2_000.0);
        assert!((before - 1.0 / (1.0 + warp(2_000.0, 1_000.0).powi(4))).abs() < TOL);
        c.set_crossover(0, Hz(2_000.0));
        let after = gain_at(&mut n, 2_000.0);
        assert!((after - 0.5).abs() < TOL, "{after}");
    }

    /// Crossover writes that are out of order, not above 0 or not finite are
    /// refused as a whole and the last good set stands; a consistent set
    /// lands.
    #[test]
    fn a_bad_crossover_set_is_held_until_consistent() {
        let (n, c) = node(4);
        let mut n = prepared(n);
        let held = |n: &mut MultibandCompressorNode| {
            render_chunked(n, &[vec![0.0f32; 64]], 64);
            [0, 1, 2].map(|i| n.filters[0].frequency(i).unwrap().get())
        };
        for bad in [5_000.0, -1.0, 0.0, f32::NAN] {
            c.set_crossover(0, Hz(bad));
            assert_eq!(held(&mut n), [120.0, 1_000.0, 6_000.0], "{bad}");
        }
        // Written top-down with a block between: the set stays out of order
        // (`[0, 1000, 9000]`, then `[0, 8000, 9000]`) until the last write.
        c.set_crossover(2, Hz(9_000.0));
        assert_eq!(held(&mut n), [120.0, 1_000.0, 6_000.0]);
        c.set_crossover(1, Hz(8_000.0));
        assert_eq!(held(&mut n), [120.0, 1_000.0, 6_000.0]);
        c.set_crossover(0, Hz(5_000.0));
        assert_eq!(held(&mut n), [5_000.0, 8_000.0, 9_000.0]);
    }

    /// The typed toggles write the band's own cell and read it back; past the
    /// last band they write nothing and read false.
    #[test]
    fn the_controls_toggle_bands_by_type() {
        let (n, _) = node(3);
        let c = n.into_parts().controls;
        assert!(c.set_bypass(1, true) && c.set_solo(2, true));
        assert!(c.is_bypassed(1) && !c.is_bypassed(0) && !c.is_bypassed(2));
        assert!(c.is_soloed(2) && !c.is_soloed(1));
        assert_eq!(
            c.params().get(UnitAddr::new(UnitParam::Bypass, 1)),
            Some(1.0)
        );
        assert!(!c.set_bypass(3, true) && !c.is_bypassed(3) && !c.set_solo(300, true));
        c.params().set(UnitAddr::new(UnitParam::Bypass, 0), 0.5);
        assert!(c.is_bypassed(0));
        assert!(c.set_bypass(1, false) && !c.is_bypassed(1));
    }

    /// A driven make-up port moves its own band: band 1 alone heard (band 0
    /// soloed out), a 10 kHz tone at unity ratio is `x^4 / (1 + x^4)` of
    /// itself through the high-pass, times the driven +6 dB; the cell stays
    /// 0 dB.
    #[test]
    fn a_driven_makeup_port_moves_its_band() {
        let (n, c) = node(2);
        unity(&c);
        c.band(1).set_solo(true);
        let mut n = prepared(n);
        let port = PER_BAND + 5;
        let x = sine(10_000.0, FS, SETTLE + WINDOW);
        let lane = [6.0f32; 512];
        let mut params: Vec<Option<&[f32]>> = vec![None; port + 1];
        let mut y = Vec::new();
        for chunk in x.chunks(512) {
            params[port] = Some(&lane[..chunk.len()]);
            y.extend_from_slice(&drive(&mut n, RATE, &[chunk], &params)[0]);
        }
        let x4 = warp(10_000.0, 1_000.0).powi(4);
        let want = x4 / (1.0 + x4) * libm::pow(10.0, 6.0 / 20.0);
        let amp = amplitude_at(&y[SETTLE..], 10_000.0, FS);
        assert!((amp - want).abs() < 1e-5, "{amp} vs {want}");
    }

    /// A driven crossover port moves its crossover: port `bands * 6` is
    /// crossover 0, and a driven 2 kHz leaves a 2 kHz tone at half amplitude
    /// in band 0 (soloed).
    #[test]
    fn a_driven_crossover_port_moves_the_split() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).set_solo(true);
        let mut n = prepared(n);
        let port = 2 * PER_BAND;
        let x = sine(2_000.0, FS, SETTLE + WINDOW);
        let lane = [2_000.0f32; 512];
        let mut params: Vec<Option<&[f32]>> = vec![None; port + 1];
        let mut y = Vec::new();
        for chunk in x.chunks(512) {
            params[port] = Some(&lane[..chunk.len()]);
            y.extend_from_slice(&drive(&mut n, RATE, &[chunk], &params)[0]);
        }
        let amp = amplitude_at(&y[SETTLE..], 2_000.0, FS);
        assert!((amp - 0.5).abs() < TOL, "{amp}");
    }

    /// A one-frame block and a full 1024-frame block both render.
    #[test]
    fn any_block_up_to_the_prepared_maximum_renders() {
        let (n, c) = node(4);
        c.set_crossover(0, Hz(200.0));
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
