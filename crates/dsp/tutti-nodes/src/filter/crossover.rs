//! Linkwitz-Riley 4th-order crossover: [`Crossover`] and [`CrossoverNode`].
//!
//! Each split is a low-pass and a high-pass of two cascaded 2nd-order
//! Butterworth sections (Q = 1/sqrt(2)) at the crossover frequency
//! (Linkwitz and Riley, 1976). The two branches are -6.02 dB and in phase at
//! the crossover, so they sum to unity magnitude: the sum is the 2nd-order
//! Butterworth all-pass of that frequency.
//!
//! N bands are N-1 splits in a tree: split 0 divides the input into band 0
//! and the rest, split 1 divides the rest into band 1 and the rest, and so
//! on. A band split off before the last crossover has not been through the
//! later all-passes its siblings share, so it runs through the all-pass of
//! each later crossover. Every band then has the same phase response: for a
//! crossover with denominator `D`, `LP^2`, `HP^2` and the all-pass all have
//! phase `-2 * arg D`. The bands are phase-aligned at every frequency, so
//! they sum without cancelling, and the sum has flat magnitude.
//!
//! The filters are IIR, so the response is **not linear-phase**: the shared
//! phase rotates with frequency (the group delay is not constant), and
//! the sum is an all-pass, not a delayed copy of the input. They add no
//! latency.

use core::f64::consts::{FRAC_1_SQRT_2, PI};
use std::fmt;

use tutti_core::{Hz, Param, SampleRate};
use tutti_graph::{Cost, Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, Tail};

use crate::fresh::{Fresh, fresh_parts};
use crate::ramp::{COEFF_INTERVAL, LastGood};

/// Most splits a [`Crossover`] holds: one fewer than [`Crossover::MAX_BANDS`].
const MAX_SPLITS: usize = 5;

/// Highest frequency the coefficients are solved at, as a fraction of the
/// sample rate. `tan` diverges at Nyquist; a frequency a rate change leaves
/// above this is solved here instead.
const SOLVE_CEILING: f64 = 0.499;

/// Why a [`Crossover`] or crossover-based node refused its settings.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum CrossoverError {
    /// The band count is outside the supported range.
    Bands {
        /// The count asked for.
        bands: usize,
        /// The fewest supported.
        min: usize,
        /// The most supported.
        max: usize,
    },
    /// A whole set of frequencies has the wrong length.
    Count {
        /// How many were given.
        got: usize,
        /// How many there are crossovers.
        expected: usize,
    },
    /// The channel count is zero, or more channels than the graph has ports
    /// for.
    Channels {
        /// The count asked for.
        channels: usize,
    },
    /// The sample rate is not finite and positive.
    SampleRate,
    /// A frequency is not finite, or not above 0 Hz.
    Frequency {
        /// Which crossover.
        index: usize,
        /// The value given.
        hz: Hz,
    },
    /// A frequency is not above the one before it.
    Order {
        /// Which crossover.
        index: usize,
    },
    /// A frequency is not below Nyquist.
    Nyquist {
        /// Which crossover.
        index: usize,
        /// Nyquist at the rate given.
        nyquist: Hz,
    },
    /// No crossover has this index.
    Index {
        /// The index given.
        index: usize,
        /// How many crossovers there are.
        len: usize,
    },
}

impl fmt::Display for CrossoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Bands { bands, min, max } => {
                write!(f, "{bands} bands: a crossover has {min} to {max}")
            }
            Self::Count { got, expected } => {
                write!(f, "{got} frequencies given for {expected} crossovers")
            }
            Self::Channels { channels } => write!(f, "{channels} channels is not supported"),
            Self::SampleRate => f.write_str("the sample rate must be finite and positive"),
            Self::Frequency { index, hz } => write!(
                f,
                "crossover {index} is {} Hz: it must be finite and above 0",
                hz.get()
            ),
            Self::Order { index } => write!(
                f,
                "crossover {index} is not above crossover {}: they must strictly increase",
                index.saturating_sub(1)
            ),
            Self::Nyquist { index, nyquist } => write!(
                f,
                "crossover {index} is not below Nyquist ({} Hz)",
                nyquist.get()
            ),
            Self::Index { index, len } => {
                write!(f, "no crossover {index}: there are {len}")
            }
        }
    }
}

impl std::error::Error for CrossoverError {}

/// One biquad's coefficients, normalised so `a0 = 1`.
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

/// The three biquads one split needs, from one warped frequency.
#[derive(Clone, Copy, Debug)]
struct Split {
    low: Biquad,
    high: Biquad,
    all_pass: Biquad,
}

impl Split {
    /// The Butterworth (Q = 1/sqrt(2)) low-pass, high-pass and all-pass at
    /// `hz` against `rate` (bilinear transform).
    fn solve(hz: f64, rate: f64) -> Self {
        let hz = hz.min(SOLVE_CEILING * rate);
        let k = libm::tan(PI * hz / rate);
        let k2 = k * k;
        let norm = 1.0 / (1.0 + k / FRAC_1_SQRT_2 + k2);
        let a1 = 2.0 * (k2 - 1.0) * norm;
        let a2 = (1.0 - k / FRAC_1_SQRT_2 + k2) * norm;
        let low = k2 * norm;
        Self {
            low: Biquad {
                b0: low,
                b1: 2.0 * low,
                b2: low,
                a1,
                a2,
            },
            high: Biquad {
                b0: norm,
                b1: -2.0 * norm,
                b2: norm,
                a1,
                a2,
            },
            all_pass: Biquad {
                b0: a2,
                b1: a1,
                b2: 1.0,
                a1,
                a2,
            },
        }
    }
}

/// A biquad's two state words (transposed direct form II).
#[derive(Clone, Copy, Debug, Default)]
struct State([f64; 2]);

impl State {
    #[inline]
    fn tick(&mut self, c: &Biquad, x: f64) -> f64 {
        let y = c.b0 * x + self.0[0];
        self.0[0] = c.b1 * x - c.a1 * y + self.0[1];
        self.0[1] = c.b2 * x - c.a2 * y;
        y
    }
}

/// A Linkwitz-Riley 4th-order crossover of 2 to 6 bands over one channel.
///
/// State and coefficients are `f64`; samples in and out are `f32`. A
/// crossover frequency change ([`set_frequency`](Self::set_frequency)) is
/// ramped across the next [`split_block`](Self::split_block), with the
/// coefficients re-solved every few samples from the interpolated frequency,
/// so it does not click.
///
/// A multichannel signal needs one `Crossover` per channel, all set to the
/// same frequencies.
///
/// ```
/// use tutti_nodes::{Crossover, Hz};
///
/// let mut xo = Crossover::new(&[Hz(200.0), Hz(2_000.0)], 48_000.0).unwrap();
/// assert_eq!(xo.bands(), 3);
/// // The sum of the bands is an all-pass of the input: a constant 1.0
/// // settles to a sum of 1.0 (unity gain at DC).
/// let mut sum = 0.0;
/// for _ in 0..48_000 {
///     sum = xo.split(1.0)[..3].iter().sum::<f32>();
/// }
/// assert!((sum - 1.0).abs() < 1e-5);
/// ```
#[derive(Clone, Debug)]
pub struct Crossover {
    rate: f64,
    bands: usize,
    /// The frequencies the coefficients are solved at now.
    current: [f64; MAX_SPLITS],
    /// The frequencies set; `current` ramps to them.
    target: [f64; MAX_SPLITS],
    coeffs: [Split; MAX_SPLITS],
    /// Per split, the two cascaded sections of its low-pass and high-pass.
    low: [[State; 2]; MAX_SPLITS],
    high: [[State; 2]; MAX_SPLITS],
    /// `all_pass[j][k]`: band `j`'s all-pass for the later split `k > j`.
    all_pass: [[State; MAX_SPLITS]; MAX_SPLITS],
}

impl Crossover {
    /// The fewest bands.
    pub const MIN_BANDS: usize = 2;
    /// The most bands.
    pub const MAX_BANDS: usize = MAX_SPLITS + 1;

    /// A crossover with one split at each of `frequencies`, so
    /// `frequencies.len() + 1` bands, at `rate`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if the band count is not in
    /// [`MIN_BANDS`](Self::MIN_BANDS)`..=`[`MAX_BANDS`](Self::MAX_BANDS), the
    /// rate is not finite and positive, or the frequencies are not finite,
    /// above 0, strictly increasing and below Nyquist.
    pub fn new(frequencies: &[Hz], rate: impl Into<SampleRate>) -> Result<Self, CrossoverError> {
        let rate = rate.into();
        validate(frequencies, rate)?;
        let mut xo = Self {
            rate: rate.get(),
            bands: frequencies.len() + 1,
            current: [0.0; MAX_SPLITS],
            target: [0.0; MAX_SPLITS],
            coeffs: [Split::solve(1_000.0, 48_000.0); MAX_SPLITS],
            low: [[State::default(); 2]; MAX_SPLITS],
            high: [[State::default(); 2]; MAX_SPLITS],
            all_pass: [[State::default(); MAX_SPLITS]; MAX_SPLITS],
        };
        for (j, f) in frequencies.iter().enumerate() {
            xo.target[j] = f64::from(f.get());
        }
        xo.settle();
        Ok(xo)
    }

    /// How many bands [`split`](Self::split) produces.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Crossover `i`'s frequency as last set (the ramp's target), or `None`
    /// past the last.
    #[must_use]
    pub fn frequency(&self, i: usize) -> Option<Hz> {
        (i < self.splits()).then(|| Hz(self.target[i] as f32))
    }

    /// The sample rate the coefficients are solved at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.rate)
    }

    fn splits(&self) -> usize {
        self.bands - 1
    }

    /// Moves crossover `i` to `hz`: ramped across the next
    /// [`split_block`](Self::split_block), or taken whole by the next
    /// [`split`](Self::split).
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if `i` is past the last crossover, or `hz` is not
    /// finite, above 0, above the crossover before it, below the one after
    /// it and below Nyquist. Nothing changes then.
    pub fn set_frequency(&mut self, i: usize, hz: Hz) -> Result<(), CrossoverError> {
        let n = self.splits();
        if i >= n {
            return Err(CrossoverError::Index { index: i, len: n });
        }
        check_one(i, hz, &self.target[..n], SampleRate(self.rate))?;
        self.target[i] = f64::from(hz.get());
        Ok(())
    }

    /// Moves every crossover at once, as [`set_frequency`](Self::set_frequency)
    /// moves one. `frequencies` is all of them: its length must be
    /// [`bands`](Self::bands) `- 1`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if the length differs, or the frequencies are not
    /// finite, above 0, strictly increasing and below Nyquist. Nothing
    /// changes then.
    pub fn set_frequencies(&mut self, frequencies: &[Hz]) -> Result<(), CrossoverError> {
        if frequencies.len() != self.splits() {
            return Err(CrossoverError::Count {
                got: frequencies.len(),
                expected: self.splits(),
            });
        }
        validate(frequencies, SampleRate(self.rate))?;
        for (t, f) in self.target.iter_mut().zip(frequencies) {
            *t = f64::from(f.get());
        }
        Ok(())
    }

    /// Re-solves for `rate`, keeping the filter state and the frequencies. A
    /// rate that is not finite and positive changes nothing. A frequency at
    /// or above the new Nyquist is solved just under it.
    pub fn set_sample_rate(&mut self, rate: impl Into<SampleRate>) {
        let rate = rate.into().get();
        if rate.is_finite() && rate > 0.0 {
            self.rate = rate;
            self.settle();
        }
    }

    /// Clears the filter state, and takes any pending frequency change whole.
    pub fn reset(&mut self) {
        self.low = [[State::default(); 2]; MAX_SPLITS];
        self.high = [[State::default(); 2]; MAX_SPLITS];
        self.all_pass = [[State::default(); MAX_SPLITS]; MAX_SPLITS];
        self.settle();
    }

    /// Splits one sample into [`bands`](Self::bands) bands, in the first
    /// entries of the result (the rest are 0), lowest band first. A pending
    /// frequency change lands whole on this sample: for a ramp, use
    /// [`split_block`](Self::split_block).
    #[inline]
    pub fn split(&mut self, x: f32) -> [f32; Self::MAX_BANDS] {
        if self.current != self.target {
            self.settle();
        }
        self.tick(x)
    }

    /// Splits `input` into [`bands`](Self::bands) bands: band `b` is written
    /// to `out[b * input.len()..][..input.len()]`. A pending frequency change
    /// ramps across the block, the last frame landing exactly on it.
    ///
    /// # Panics
    ///
    /// If `out.len()` is not `bands() * input.len()`.
    pub fn split_block(&mut self, input: &[f32], out: &mut [f32]) {
        let n = input.len();
        assert_eq!(
            out.len(),
            self.bands * n,
            "split_block: out holds one block per band"
        );
        let from = self.current;
        let mut done = 0;
        while done < n {
            let len = if self.current == self.target {
                n - done
            } else {
                COEFF_INTERVAL.min(n - done)
            };
            if self.current != self.target {
                let end = done + len;
                let splits = self.splits();
                let ramps = self.current.iter_mut().zip(&self.target).zip(&from);
                for ((cur, &to), &from) in ramps.take(splits) {
                    *cur = if end >= n {
                        to
                    } else {
                        from + (to - from) * (end as f64 / n as f64)
                    };
                }
                self.solve();
            }
            for i in done..done + len {
                let bands = self.tick(input[i]);
                for (b, y) in bands.iter().take(self.bands).enumerate() {
                    out[b * n + i] = *y;
                }
            }
            done += len;
        }
    }

    /// Takes the targets whole.
    fn settle(&mut self) {
        self.current = self.target;
        self.solve();
    }

    fn solve(&mut self) {
        for j in 0..self.splits() {
            self.coeffs[j] = Split::solve(self.current[j], self.rate);
        }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> [f32; Self::MAX_BANDS] {
        let mut out = [0.0f32; Self::MAX_BANDS];
        let splits = self.splits();
        let mut rest = f64::from(x);
        for (j, slot) in out.iter_mut().enumerate().take(splits) {
            let c = &self.coeffs[j];
            let mut lo = rest;
            lo = self.low[j][0].tick(&c.low, lo);
            lo = self.low[j][1].tick(&c.low, lo);
            let mut hi = rest;
            hi = self.high[j][0].tick(&c.high, hi);
            hi = self.high[j][1].tick(&c.high, hi);
            for k in j + 1..splits {
                lo = self.all_pass[j][k].tick(&self.coeffs[k].all_pass, lo);
            }
            *slot = lo as f32;
            rest = hi;
        }
        out[splits] = rest as f32;
        out
    }
}

/// Checks a whole set: count, then each frequency in order.
fn validate(frequencies: &[Hz], rate: SampleRate) -> Result<(), CrossoverError> {
    let bands = frequencies.len() + 1;
    if !(Crossover::MIN_BANDS..=Crossover::MAX_BANDS).contains(&bands) {
        return Err(CrossoverError::Bands {
            bands,
            min: Crossover::MIN_BANDS,
            max: Crossover::MAX_BANDS,
        });
    }
    if !(rate.get().is_finite() && rate.get() > 0.0) {
        return Err(CrossoverError::SampleRate);
    }
    let nyquist = rate.nyquist();
    for (index, &hz) in frequencies.iter().enumerate() {
        if !(hz.get().is_finite() && hz.get() > 0.0) {
            return Err(CrossoverError::Frequency { index, hz });
        }
        if index > 0 && hz <= frequencies[index - 1] {
            return Err(CrossoverError::Order { index });
        }
        if hz >= nyquist {
            return Err(CrossoverError::Nyquist { index, nyquist });
        }
    }
    Ok(())
}

/// Checks `hz` as crossover `i` against the others' `targets`, and Nyquist.
fn check_one(i: usize, hz: Hz, targets: &[f64], rate: SampleRate) -> Result<(), CrossoverError> {
    // Every target came from an `Hz`, so narrowing it back is exact.
    check_neighbours(i, hz, |k| Hz(targets[k] as f32), targets.len())?;
    let nyquist = rate.nyquist();
    if hz >= nyquist {
        return Err(CrossoverError::Nyquist { index: i, nyquist });
    }
    Ok(())
}

/// The handles on a [`CrossoverNode`]'s frequencies. Clones share the cells.
#[derive(Clone, Debug)]
pub struct CrossoverControls {
    frequencies: Vec<Param<Hz>>,
}

impl CrossoverControls {
    /// How many crossover frequencies there are (`bands - 1`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.frequencies.len()
    }

    /// Whether there are none. Never true: a crossover has at least one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frequencies.is_empty()
    }

    /// Crossover `i`'s frequency, or `None` past the last.
    #[must_use]
    pub fn frequency(&self, i: usize) -> Option<Hz> {
        self.frequencies.get(i).map(Param::load)
    }

    /// Moves crossover `i` to `hz`; the node ramps to it across its next block.
    ///
    /// Checked against the neighbours' values now, and against Nyquist by the
    /// node (a value it cannot use reads as unchanged).
    ///
    /// # Errors
    ///
    /// [`CrossoverError::Index`] past the last crossover,
    /// [`CrossoverError::Frequency`] if `hz` is not finite and above 0,
    /// [`CrossoverError::Order`] if it is not between its neighbours. Nothing
    /// is written then.
    pub fn set_frequency(&self, i: usize, hz: Hz) -> Result<(), CrossoverError> {
        let cell = self.frequencies.get(i).ok_or(CrossoverError::Index {
            index: i,
            len: self.frequencies.len(),
        })?;
        check_neighbours(
            i,
            hz,
            |k| self.frequencies[k].load(),
            self.frequencies.len(),
        )?;
        cell.store(hz);
        Ok(())
    }
}

/// Clamps each of `frequencies` under what the coefficients can be solved at
/// for `rate`, keeping them strictly increasing: index `i` of `n` is capped at
/// `SOLVE_CEILING * rate * (1 - 0.01 * (n - 1 - i))`, a strictly increasing
/// cap, so the minimum of it and a strictly increasing set still is. A
/// frequency out of range costs only itself, not the whole set.
pub(crate) fn fit_to_rate(frequencies: &mut [Hz], rate: SampleRate) {
    let n = frequencies.len();
    for (i, f) in frequencies.iter_mut().enumerate() {
        let cap = SOLVE_CEILING * rate.get() * (1.0 - 0.01 * (n - 1 - i) as f64);
        *f = Hz(f64::from(f.get()).min(cap) as f32);
    }
}

/// [`CrossoverControls::set_frequency`]'s checks, shared with the multiband
/// node's: finite, above 0, between the neighbours `at(i - 1)` and `at(i + 1)`.
pub(crate) fn check_neighbours(
    i: usize,
    hz: Hz,
    at: impl Fn(usize) -> Hz,
    len: usize,
) -> Result<(), CrossoverError> {
    if !(hz.get().is_finite() && hz.get() > 0.0) {
        return Err(CrossoverError::Frequency { index: i, hz });
    }
    if i > 0 && hz <= at(i - 1) {
        return Err(CrossoverError::Order { index: i });
    }
    if i + 1 < len && hz >= at(i + 1) {
        return Err(CrossoverError::Order { index: i + 1 });
    }
    Ok(())
}

/// A graph node splitting its input into bands with a [`Crossover`] per
/// channel.
///
/// `channels` inputs, `bands * channels` outputs, band-major: output
/// `band * channels + channel`. That layout is fixed, and `bands * channels`
/// is at most the graph's 64 ports ([`CrossoverNode::new`] refuses more). The
/// bands are not delayed and the node adds no
/// latency; the sum of the bands has flat magnitude and the phase of the
/// crossovers' all-pass (see [`Crossover`] for why, and for what
/// "not linear-phase" means for the bands).
///
/// Its controls ([`CrossoverControls`]) move the crossover frequencies, read
/// once per block and ramped across it. A frequency at or above what the rate
/// allows is held just under it (about 0.499 of the rate) without disturbing
/// the others; a set out of order after a write race reads as unchanged.
pub struct CrossoverNode {
    layout: ChannelLayout,
    bands: usize,
    frequencies: Vec<Param<Hz>>,
    good: Vec<LastGood>,
    filters: Vec<Crossover>,
    /// One channel's bands, as [`Crossover::split_block`] writes them.
    scratch: Vec<f32>,
}

impl fmt::Debug for CrossoverNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrossoverNode")
            .field("layout", &self.layout)
            .field("bands", &self.bands)
            .finish_non_exhaustive()
    }
}

impl CrossoverNode {
    /// A crossover over `layout` with one split at each of `frequencies`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if the frequencies are invalid at 48 kHz (see
    /// [`Crossover::new`]), or `layout` has no channels or `bands * channels`
    /// exceeds the graph's 64 ports.
    pub fn new(layout: ChannelLayout, frequencies: &[Hz]) -> Result<Self, CrossoverError> {
        let proto = Crossover::new(frequencies, SampleRate::DEFAULT)?;
        let channels = usize::from(layout.count());
        if channels == 0 || channels * proto.bands() > tutti_graph::MAX_PORTS {
            return Err(CrossoverError::Channels { channels });
        }
        Ok(Self {
            layout,
            bands: proto.bands(),
            frequencies: frequencies.iter().map(|&f| Param::new(f)).collect(),
            good: frequencies.iter().map(|f| LastGood::new(f.get())).collect(),
            filters: vec![proto; channels],
            scratch: Vec::new(),
        })
    }

    /// The frequency cells read once, held to the last finite value and
    /// fitted to `rate` ([`fit_to_rate`]).
    fn read_targets(&mut self, rate: SampleRate) -> [Hz; MAX_SPLITS] {
        let mut targets = [Hz(0.0); MAX_SPLITS];
        let n = self.bands - 1;
        for ((t, cell), good) in targets
            .iter_mut()
            .zip(&self.frequencies)
            .zip(&mut self.good)
        {
            *t = Hz(good.read(cell.load().get()));
        }
        fit_to_rate(&mut targets[..n], rate);
        targets
    }

    /// The handles on the frequencies.
    #[must_use]
    pub fn controls(&self) -> CrossoverControls {
        CrossoverControls {
            frequencies: self.frequencies.clone(),
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
        self.layout
    }
}

impl Node for CrossoverNode {
    /// `channels` in, `bands * channels` out.
    ///
    /// Zero latency. The tail is [`Tail::Unknown`], as the other IIR filters'
    /// is: the sections ring after the input stops.
    fn shape(&self) -> Shape {
        let ch = self.layout.count();
        Shape::audio(
            self.layout,
            ChannelLayout::from_count(ch * self.bands as u16),
        )
        .with_cost(Cost::Moderate)
        .with_tail(Tail::Unknown)
    }

    fn prepare(&mut self, p: &Prepare) {
        let targets = self.read_targets(p.sample_rate());
        for f in &mut self.filters {
            f.set_sample_rate(p.sample_rate());
            // Taken whole by the `reset`, so the first block does not ramp
            // in from the constructor's values.
            let _ = f.set_frequencies(&targets[..self.bands - 1]);
            f.reset();
        }
        self.scratch = vec![0.0; self.bands * p.max_block().get()];
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let ch = usize::from(self.layout.count());
        let splits = self.bands - 1;
        let targets = self.read_targets(self.filters[0].sample_rate());
        let (inputs, mut outputs) = io.split();
        for (c, filter) in self.filters.iter_mut().enumerate() {
            // `read_targets` fitted every entry to the rate, so only a set
            // out of order (a write race between two cells) is refused, and
            // it keeps the last one.
            let _ = filter.set_frequencies(&targets[..splits]);
            let out = &mut self.scratch[..self.bands * n];
            filter.split_block(&inputs.get(c)[..n], out);
            for b in 0..self.bands {
                outputs.get(b * ch + c)[..n].copy_from_slice(&out[b * n..(b + 1) * n]);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        for f in &mut self.filters {
            f.reset();
        }
    }
}

impl Clone for CrossoverNode {
    /// Shares the frequency cells (the template a fork is taken from); the
    /// filter state is copied.
    fn clone(&self) -> Self {
        Self {
            layout: self.layout,
            bands: self.bands,
            frequencies: self.frequencies.clone(),
            good: self.good.clone(),
            filters: self.filters.clone(),
            scratch: self.scratch.clone(),
        }
    }
}

impl Fresh for CrossoverNode {
    fn fresh(&self) -> Self {
        let mut fork = self.clone();
        for f in &mut fork.frequencies {
            f.detach();
        }
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`CrossoverControls`] as its handles; a fork starts from
/// the frequencies last set.
impl IntoNode for CrossoverNode {
    type Controls = CrossoverControls;

    fn into_parts(self) -> NodeParts<CrossoverControls> {
        let controls = self.controls();
        fresh_parts(self, controls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{amplitude_at, lock_in, sine};
    use crate::test_support::{prepared, render_chunked};
    use tutti_graph::Node;
    use tutti_types::Tail;

    const RATE: f64 = 48_000.0;
    /// A whole number of periods of every tone on the 5 Hz grid.
    const WINDOW: usize = 9_600;
    const SETTLE: usize = 12_000;
    /// f32 rounding of the input, each band, and the sum: at most
    /// `(bands + 2)` half-ulps of the peak, each `2^-24`, which the lock-in
    /// only averages down.
    const TOL: f64 = 1e-6;

    /// The 5 Hz-grid frequencies of a log sweep from 20 Hz to 16 kHz.
    fn sweep() -> Vec<f64> {
        (0..48)
            .map(|i| {
                let f = 20.0 * (16_000.0f64 / 20.0).powf(i as f64 / 47.0);
                (f / 5.0).round() * 5.0
            })
            .collect()
    }

    /// Band `b`'s phasor and the bands' summed amplitude for a unit tone at
    /// `hz`, in steady state.
    fn bands_at(freqs: &[f32], hz: f64) -> (Vec<(f64, f64)>, f64) {
        let freqs: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
        let mut xo = Crossover::new(&freqs, RATE).unwrap();
        let x = sine(hz, RATE, SETTLE + WINDOW);
        let nb = xo.bands();
        let mut bands = vec![Vec::with_capacity(WINDOW); nb];
        let mut sum = Vec::with_capacity(WINDOW);
        for (i, &s) in x.iter().enumerate() {
            let out = xo.split(s);
            if i >= SETTLE {
                for (b, v) in bands.iter_mut().zip(out) {
                    b.push(v);
                }
                sum.push(out[..nb].iter().sum::<f32>());
            }
        }
        (
            bands.iter().map(|b| lock_in(b, hz, RATE)).collect(),
            amplitude_at(&sum, hz, RATE),
        )
    }

    fn mag(z: (f64, f64)) -> f64 {
        z.0.hypot(z.1)
    }

    fn assert_flat(freqs: &[f32]) {
        for hz in sweep() {
            let (_, sum) = bands_at(freqs, hz);
            assert!(
                (sum - 1.0).abs() < TOL,
                "{} bands at {hz} Hz: the sum has amplitude {sum}",
                freqs.len() + 1
            );
        }
    }

    /// The two branches sum to unity magnitude across the audio band.
    ///
    /// Mutation (run): one Butterworth section per branch instead of two (the
    /// second `tick` dropped for `low` and `high`) → the sum dips to 0 at the
    /// crossover → fails.
    #[test]
    fn two_bands_sum_to_flat_magnitude() {
        assert_flat(&[1_000.0]);
    }

    /// With the all-pass compensation, three and four bands sum flat too.
    ///
    /// Mutation (run): drop the `all_pass` loop in `tick` → the sum is no
    /// longer flat at the upper crossovers → fails both.
    #[test]
    fn three_and_four_bands_sum_to_flat_magnitude() {
        assert_flat(&[120.0, 2_500.0]);
        assert_flat(&[120.0, 1_000.0, 6_000.0]);
    }

    /// Six bands, the most, sum flat.
    ///
    /// Mutation (run): drop the `all_pass` loop in `tick` → the upper
    /// crossovers' sums dip → fails; one Butterworth section per branch →
    /// fails.
    #[test]
    fn six_bands_sum_to_flat_magnitude() {
        assert_flat(&[100.0, 300.0, 1_000.0, 3_000.0, 9_000.0]);
    }

    /// Each branch is 6.02 dB down at the crossover (half amplitude) and the
    /// two are in phase there.
    ///
    /// Mutation (run): Q of 0.5 instead of `FRAC_1_SQRT_2` in `Split::solve`
    /// → fails (with the flat and curve tests); one section per branch →
    /// 3.01 dB down and 90 degrees apart → fails.
    #[test]
    fn each_branch_is_6_02_db_down_and_in_phase_at_the_crossover() {
        let (bands, _) = bands_at(&[1_000.0], 1_000.0);
        let (low, high) = (bands[0], bands[1]);
        assert!((mag(low) - 0.5).abs() < TOL, "low {}", mag(low));
        assert!((mag(high) - 0.5).abs() < TOL, "high {}", mag(high));
        // z_low / z_high = 1 when equal in magnitude and phase.
        let d = low.0 * high.0 + low.1 * high.1;
        let c = low.1 * high.0 - low.0 * high.1;
        let norm = high.0 * high.0 + high.1 * high.1;
        assert!((d / norm - 1.0).abs() < 4.0 * TOL, "ratio re {}", d / norm);
        assert!((c / norm).abs() < 4.0 * TOL, "ratio im {}", c / norm);
    }

    /// Each branch's magnitude is the analytic 4th-order one: low
    /// `1 / (1 + x^4)`, high `x^4 / (1 + x^4)`, with `x` the frequency ratio
    /// as the bilinear transform warps it, `tan(pi f / fs) / tan(pi fc / fs)`.
    ///
    /// Mutation (run): Q of 0.5 in `Split::solve` → the magnitudes leave the
    /// curve → fails.
    #[test]
    fn branch_magnitudes_are_the_butterworth_squared_curve() {
        for hz in [100.0, 500.0, 800.0, 1_000.0, 1_250.0, 2_000.0, 8_000.0] {
            let (bands, _) = bands_at(&[1_000.0], hz);
            let warp = |f: f64| libm::tan(core::f64::consts::PI * f / RATE);
            let x4 = (warp(hz) / warp(1_000.0)).powi(4);
            assert!(
                (mag(bands[0]) - 1.0 / (1.0 + x4)).abs() < TOL,
                "low at {hz}"
            );
            assert!(
                (mag(bands[1]) - x4 / (1.0 + x4)).abs() < TOL,
                "high at {hz}"
            );
        }
    }

    /// The largest sample-to-sample step of `x`.
    fn max_step(x: &[f32]) -> f32 {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// A crossover moved from 800 Hz to 3 kHz mid-signal steps no harder than
    /// the steadier of the two settings does.
    ///
    /// The sine is 1 kHz, so the step changes which band it lands in: the
    /// low band falls from nearly all of it to almost nothing. What must not
    /// appear is a discontinuity beyond that: the largest sample step through
    /// the move stays within the steady figure of the louder setting
    /// (`2 pi f / fs` of its amplitude) plus 10 %.
    ///
    /// Mutation (run): `split_block` solving at `self.target[j]` instead of
    /// the interpolated frequency (the coefficients jump at the block's
    /// first chunk) → a band steps past the bound → fails.
    #[test]
    fn a_frequency_step_is_ramped_not_jumped() {
        let hz = 1_000.0;
        let x = sine(hz, RATE, 24_000);
        let run = |block: usize| {
            let mut xo = Crossover::new(&[Hz(800.0)], RATE).unwrap();
            let mut low = Vec::new();
            let mut high = Vec::new();
            for (k, chunk) in x.chunks(block).enumerate() {
                if k * block == 12_000 / block * block {
                    xo.set_frequency(0, Hz(3_000.0)).unwrap();
                }
                let mut out = vec![0.0; 2 * chunk.len()];
                xo.split_block(chunk, &mut out);
                low.extend_from_slice(&out[..chunk.len()]);
                high.extend_from_slice(&out[chunk.len()..]);
            }
            (low, high)
        };
        let (low, high) = run(256);
        // Steady figures, on the second half of each side of the step.
        let steady = |b: &[f32]| max_step(&b[4_000..10_000]).max(max_step(&b[20_000..]));
        let bound = 1.1 * steady(&low).max(steady(&high));
        let moving = |b: &[f32]| max_step(&b[11_000..16_000]);
        assert!(moving(&low) <= bound, "low band stepped {}", moving(&low));
        assert!(
            moving(&high) <= bound,
            "high band stepped {}",
            moving(&high)
        );
    }

    fn xo(freqs: &[f32]) -> Result<Crossover, CrossoverError> {
        let f: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
        Crossover::new(&f, RATE)
    }

    /// The band count is 2 to 6.
    ///
    /// Mutation (run): `MAX_SPLITS` of 6 → the 7-band case builds (and
    /// indexing past the arrays would panic elsewhere) → fails.
    #[test]
    fn the_band_count_is_two_to_six() {
        assert!(matches!(
            xo(&[]),
            Err(CrossoverError::Bands {
                bands: 1,
                min: 2,
                max: 6
            })
        ));
        assert!(xo(&[100.0]).is_ok());
        assert!(xo(&[1.0, 2.0, 3.0, 4.0, 5.0]).is_ok());
        assert!(matches!(
            xo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            Err(CrossoverError::Bands { bands: 7, .. })
        ));
    }

    /// Frequencies must be finite, above 0, strictly increasing, below Nyquist.
    ///
    /// Mutation (run): `hz <= frequencies[index - 1]` to `<` → the equal pair
    /// builds → fails; `hz >= nyquist` to `>` → 24 kHz builds → fails.
    #[test]
    fn frequencies_are_validated() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -5.0] {
            assert!(
                matches!(xo(&[bad]), Err(CrossoverError::Frequency { index: 0, .. })),
                "{bad}"
            );
            assert!(
                matches!(
                    xo(&[100.0, bad]),
                    Err(CrossoverError::Frequency { index: 1, .. })
                ),
                "{bad}"
            );
        }
        assert_eq!(
            xo(&[500.0, 500.0]).unwrap_err(),
            CrossoverError::Order { index: 1 }
        );
        assert_eq!(
            xo(&[500.0, 100.0]).unwrap_err(),
            CrossoverError::Order { index: 1 }
        );
        assert!(matches!(
            xo(&[24_000.0]),
            Err(CrossoverError::Nyquist { index: 0, .. })
        ));
        assert!(matches!(
            xo(&[100.0, 30_000.0]),
            Err(CrossoverError::Nyquist { index: 1, .. })
        ));
        assert!(xo(&[23_999.0]).is_ok());
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                Crossover::new(&[Hz(100.0)], rate).unwrap_err(),
                CrossoverError::SampleRate
            );
        }
    }

    /// A refused `set_frequency` changes nothing; an accepted one is read back.
    ///
    /// Mutation (run): store before validating in `set_frequency` → the
    /// refused writes land → fails.
    #[test]
    fn set_frequency_validates_against_the_neighbours() {
        let mut c = xo(&[100.0, 1_000.0, 5_000.0]).unwrap();
        for (i, hz) in [
            (0, f32::NAN),
            (1, f32::INFINITY),
            (1, 100.0),
            (1, 5_000.0),
            (0, 1_000.0),
            (2, 1_000.0),
            (2, 24_000.0),
            (2, -1.0),
        ] {
            assert!(c.set_frequency(i, Hz(hz)).is_err(), "{i} {hz}");
        }
        assert_eq!(
            c.set_frequency(3, Hz(9_000.0)),
            Err(CrossoverError::Index { index: 3, len: 3 })
        );
        assert_eq!(
            [
                c.frequency(0),
                c.frequency(1),
                c.frequency(2),
                c.frequency(3)
            ],
            [Some(Hz(100.0)), Some(Hz(1_000.0)), Some(Hz(5_000.0)), None]
        );
        c.set_frequency(1, Hz(1_500.0)).unwrap();
        assert_eq!(c.frequency(1), Some(Hz(1_500.0)));
        assert!(c.set_frequencies(&[Hz(100.0), Hz(200.0)]).is_err());
        assert!(
            c.set_frequencies(&[Hz(100.0), Hz(90.0), Hz(300.0)])
                .is_err()
        );
        assert_eq!(c.frequency(1), Some(Hz(1_500.0)));
    }

    /// A rate that is not finite and positive changes nothing; a frequency a
    /// lower rate leaves above Nyquist still solves to a finite, stable filter.
    ///
    /// The bound: LR4 has no gain peaking, so a unit sine leaves each band at
    /// most 1 in steady state; the cascaded 2nd-order Butterworth step
    /// response overshoots by under 25 %, so 1.5 leaves margin for the
    /// transient and still fails a filter that has gone unstable.
    ///
    /// Mutation (run): accept any rate in `set_sample_rate` (`if true`) → the
    /// NaN rate sticks → fails.
    #[test]
    fn a_rate_change_keeps_the_filter_finite() {
        let mut c = xo(&[100.0, 20_000.0]).unwrap();
        c.set_sample_rate(f64::NAN);
        assert_eq!(c.sample_rate(), SampleRate(RATE));
        c.set_sample_rate(8_000.0);
        let mut peak = 0.0f32;
        for x in sine(300.0, 8_000.0, 4_000) {
            for b in c.split(x) {
                assert!(b.is_finite());
                peak = peak.max(b.abs());
            }
        }
        assert!(peak < 1.5, "{peak}");
    }

    /// `reset` clears the state: after it, silence in is silence out.
    ///
    /// Mutation (run): drop the `all_pass` clear in `reset` → fails.
    #[test]
    fn reset_clears_every_section() {
        let mut c = xo(&[100.0, 1_000.0, 5_000.0]).unwrap();
        for x in sine(80.0, RATE, 2_000) {
            c.split(x);
        }
        c.reset();
        assert_eq!(c.split(0.0), [0.0; Crossover::MAX_BANDS]);
    }

    fn render(node: &mut dyn Node, inputs: &[Vec<f32>]) -> Vec<Vec<f32>> {
        render_chunked(node, inputs, 500)
    }

    /// The node routes band-major: output `band * channels + channel`, with
    /// each channel filtered on its own.
    ///
    /// The shape declares what the node is: zero latency, an IIR's unknown tail,
    /// and a moderate cost.
    ///
    /// Mutation (run): `Tail::None` or `Cost::Heavy` in `shape` → fails.
    ///
    /// Mutation (run): index outputs `c * bands + b` → the silent right
    /// channel's outputs carry the left's bands → fails.
    #[test]
    fn the_node_writes_bands_band_major_per_channel() {
        let mut node = prepared(CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1_000.0)]).unwrap());
        let shape = node.shape();
        assert_eq!((shape.audio_in.count(), shape.audio_out.count()), (2, 4));
        assert_eq!(shape.latency, tutti_types::Latency::default());
        assert_eq!(shape.tail, Tail::Unknown);
        assert_eq!(shape.cost, Cost::Moderate);
        let left = sine(100.0, RATE, SETTLE + WINDOW);
        let out = render(&mut node, &[left, vec![0.0; SETTLE + WINDOW]]);
        let warp = |f: f64| libm::tan(core::f64::consts::PI * f / RATE);
        let x4 = (warp(100.0) / warp(1_000.0)).powi(4);
        let low = amplitude_at(&out[0][SETTLE..], 100.0, RATE);
        let high = amplitude_at(&out[2][SETTLE..], 100.0, RATE);
        assert!((low - 1.0 / (1.0 + x4)).abs() < TOL, "{low}");
        assert!((high - x4 / (1.0 + x4)).abs() < TOL, "{high}");
        assert!(out[1].iter().chain(&out[3]).all(|&s| s == 0.0));
    }

    /// Moving a frequency through the controls reaches the node; one it cannot
    /// use (out of order) is refused at the handle.
    ///
    /// Mutation (run): `process` never reading the cell → the band keeps its
    /// 1 kHz split → fails.
    #[test]
    fn the_controls_move_the_node_and_refuse_a_bad_order() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0), Hz(4_000.0)]).unwrap();
        let controls = node.controls();
        let mut node = prepared(node);
        assert_eq!(controls.len(), 2);
        assert_eq!(
            controls.set_frequency(0, Hz(5_000.0)),
            Err(CrossoverError::Order { index: 1 })
        );
        assert_eq!(
            controls.set_frequency(2, Hz(5_000.0)),
            Err(CrossoverError::Index { index: 2, len: 2 })
        );
        assert!(controls.set_frequency(0, Hz(f32::NAN)).is_err());
        assert_eq!(controls.frequency(0), Some(Hz(500.0)));
        controls.set_frequency(0, Hz(2_000.0)).unwrap();
        let x = sine(2_000.0, RATE, SETTLE + WINDOW);
        let out = render(&mut node, &[x]);
        // At the new split the low band is half amplitude.
        let low = amplitude_at(&out[0][SETTLE..], 2_000.0, RATE);
        assert!((low - 0.5).abs() < TOL, "{low}");
    }

    /// A fork starts from the frequencies set and shares no cell with the
    /// live node.
    ///
    /// Mutation (run): drop `f.detach()` in `Fresh::fresh` → a write to the
    /// live node reaches the fork → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0)]).unwrap();
        let live = node.controls();
        live.set_frequency(0, Hz(700.0)).unwrap();
        let fork = node.fresh();
        let theirs = fork.controls();
        assert_eq!(theirs.frequency(0), Some(Hz(700.0)));
        live.set_frequency(0, Hz(900.0)).unwrap();
        assert_eq!(theirs.frequency(0), Some(Hz(700.0)));
        theirs.set_frequency(0, Hz(300.0)).unwrap();
        assert_eq!(live.frequency(0), Some(Hz(900.0)));
    }

    /// The node refuses a layout it has no ports for, and bad frequencies.
    ///
    /// Mutation (run): drop the `channels == 0 ||` test in `CrossoverNode::new`
    /// → the empty layout builds → fails; `MAX_PORTS` to `MAX_PORTS + 4` →
    /// the 66-port layout builds → fails.
    #[test]
    fn the_node_validates_its_layout_and_frequencies() {
        assert!(matches!(
            CrossoverNode::new(ChannelLayout::from_count(0), &[Hz(100.0)]),
            Err(CrossoverError::Channels { channels: 0 })
        ));
        // 11 channels x 6 bands = 66 ports.
        assert!(matches!(
            CrossoverNode::new(
                ChannelLayout::from_count(11),
                &[Hz(1.0), Hz(2.0), Hz(3.0), Hz(4.0), Hz(5.0)]
            ),
            Err(CrossoverError::Channels { channels: 11 })
        ));
        assert!(
            CrossoverNode::new(
                ChannelLayout::from_count(10),
                &[Hz(1.0), Hz(2.0), Hz(3.0), Hz(4.0), Hz(5.0)]
            )
            .is_ok()
        );
        assert!(CrossoverNode::new(ChannelLayout::MONO, &[Hz(f32::NAN)]).is_err());
        assert!(CrossoverNode::new(ChannelLayout::MONO, &[Hz(9.0), Hz(3.0)]).is_err());
    }

    /// The messages say what is wrong, including a whole set of the wrong length.
    ///
    /// Mutation (run): change "increase" in the `Order` message → fails; return
    /// `Bands` for a wrong length in `set_frequencies` → the message reads
    /// "4 to 4" → fails.
    #[test]
    fn errors_name_what_is_wrong() {
        let e = xo(&[500.0, 100.0]).unwrap_err().to_string();
        assert!(e.contains("crossover 1") && e.contains("increase"), "{e}");
        let mut c = xo(&[100.0, 200.0, 300.0]).unwrap();
        let e = c.set_frequencies(&[Hz(1.0)]).unwrap_err();
        assert_eq!(
            e,
            CrossoverError::Count {
                got: 1,
                expected: 3
            }
        );
        assert_eq!(e.to_string(), "1 frequencies given for 3 crossovers");
    }

    /// Every band of a 3- or 4-band split has the same phase at every
    /// frequency it carries: `LP^2`, `HP^2` and the all-pass all have phase
    /// `-2 arg D`. Compared where a band has at least 0.1 of the tone (a
    /// weaker band's phase is below the lock-in's resolution of `TOL / 0.1`).
    ///
    /// Mutation (run): drop the `all_pass` loop in `tick` → band 0 lacks the
    /// later crossovers' phase → the phases differ at the lower crossover →
    /// fails.
    #[test]
    fn every_band_has_the_same_phase() {
        for freqs in [vec![120.0, 2_500.0], vec![120.0, 1_000.0, 6_000.0]] {
            let mut compared = 0;
            for hz in [
                100.0, 120.0, 200.0, 500.0, 1_000.0, 2_500.0, 4_000.0, 6_000.0, 9_000.0,
            ] {
                let (bands, _) = bands_at(&freqs, hz);
                let loud: Vec<_> = bands.iter().filter(|z| mag(**z) > 0.1).collect();
                for pair in loud.windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    let cross = (a.0 * b.1 - a.1 * b.0) / (mag(*a) * mag(*b));
                    let dot = (a.0 * b.0 + a.1 * b.1) / (mag(*a) * mag(*b));
                    assert!(
                        cross.abs() < 2e-5 && dot > 0.0,
                        "{freqs:?} at {hz}: {cross} {dot}"
                    );
                    compared += 1;
                }
            }
            assert!(
                compared >= 3,
                "{freqs:?}: only {compared} overlapping pairs"
            );
        }
    }

    /// A node at 22.05 kHz with one frequency above its Nyquist still moves the
    /// others: the out-of-range one is held under the rate, not the whole set
    /// frozen.
    ///
    /// Mutation (run): drop the `fit_to_rate` call in `read_targets` →
    /// `set_frequencies` refuses the set at the rate, every crossover stays
    /// where it was → the 300 Hz split never lands → fails.
    #[test]
    fn a_frequency_above_the_rate_does_not_freeze_the_others() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(200.0), Hz(15_000.0)]).unwrap();
        let controls = node.controls();
        let rate = 22_050.0;
        let mut node = tutti_graph::contract::prepared(node, SampleRate(rate), 1024);
        controls.set_frequency(0, Hz(300.0)).unwrap();
        // 9 555 frames are 130 whole periods of 300 Hz at 22.05 kHz.
        let len = 9_555 + 12_000;
        let out = render(&mut node, &[sine(300.0, rate, len)]);
        // The split moved: the low band is half amplitude at 300 Hz.
        let low = amplitude_at(&out[0][12_000..], 300.0, rate);
        assert!((low - 0.5).abs() < TOL, "{low}");
        // The top crossover is under Nyquist, so the top band exists and is finite.
        assert!(out[2].iter().all(|s| s.is_finite()));
        let cap = SOLVE_CEILING * rate;
        assert!(node.filters[0].target[1] <= cap + 1e-2 && node.filters[0].target[1] > 300.0);
    }

    /// `prepare` takes the cells' values whole, so the first block does not
    /// ramp in from the constructor's frequencies.
    ///
    /// Mutation (run): drop the `set_frequencies` in `prepare` → the filter
    /// is still at the constructor's 500 Hz → fails.
    #[test]
    fn prepare_reads_the_cells() {
        let node = CrossoverNode::new(ChannelLayout::MONO, &[Hz(500.0)]).unwrap();
        node.controls().set_frequency(0, Hz(2_000.0)).unwrap();
        let node = prepared(node);
        assert_eq!(node.filters[0].current[0], 2_000.0);
        assert_eq!(node.filters[0].target[0], 2_000.0);
    }

    /// While four bands' frequencies all ramp, the sum keeps unit amplitude.
    ///
    /// A unit 1 kHz tone through `[120, 1000, 6000]` moving to
    /// `[150, 1300, 5000]` across 256-frame blocks. The sampled peak of a
    /// 48-samples-per-period sine is at least `cos(pi / 48)`, 2.1e-3 under 1,
    /// so the bound is 3e-3 (measured: 2e-4 under 1 on the sampled peak, so
    /// the ramp's own deviation is under that).
    ///
    /// Mutation (run): drop the `all_pass` loop in `tick` → the sum is no
    /// longer unit → fails.
    #[test]
    fn the_sum_stays_flat_while_frequencies_ramp() {
        let mut c = xo(&[120.0, 1_000.0, 6_000.0]).unwrap();
        let x = sine(1_000.0, RATE, SETTLE + 4_096);
        let mut sum = Vec::new();
        for (k, chunk) in x.chunks(256).enumerate() {
            if k * 256 == SETTLE {
                c.set_frequencies(&[Hz(150.0), Hz(1_300.0), Hz(5_000.0)])
                    .unwrap();
            }
            let mut out = vec![0.0; 4 * chunk.len()];
            c.split_block(chunk, &mut out);
            for i in 0..chunk.len() {
                sum.push((0..4).map(|b| out[b * chunk.len() + i]).sum::<f32>());
            }
        }
        // The first 1 kHz period after the move starts to the last of the ramp.
        let peak = sum[SETTLE..SETTLE + 1_024]
            .iter()
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 3e-3, "peak {peak}");
    }
}
