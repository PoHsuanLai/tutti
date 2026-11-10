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

use tutti_types::{ChannelLayout, Db, Depth, Hz, Mix, SampleRate, Seconds, StereoWidth};

use crate::fdn::FdnKernel;
use crate::network::{
    Absorption, Bounds, Ceiling, Channels, Design, Fdn, Gains, Inline, Line, MAX_LINES, Matrix,
    Modulation, Network, OutputMap, Ring, Stage, pre_delay_frames,
};

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
    /// times the table. **A move lands over seconds, by design**: the kernel
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
    /// to `max_decay` (at most [`Ceiling::MAX_DECAY_CAP`]), which holds its tail
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
            && max_decay.get() <= Ceiling::MAX_DECAY_CAP.get();
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
    fn rounding_margin(&self, ceiling: &Ceiling) -> f32 {
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
            max_decay: Ceiling::MAX_DECAY,
            freezable: false,
        }
    }
}

/// `v` if finite, else `default`.
fn finite(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

impl Design for FdnDesign {
    type Controls = FdnControls;
    type Kernel = FdnKernel;

    fn channels(&self) -> Channels {
        Channels {
            input: ChannelLayout::MONO,
            output: ChannelLayout::STEREO,
        }
    }

    fn kernel(&self) -> FdnKernel {
        FdnKernel::new()
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
            let v = finite(s.to_f32(), d.decay[0].to_f32());
            Seconds::from_f32(v.clamp(MIN_DECAY.to_f32(), self.max_decay.to_f32()))
        });
        let rt60 = if freeze {
            [Seconds(f64::INFINITY); 3]
        } else {
            rt60
        };
        let size = finite(c.size.get(), d.size.get()).clamp(0.0, 1.0);
        let diffusion = finite(c.diffusion.get(), d.diffusion.get()).clamp(0.0, 1.0);
        let pre = finite(c.pre_delay.to_f32(), 0.0).clamp(0.0, MAX_PRE_DELAY.to_f32());
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
                absorption: Absorption::from_rt60(rt60, CROSSOVER, length, rate),
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
        out.pre_delay = pre_delay_frames(Seconds::from_f32(pre), rate);
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
    fn bounds(&self, ceiling: &Ceiling) -> Bounds {
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
        let mut b = Bounds::of(&network, ceiling.floor());
        b.tail = match b.tail {
            _ if self.freezable => Ring::Unbounded,
            Ring::Finite(t) => Ring::Finite(Seconds::from_f32(
                t.to_f32() * self.rounding_margin(ceiling),
            )),
            other => other,
        };
        b
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::*;
    use crate::network::contract::{DesignCase, design_contract};
    use crate::network::kernel_contract::{KernelCase, kernel_contract};
    use crate::network::{Band, Fdn, Matrix, Network, Stage};

    fn freezable() -> FdnDesign {
        FdnDesign::new(8, Ceiling::MAX_DECAY, true).unwrap()
    }

    fn base() -> FdnControls {
        FdnControls::DEFAULT
    }

    fn with(f: impl FnOnce(&mut FdnControls)) -> FdnControls {
        let mut c = base();
        f(&mut c);
        c
    }

    impl DesignCase for FdnDesign {
        fn reachable(&self) -> Vec<FdnControls> {
            let mut all = Vec::new();
            for low in [0.2, 1.0, 8.0] {
                for mid in [0.2, 1.0, 8.0] {
                    for high in [0.2, 1.0, 8.0] {
                        for size in [0.0, 0.5, 1.0] {
                            for diffusion in [0.0, 0.5, 1.0] {
                                for freeze in [false, true] {
                                    for pre in [0.0, 0.1] {
                                        all.push(FdnControls {
                                            decay: [Seconds(mid), Seconds(low), Seconds(high)],
                                            size: Mix(size),
                                            diffusion: Mix(diffusion),
                                            pre_delay: Seconds(pre),
                                            freeze,
                                            ..base()
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let mut rng = StdRng::seed_from_u64(22);
            for _ in 0..200 {
                all.push(FdnControls {
                    decay: [0; 3].map(|_| Seconds(rng.gen_range(0.05..8.0))),
                    size: Mix(rng.gen_range(0.0..=1.0)),
                    diffusion: Mix(rng.gen_range(0.0..=1.0)),
                    pre_delay: Seconds(rng.gen_range(0.0..=0.5)),
                    modulation: Depth(rng.gen_range(-1.0..=1.0)),
                    rate: Hz(rng.gen_range(0.05..=5.0)),
                    width: StereoWidth(rng.gen_range(0.0..=2.0)),
                    wet: Db(rng.gen_range(-60.0..=12.0)),
                    freeze: rng.gen_bool(0.2),
                });
            }
            all
        }

        fn hostile(&self) -> Vec<FdnControls> {
            let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 40.0, 0.0];
            nasty
                .iter()
                .map(|&v| FdnControls {
                    decay: [Seconds::from_f32(v); 3],
                    size: Mix(v),
                    diffusion: Mix(v),
                    pre_delay: Seconds::from_f32(v),
                    modulation: Depth(v),
                    rate: Hz(v),
                    width: StereoWidth(v),
                    wet: Db(v),
                    freeze: false,
                })
                .collect()
        }

        fn requested_rt60(&self, c: &FdnControls, _: SampleRate, band: Band) -> Option<Seconds> {
            Some(if self.freezable && c.freeze {
                Seconds(f64::INFINITY)
            } else {
                c.decay[match band {
                    Band::Mid => 0,
                    Band::Low => 1,
                    Band::High => 2,
                }]
            })
        }

        fn frozen(&self, c: &FdnControls) -> bool {
            self.freezable && c.freeze
        }

        fn pre_delay(&self, c: &FdnControls) -> Seconds {
            c.pre_delay
        }

        fn pre_delay_pairs(&self) -> Vec<(FdnControls, FdnControls, Seconds)> {
            vec![(
                with(|c| c.modulation = Depth(0.0)),
                with(|c| {
                    c.modulation = Depth(0.0);
                    c.pre_delay = Seconds(0.1);
                }),
                Seconds(0.1),
            )]
        }

        fn decay_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
            [(0.2, 1.0), (1.0, 8.0)]
                .map(|(a, b)| {
                    (
                        with(|c| c.decay = [Seconds(a); 3]),
                        with(|c| c.decay = [Seconds(b); 3]),
                    )
                })
                .to_vec()
        }

        fn size_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
            [(0.0, 0.5), (0.5, 1.0)]
                .map(|(a, b)| {
                    (
                        with(|c| {
                            c.size = Mix(a);
                            c.modulation = Depth(0.0);
                        }),
                        with(|c| {
                            c.size = Mix(b);
                            c.modulation = Depth(0.0);
                        }),
                    )
                })
                .to_vec()
        }

        fn diffusion_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
            [(0.0, 0.5), (0.0, 1.0)]
                .map(|(a, b)| {
                    (
                        with(|c| c.diffusion = Mix(a)),
                        with(|c| c.diffusion = Mix(b)),
                    )
                })
                .to_vec()
        }
    }

    mod freezable_design {
        use super::*;
        design_contract!(freezable());
    }

    mod plain_design {
        use super::*;
        design_contract!(FdnDesign::default());
    }

    impl KernelCase for FdnDesign {
        fn tolerance(&self) -> f32 {
            0.15
        }

        /// Deep: a train of echoes (parallel combs) decays in steps, and a fit
        /// over a wide range averages them.
        fn fit_depth(&self) -> (f64, f64) {
            (20.0, 100.0)
        }

        fn probes(&self) -> Vec<FdnControls> {
            vec![
                base(),
                with(|c| {
                    c.decay = [Seconds(3.0), Seconds(3.5), Seconds(1.5)];
                    c.diffusion = Mix(1.0);
                    c.pre_delay = Seconds(0.05);
                    c.modulation = Depth(0.5);
                }),
                with(|c| {
                    c.decay = [Seconds(1.0), Seconds(1.5), Seconds(0.5)];
                    c.size = Mix(0.2);
                    c.diffusion = Mix(0.3);
                }),
                with(|c| {
                    c.diffusion = Mix(0.0);
                    c.modulation = Depth(0.0);
                }),
            ]
        }

        fn static_probes(&self) -> Vec<FdnControls> {
            self.probes()
                .into_iter()
                .map(|c| FdnControls {
                    modulation: Depth(0.0),
                    pre_delay: Seconds(0.02),
                    ..c
                })
                .collect()
        }

        fn extremes(&self) -> Vec<FdnControls> {
            let hard = |f: fn(&mut FdnControls)| {
                with(|c| {
                    c.size = Mix(1.0);
                    c.modulation = Depth(1.0);
                    c.rate = Hz(5.0);
                    f(c)
                })
            };
            vec![
                hard(|c| c.decay = [Seconds(8.0); 3]),
                hard(|c| c.decay = [Seconds(8.0), Seconds(8.0), Seconds(0.05)]),
                hard(|c| c.decay = [Seconds(8.0), Seconds(0.05), Seconds(0.05)]),
                hard(|c| {
                    c.decay = [Seconds(8.0); 3];
                    c.diffusion = Mix(0.0);
                }),
            ]
        }

        fn steps(&self) -> Vec<(FdnControls, FdnControls)> {
            vec![
                (base(), with(|c| c.decay = [Seconds(6.0); 3])),
                (base(), with(|c| c.size = Mix(0.9))),
                (base(), with(|c| c.diffusion = Mix(0.1))),
                // A pre-delay that is not a whole number of cycles of the 150 Hz
                // tone the case plays.
                (base(), with(|c| c.pre_delay = Seconds(0.0123))),
                (base(), with(|c| c.wet = Db(-12.0))),
                (base(), with(|c| c.freeze = true)),
            ]
        }

        fn tone(&self, band: Band, rate: SampleRate) -> f32 {
            match band {
                Band::Low => 0.0,
                Band::Mid => (libm::sqrt(300.0 * 4_000.0) / rate.get()) as f32,
                Band::High => 0.5,
            }
        }

        fn freeze_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
            vec![(base(), with(|c| c.freeze = true))]
        }

        fn width_pairs(&self) -> Vec<(FdnControls, FdnControls)> {
            vec![(base(), with(|c| c.width = StereoWidth::MONO))]
        }

        fn wet_pairs(&self) -> Vec<(FdnControls, FdnControls, f32)> {
            vec![
                (base(), with(|c| c.wet = Db(-6.0206)), 0.5),
                (base(), with(|c| c.wet = Db(f32::NEG_INFINITY)), 0.0),
            ]
        }

        /// The state's amplitude is at most `|B| m / (1 - g)` (the input's norm
        /// times the longest line over one minus the largest band gain: each trip
        /// loses a factor `g` and the reflection is orthogonal), and the output
        /// maps it by `|C|`, the width and the wet gain.
        fn peak_bound(&self, _: &FdnControls, n: &Network) -> f32 {
            let Some(crate::network::Stage::Fdn(f)) = n.stages.first() else {
                unreachable!("a network of one FDN")
            };
            let g = f
                .lines
                .iter()
                .flat_map(|l| Band::ALL.map(|b| l.absorption.gain(b).get()))
                .fold(0.0f32, f32::max);
            let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
            let m = f
                .lines
                .iter()
                .map(|l| l.length + l.modulation.depth)
                .fold(0.0, f32::max);
            let c = f
                .output
                .rows
                .iter()
                .map(|r| norm(r).powi(2))
                .sum::<f32>()
                .sqrt();
            norm(&f.input) * m / (1.0 - g) * c * (1.0 + n.width.get()) * n.wet.to_amplitude().get()
        }
    }

    kernel_contract!(freezable());

    use crate::network::kernel_contract::{Defect, Defective, cases};

    fn defective(defect: Defect) -> Defective<FdnDesign> {
        Defective {
            inner: freezable(),
            defect,
            tweak: |_| {},
        }
    }

    #[track_caller]
    fn fails(result: Result<(), String>, why: &str) {
        let e = result.expect_err("the case should fail on the defective kernel");
        assert!(e.contains(why), "{e}");
    }

    /// Each case of the kernel contract fails on an FDN with the defect its
    /// mutation names, the cases Freeverb cannot run (pre-delay, size,
    /// diffusion, freeze, width, wet gain) included.
    #[test]
    fn a_defective_fdn_fails_its_case() {
        // rt60 and tail: every loop gain 5% and 3% too high.
        fails(
            cases::rt60(&defective(Defect::LoopGain(1.05))),
            "measured RT60",
        );
        fails(cases::tail_covers(&defective(Defect::LoopGain(1.03))), "dB");
        // band order: every band like the lowest.
        fails(cases::band_order(&defective(Defect::NoDamping)), "bands");
        // arrival and latency.
        fails(
            cases::arrival(&defective(Defect::LateByOne)),
            "first output",
        );
        fails(
            cases::arrival(&defective(Defect::FalseLatency)),
            "first output",
        );
        fails(cases::latency(&defective(Defect::FalseLatency)), "latency");
        fails(
            cases::wet_only(&defective(Defect::DryLeak)),
            "shortest path",
        );
        fails(
            cases::decorrelated(&defective(Defect::MonoOutput)),
            "correlate",
        );
        fails(cases::stability(&defective(Defect::Runaway)), "");
        fails(
            cases::click_free(&defective(Defect::UnslewedGain(4.0))),
            "jump",
        );
        // A modest step too: 1.5 times fails the 3 times allowance (measured
        // margins of the clean kernels: Freeverb 1.98, the FDN 1.51 times the
        // steady jump).
        fails(
            cases::click_free(&defective(Defect::UnslewedGain(1.5))),
            "jump",
        );
        fails(cases::reset(&defective(Defect::NoReset)), "reset");
        fails(cases::fresh(&defective(Defect::FreshKeepsState)), "fresh()");
        fails(
            cases::block_independent(&defective(Defect::BlockDependent)),
            "blocks of",
        );
        // line lengths: the last line 5% long, inside the RT60 tolerance.
        fails(
            cases::line_lengths(&defective(Defect::LastLineLong)),
            "first echo",
        );
        // pre-delay, size, diffusion, freeze, width, wet gain.
        fails(
            cases::pre_delay(&defective(Defect::PreDelayIgnored)),
            "pre-delay",
        );
        fails(cases::size(&defective(Defect::FixedLines)), "bigger room");
        fails(cases::diffusion(&defective(Defect::NoMixing)), "diffusion");
        fails(
            cases::freeze(&defective(Defect::LoopGain(0.998))),
            "frozen level",
        );
        fails(
            cases::freeze(&defective(Defect::OpenInput)),
            "input reached",
        );
        fails(
            cases::width(&defective(Defect::WidthIgnored)),
            "correlation",
        );
        fails(cases::wet_gain(&defective(Defect::WetIgnored)), "wet gain");
    }

    /// No hook of the FDN is vacuous: every case that Freeverb leaves empty has
    /// something to run here, so a case that stops running fails this test.
    ///
    /// Mutation (run): returning no pairs from `width_pairs` fails it.
    #[test]
    fn no_hook_of_the_fdn_is_vacuous() {
        let d = freezable();
        assert!(!KernelCase::freeze_pairs(&d).is_empty());
        assert!(!KernelCase::width_pairs(&d).is_empty());
        assert!(!KernelCase::wet_pairs(&d).is_empty());
        assert!(!DesignCase::pre_delay_pairs(&d).is_empty());
        assert!(!DesignCase::size_pairs(&d).is_empty());
        assert!(!DesignCase::diffusion_pairs(&d).is_empty());
        assert!(DesignCase::frozen(&d, &with(|c| c.freeze = true)));
        assert!(DesignCase::pre_delay(&d, &with(|c| c.pre_delay = Seconds(0.1))).get() > 0.0);
    }

    fn design_of(d: &FdnDesign, c: &FdnControls) -> Network {
        d.design(c, SampleRate(48_000.0))
    }

    fn fdn_of(n: &Network) -> &Fdn {
        let Some(Stage::Fdn(f)) = n.stages.first() else {
            panic!("one FDN stage")
        };
        f
    }

    /// Diffusion is the reflection vector: none couples no line (the network
    /// says so), full is the Householder matrix, and between is a unit vector
    /// that leans from one axis toward the flat vector, so the loop is
    /// orthogonal at every setting.
    ///
    /// Mutations (run): skipping the normalisation fails the unit-length
    /// assertion; `diffusion <= 0.0` written `< 0.0` fails the first row.
    #[test]
    fn diffusion_is_the_reflection_vector() {
        let d = freezable();
        assert_eq!(
            fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix(0.0)))).matrix,
            Matrix::Diagonal
        );
        assert_eq!(
            fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix(1.0)))).matrix,
            Matrix::Householder
        );
        let mut last_axis = f32::INFINITY;
        for diffusion in [0.1, 0.25, 0.5, 0.75, 0.9] {
            let n = design_of(&d, &with(|c| c.diffusion = Mix(diffusion)));
            let Matrix::Reflection(v) = &fdn_of(&n).matrix else {
                panic!("a reflection at {diffusion}")
            };
            assert_eq!(v.len(), 8);
            let unit: f64 = v.iter().map(|&x| f64::from(x).powi(2)).sum();
            assert!((unit - 1.0).abs() < 1e-6, "{diffusion}: |v|^2 = {unit}");
            assert!(
                v[0] < last_axis,
                "the first line's share falls as the mix grows"
            );
            last_axis = v[0];
        }
    }

    /// A freeze holds every gain at exactly one, shuts the input, stops the
    /// modulation and makes the network lossless with an unbounded tail; a
    /// design that is not freezable ignores it.
    ///
    /// Mutations (run): leaving the input open fails the input assertion;
    /// `self.freezable && c.freeze` written `c.freeze` fails the last one.
    #[test]
    fn a_freeze_is_lossless_and_takes_no_input() {
        let (d, plain) = (freezable(), FdnDesign::default());
        let frozen = with(|c| c.freeze = true);
        let n = design_of(&d, &frozen);
        let f = fdn_of(&n);
        assert!(n.is_lossless() && n.is_stable());
        assert_eq!(n.tail(Db(-60.0)), Ring::Unbounded);
        assert!(f.input.iter().all(|&g| g == 0.0), "the input is shut");
        assert!(f.lines.iter().all(|l| l.modulation.depth == 0.0));
        for l in f.lines.iter() {
            for band in crate::network::Band::ALL {
                assert_eq!(l.absorption.gain(band).get().to_bits(), 1.0f32.to_bits());
            }
        }
        assert_eq!(design_of(&plain, &frozen), design_of(&plain, &base()));
        assert!(!design_of(&plain, &frozen).is_lossless());
    }

    /// Lengths are whole frames, grow with the size and scale with the rate, so
    /// a settled network delays by whole frames and a frozen one reads no
    /// fraction.
    ///
    /// Mutation (run): dropping the `round` fails the whole-frame assertion.
    #[test]
    fn lengths_are_whole_frames_that_grow_with_size_and_rate() {
        let d = FdnDesign::default();
        let mut last = vec![0.0f32; 8];
        for size in [0.0, 0.3, 0.6, 1.0] {
            let n = design_of(&d, &with(|c| c.size = Mix(size)));
            let lengths: Vec<f32> = fdn_of(&n).lines.iter().map(|l| l.length).collect();
            assert!(lengths.iter().all(|l| l.fract() == 0.0), "{lengths:?}");
            assert!(lengths.iter().zip(&last).all(|(a, b)| a > b));
            last = lengths;
        }
        let at = |hz| d.design(&base(), SampleRate(hz));
        let (a, b) = (at(44_100.0), at(88_200.0));
        for (x, y) in fdn_of(&a).lines.iter().zip(fdn_of(&b).lines.iter()) {
            assert!((y.length - 2.0 * x.length).abs() <= 1.0);
        }
    }

    /// The configuration refuses what the network cannot hold.
    ///
    /// Mutation (run): dropping the evenness test accepts 7 lines.
    #[test]
    fn a_configuration_refuses_what_it_cannot_hold() {
        let ok = Seconds(8.0);
        assert!(FdnDesign::new(8, ok, false).is_some());
        assert!(FdnDesign::new(16, ok, true).is_some());
        for lines in [0, 1, 7, 17, 18] {
            assert!(FdnDesign::new(lines, ok, false).is_none(), "{lines}");
        }
        for bad in [0.0, 0.01, 60.001, f32::NAN, f32::INFINITY] {
            assert!(
                FdnDesign::new(8, Seconds::from_f32(bad), false).is_none(),
                "{bad}"
            );
        }
    }

    /// A frozen tail keeps its energy through a diffusion ramp from 0 to 1: the
    /// reflection is a unit vector at every step, not a straight line between
    /// two (which dips to `|v|^2` of 0.68 and is a contraction). The energy the
    /// lines hold (their whole state while frozen) before and after the 20 ms
    /// ramp agrees to 0.01 dB. (The output's level would not do: it is the
    /// lines' energy over their lengths, and mixing moves energy between lines of
    /// different length.)
    ///
    /// Mutation (run): dropping the per-chunk renormalisation in
    /// `FdnKernel::begin_chunk` loses 0.25 dB and fails.
    #[test]
    fn a_diffusion_ramp_keeps_a_frozen_tails_energy() {
        use crate::network::{Kernel, Setup};
        let rate = SampleRate(48_000.0);
        let d = freezable();
        let mut k = d.kernel();
        k.prepare(
            &d.bounds(&Ceiling::at(rate).unwrap()),
            &Setup {
                rate,
                max_block: tutti_types::MaxBlock::of(tutti_types::Samples(64)),
                input: ChannelLayout::MONO,
                output: ChannelLayout::STEREO,
            },
        );
        let at = |diffusion: f32, freeze: bool| {
            d.design(
                &with(|c| {
                    c.diffusion = Mix(diffusion);
                    c.freeze = freeze;
                    c.modulation = Depth(0.0);
                }),
                rate,
            )
        };
        let run = |k: &mut FdnKernel, frames: usize, noise: bool| {
            let mut rng = StdRng::seed_from_u64(5);
            let x: Vec<f32> = (0..frames)
                .map(|_| if noise { rng.gen_range(-0.5..0.5) } else { 0.0 })
                .collect();
            for chunk in x.chunks(64) {
                let (mut l, mut r) = (vec![0.0f32; chunk.len()], vec![0.0f32; chunk.len()]);
                k.process(&[chunk], &mut [&mut l, &mut r]);
            }
        };
        k.retarget(&at(0.0, false));
        run(&mut k, 48_000, true);
        k.retarget(&at(0.0, true));
        run(&mut k, 24_000, false);
        let before = k.state_energy();
        k.retarget(&at(1.0, true));
        run(&mut k, 9_600, false);
        let db = 10.0 * (k.state_energy() / before).log10();
        assert!(
            db.abs() < 0.01,
            "the frozen energy moved {db:.4} dB through the ramp"
        );
    }

    /// The bounds hold a design's tail at 192 kHz too, where the rounding of the
    /// reflection vector (applied every frame by `rt60_bound`) is a larger share
    /// of a frame's decay: the margin is computed from the rate and the longest
    /// decay, not a flat figure.
    ///
    /// Mutation (run): a margin of 1 fails at some diffusion below.
    #[test]
    fn the_bounds_dominate_where_rounding_matters() {
        let d = FdnDesign::default();
        let rate = SampleRate(192_000.0);
        let ceiling = Ceiling::at(rate).unwrap();
        let bound = d.bounds(&ceiling);
        for k in 1..100 {
            let c = FdnControls {
                decay: [d.max_decay(); 3],
                size: Mix(1.0),
                diffusion: Mix(k as f32 / 100.0),
                pre_delay: MAX_PRE_DELAY,
                modulation: Depth(1.0),
                ..FdnControls::DEFAULT
            };
            let tail = d.design(&c, rate).tail(ceiling.floor());
            assert!(
                bound.tail.covers(tail),
                "diffusion {k}%: {tail:?} over {:?}",
                bound.tail
            );
        }
    }
}
