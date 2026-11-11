//! [`FdnDesign`]: a feedback delay network as a [`Design`] (design 022 step 1c).
//!
//! `N` delay lines of mutually unrelated lengths (a table of values near primes,
//! scaled by the room size), each with a three-band absorption set from the
//! three RT60s ([`Absorption::from_rt60`]: the loop gain that loses 60 dB in
//! that time), mixed by an orthogonal reflection `A = I - 2 v v^T`, with an
//! input pre-delay, a slow modulation of the line lengths, a stereo output
//! (even lines left, odd right) with a width and a wet gain, and, if the design
//! is [`freezable`](FdnDesign::freezable), a freeze: every gain exactly one and
//! the input gated off, so the network is lossless and its tail never ends.
//!
//! **Diffusion** is the vector `v`: from one axis (no line feeds another,
//! parallel combs, the sparse end) to `1/sqrt(N)` on every line (the
//! Householder matrix, the smooth end), a unit vector all the way, so the loop
//! never gains whatever the setting. At 0 the network says
//! [`Matrix::Diagonal`], at 1 [`Matrix::Householder`] and in between
//! [`Matrix::Reflection`].
//!
//! Lengths are rounded to whole frames, so a settled network delays by whole
//! frames and a frozen one loses nothing to interpolation; a modulated line
//! reads between frames, and freezing turns the modulation off.

use tutti_types::{Amplitude, BusTopology, Db, Depth, Hz, Mix, SampleRate, Seconds, StereoWidth};

use core::num::NonZeroU32;

use crate::reverb::fdn::kernel::{CHUNK, FdnKernel};
use crate::reverb::network::{
    Absorption, Bounds, Fdn, Gains, Inline, Line, MAX_LINES, Matrix, Modulation, Network,
    OutputMap, Stage, pre_delay_frames,
};
use crate::{Cadence, Ceiling, Channels, Design, MaxDecay, Realized, Ring, Sizes};

/// The line lengths at 44.1 kHz for a room size of 0.5 (the scale is
/// `0.5 + size`), values near primes, so the delays share few factors.
const LENGTHS: [f32; 16] = [
    1031.0, 1327.0, 1523.0, 1801.0, 2053.0, 2287.0, 2621.0, 2999.0, 3331.0, 3571.0, 3911.0, 4241.0,
    4549.0, 4793.0, 5099.0, 5393.0,
];

/// The crossovers of the three bands.
pub(crate) const CROSSOVER: [Hz; 2] = [Hz(300.0), Hz(4_000.0)];

/// The longest pre-delay, in seconds.
pub const MAX_PRE_DELAY: Seconds = Seconds(0.5);
/// The deepest modulation, in frames at 44.1 kHz, at a depth of 1.
const MAX_MODULATION: f32 = 8.0;
/// The modulation rates a control may ask for.
const RATES: (f32, f32) = (0.05, 5.0);
/// The shortest RT60 a control may ask for.
pub const MIN_DECAY: Seconds = Seconds(0.05);
/// The widest stereo image.
const MAX_WIDTH: f32 = 2.0;
/// The wet gain at or below which the network is silent, in decibels.
const SILENT_DB: f32 = -120.0;
/// The loudest wet gain.
const MAX_WET_DB: f32 = 24.0;

/// The controls of an [`FdnDesign`], in tutti units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnControls {
    /// The RT60 of the mid band (index 0 of [`UnitParam::Decay`](tutti_types::UnitParam)),
    /// the low band (1) and the high band (2): `[mid, low, high]`.
    pub decay: [Seconds; 3],
    /// The room size, `0..1`: scales every line, from half to one and a half
    /// times the table. **A move lands over seconds, by design**: the realization
    /// moves a line half a frame per 32-frame chunk (a pitch bend of 27 cents),
    /// so the longest line going from size 0 to 1 takes about 6 s, to keep a
    /// size sweep free of pitch artefacts.
    pub size: Mix,
    /// How much the lines mix, `0..1`: `0` is parallel combs, `1` the smooth end.
    pub diffusion: Mix,
    /// The delay before the first sound.
    pub pre_delay: Seconds,
    /// The depth of the modulation of the line lengths, `0..1` (read as its
    /// absolute value).
    pub modulation: Depth,
    /// The rate of that modulation.
    pub rate: Hz,
    /// The stereo width of the wet signal: `0` is mono, `1` natural.
    pub width: StereoWidth,
    /// The wet gain.
    pub wet: Db,
    /// Holds the tail; ignored by a design that is not freezable.
    pub freeze: bool,
}

impl FdnControls {
    /// A mid-sized room with a two second tail, darker above: the controls an
    /// FDN starts from.
    pub const DEFAULT: FdnControls = FdnControls {
        decay: [Seconds(2.0), Seconds(2.4), Seconds(1.2)],
        size: Mix(0.5),
        diffusion: Mix(0.8),
        pre_delay: Seconds(0.0),
        modulation: Depth(0.3),
        rate: Hz(0.5),
        width: StereoWidth::NATURAL,
        wet: Db(0.0),
        freeze: false,
    };
}

impl Default for FdnControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The [`Design`] of the feedback delay network: mono in, stereo out.
///
/// A control outside its range is clamped and a non-finite one takes the
/// [`FdnControls::DEFAULT`] value, so every control gives a stable network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnDesign {
    lines: usize,
    max_decay: Seconds,
    freezable: bool,
}

impl FdnDesign {
    /// The lines of the default network.
    pub const LINES: usize = 8;

    /// A design of `lines` lines (2 to 16, even: half feed each ear), RT60s up
    /// to `max_decay` (at most [`MaxDecay::CAP`]), which holds its tail
    /// on a freeze if `freezable`. `None` when the figures are out of range.
    ///
    /// **A design that can freeze has an unbounded tail ceiling**, so a node
    /// of it never has a finite tail to declare to an export (which spends the
    /// whole cap): freeze is opt-in for that reason.
    #[must_use]
    pub fn new(lines: usize, max_decay: Seconds, freezable: bool) -> Option<Self> {
        let ok = (2..=LENGTHS.len()).contains(&lines)
            && lines.is_multiple_of(2)
            && max_decay.get().is_finite()
            && max_decay.get() >= MIN_DECAY.get()
            && max_decay.get() <= MaxDecay::CAP.get();
        ok.then_some(Self {
            lines,
            max_decay,
            freezable,
        })
    }

    /// The lines.
    #[must_use]
    pub fn lines(&self) -> usize {
        self.lines
    }

    /// The longest RT60 a control may ask for.
    #[must_use]
    pub fn max_decay(&self) -> Seconds {
        self.max_decay
    }

    /// Whether the freeze control does anything.
    #[must_use]
    pub fn freezable(&self) -> bool {
        self.freezable
    }
}

impl FdnDesign {
    /// What `Bounds::tail` is multiplied by for the rounding of the reflection
    /// vector. Each entry of a unit `v` is rounded twice (stored as `f32`, then
    /// divided by the norm and rounded again), 2^-23 relative in all, so `|v|^2`
    /// is 1 to within `e = 2^-22` and `|A| = 1 + 2e` at most. `rt60_bound`
    /// applies that norm **every frame** (the step of its proof), where the decay
    /// is `3 / (RT60 fs)` decades a frame, so the bound reads a decay slower by
    /// `x = 2e / ln 10 * RT60 fs / 3` of itself, and the tail by `1 / (1 - x)`:
    /// 0.5% at 8 s and 44.1 kHz, 11% at 192 kHz.
    fn rounding_margin(&self, ceiling: &Ceiling<MaxDecay>) -> f32 {
        let e = 2.0 * f64::from(f32::EPSILON);
        let x =
            2.0 * e / std::f64::consts::LN_10 * self.max_decay.get() * ceiling.rate().get() / 3.0;
        (1.0 / (1.0 - x.min(0.5))) as f32
    }
}

impl Default for FdnDesign {
    /// Eight lines, RT60s up to 8 s, not freezable.
    fn default() -> Self {
        Self {
            lines: Self::LINES,
            max_decay: MaxDecay::DEFAULT.get(),
            freezable: false,
        }
    }
}

/// The absorption with its band gains scaled as [`FdnKernel`] scales them (peak at
/// most 0.9999), so the network carries the gains the kernel runs.
fn rescaled(a: Absorption, rate: SampleRate) -> Absorption {
    let Absorption::Bands {
        low,
        mid,
        high,
        crossover,
    } = a
    else {
        return a;
    };
    let pole = crossover
        .map(|f| libm::exp(-std::f64::consts::TAU * f64::from(f.get()) / rate.get()) as f32);
    let [low, mid, high] =
        crate::reverb::fdn::kernel::rescale_bands([low.get(), mid.get(), high.get()], pole);
    Absorption::Bands {
        low: Amplitude(low),
        mid: Amplitude(mid),
        high: Amplitude(high),
        crossover,
    }
}

/// `v` if finite, else `default`.
fn finite(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

/// `v` if finite, else `default`, exactly: a time is not narrowed.
fn finite_secs(v: Seconds, default: Seconds) -> Seconds {
    if v.get().is_finite() { v } else { default }
}

impl Design for FdnDesign {
    type Controls = FdnControls;
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = Realized<FdnKernel>;

    // The grid the render is a function of: the kernel ramps in whole chunks
    // counted from the last reset (doc 024 section 4.4). The *design* is still
    // sampled once per block; a grid finer than that would redesign per chunk
    // for nothing the kernel needs (doc 022), so this names the render's clock
    // and not a schedule for `design_into` (doc 024 Q27).
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

    fn realization(&self) -> Realized<FdnKernel> {
        Realized::new(FdnKernel::new(), &self.blank(SampleRate::DEFAULT))
    }

    fn design_into(&self, c: &FdnControls, rate: SampleRate, out: &mut Network) {
        let d = FdnControls::DEFAULT;
        let freeze = self.freezable && c.freeze;
        let rt60 = [
            c.decay[1], // low
            c.decay[0], // mid
            c.decay[2], // high
        ]
        .map(|s| {
            let v = finite_secs(s, d.decay[0]);
            v.clamp(MIN_DECAY, self.max_decay)
        });
        let rt60 = if freeze {
            [Seconds(f64::INFINITY); 3]
        } else {
            rt60
        };
        let size = finite(c.size.get(), d.size.get()).clamp(0.0, 1.0);
        let diffusion = finite(c.diffusion.get(), d.diffusion.get()).clamp(0.0, 1.0);
        let pre = finite_secs(c.pre_delay, Seconds(0.0)).clamp(Seconds(0.0), MAX_PRE_DELAY);
        let depth = finite(c.modulation.get(), 0.0).abs().min(1.0);
        let mod_rate = finite(c.rate.get(), d.rate.get()).clamp(RATES.0, RATES.1);
        let width = finite(c.width.get(), 1.0).clamp(0.0, MAX_WIDTH);
        let wet = {
            let w = c.wet.get();
            if w.is_nan() {
                0.0
            } else if w <= SILENT_DB {
                f32::NEG_INFINITY
            } else {
                w.min(MAX_WET_DB)
            }
        };

        let n = self.lines;
        let scale = (0.5 + size) * (rate.get() / 44_100.0) as f32;
        let depth_frames = if freeze {
            0.0
        } else {
            depth * MAX_MODULATION * (rate.get() / 44_100.0) as f32
        };
        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for &base in &LENGTHS[..n] {
            let length = (base * scale).round().max(1.0);
            let _ = lines.push(Line {
                length,
                modulation: Modulation {
                    depth: depth_frames,
                    rate: Hz(mod_rate),
                },
                absorption: rescaled(Absorption::from_rt60(rt60, CROSSOVER, length, rate), rate),
                diffuser: None,
            });
        }

        // The reflection vector: one axis at diffusion 0, the flat vector at 1.
        let flat = 1.0 / (n as f64).sqrt();
        let mut v = Gains::filled(0.0, n).unwrap_or_default();
        {
            let mut norm = 0.0f64;
            for (i, x) in v.as_mut_slice().iter_mut().enumerate() {
                let axis = if i == 0 { 1.0 } else { 0.0 };
                let y = f64::from(1.0 - diffusion) * axis + f64::from(diffusion) * flat;
                *x = y as f32;
                norm += y * y;
            }
            let norm = norm.sqrt();
            for x in v.as_mut_slice() {
                *x = (f64::from(*x) / norm) as f32;
            }
        }
        let matrix = if diffusion <= 0.0 {
            Matrix::Diagonal
        } else if diffusion >= 1.0 {
            Matrix::Householder
        } else {
            Matrix::Reflection(v)
        };

        // One mono input into every line, none when frozen; even lines to the
        // left ear, odd to the right.
        let gain = (1.0 / (n as f32).sqrt()).min(1.0);
        let input = Gains::filled(if freeze { 0.0 } else { gain }, n).unwrap_or_default();
        let ear = (2.0 / n as f32).sqrt();
        let mut left = Gains::filled(0.0, n).unwrap_or_default();
        let mut right = Gains::filled(0.0, n).unwrap_or_default();
        for (i, (l, r)) in left
            .as_mut_slice()
            .iter_mut()
            .zip(right.as_mut_slice())
            .enumerate()
        {
            if i % 2 == 0 {
                *l = ear;
            } else {
                *r = ear;
            }
        }
        let mut rows = Inline::new();
        let _ = rows.push(left);
        let _ = rows.push(right);

        out.clear(rate);
        out.pre_delay = pre_delay_frames(pre, rate);
        let _ = out.stages.push(Stage::Fdn(Fdn {
            lines,
            matrix,
            input,
            output: OutputMap { rows },
        }));
        out.width = StereoWidth(width);
        out.wet = Db(wet);
    }

    /// The largest design: size 1, the longest decay in every band, the longest
    /// pre-delay, the deepest modulation. The tail is unbounded if the design
    /// can freeze, and the network's own at the longest decay if not.
    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        let worst = FdnControls {
            decay: [self.max_decay; 3],
            size: Mix(1.0),
            diffusion: Mix(1.0),
            pre_delay: MAX_PRE_DELAY,
            modulation: Depth(1.0),
            freeze: false,
            ..FdnControls::DEFAULT
        };
        let mut network = Box::new(Network::new(ceiling.rate()));
        self.design_into(&worst, ceiling.rate(), &mut network);
        let mut b = Bounds::measure(&network, ceiling.floor());
        b.tail = match b.tail {
            _ if self.freezable => Ring::Unbounded,
            Ring::Finite(t) => {
                Ring::Finite(Seconds(t.get() * f64::from(self.rounding_margin(ceiling))))
            }
            other => other,
        };
        b
    }
}

#[cfg(test)]
mod tests;
