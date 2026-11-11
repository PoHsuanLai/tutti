//! The deliberately broken designs and realizations the cases are shown to fail
//! on, and a toy design with every control: test-only, the mutation record of the
//! contract.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{Amplitude, BusTopology, Db, Hz, Mix, SampleRate, Samples, Seconds, StereoWidth};

use super::{Anchor, DesignCase, Hook, LtiCase, RealizationCase, ReverbCase};
use crate::network::{
    AllPass, AllPassForm, Fdn, Gains, Inline, Line, MAX_LINES, Matrix, Modulation, OutputMap,
    pre_delay_frames,
};
use crate::{
    Absorption, Band, Bounds, Ceiling, Channels, Design, Effect, Lti, MaxDecay, Network,
    Realization, Ring, Setup, Sizes, Stage, System, loop_gain,
};

/// A design whose model is a reverb [`Network`] and whose realization renders it:
/// what a defect can be wrapped around.
pub(crate) trait NetDesign:
    Design<
        Model = Network,
        Bounds = Bounds,
        Range = MaxDecay,
        Realization: Effect<Network, Bounds = Bounds> + Clone,
    >
{
}

impl<D> NetDesign for D where
    D: Design<
            Model = Network,
            Bounds = Bounds,
            Range = MaxDecay,
            Realization: Effect<Network, Bounds = Bounds> + Clone,
        >
{
}

/// A design with one deliberate defect, to show a case fails on it.
#[derive(Clone)]
pub(crate) struct Mutant<D> {
    pub(crate) inner: D,
    pub(crate) network: fn(&mut Network),
    pub(crate) bounds: fn(Bounds) -> Bounds,
    /// The hooks it says are vacant, when not the inner design's.
    pub(crate) vacant: Option<&'static [(Hook, &'static str)]>,
}

impl<D> Mutant<D> {
    pub(crate) fn new(inner: D, network: fn(&mut Network)) -> Self {
        Self {
            inner,
            network,
            bounds: |b| b,
            vacant: None,
        }
    }
}

impl<D: NetDesign> Design for Mutant<D> {
    type Controls = D::Controls;
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = D::Realization;

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    fn channels(&self) -> Channels {
        self.inner.channels()
    }

    fn design_into(&self, controls: &D::Controls, rate: SampleRate, out: &mut Network) {
        self.inner.design_into(controls, rate, out);
        (self.network)(out);
    }

    fn realization(&self) -> D::Realization {
        self.inner.realization()
    }

    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        (self.bounds)(self.inner.bounds(ceiling))
    }
}

impl<D: NetDesign + DesignCase> DesignCase for Mutant<D> {
    fn rates(&self) -> Vec<SampleRate> {
        self.inner.rates()
    }
    fn reachable(&self) -> Vec<D::Controls> {
        self.inner.reachable()
    }
    fn hostile(&self) -> Vec<D::Controls> {
        self.inner.hostile()
    }
    fn ceiling(&self, rate: SampleRate) -> Ceiling<MaxDecay> {
        self.inner.ceiling(rate)
    }
    fn musical_delay(&self, model: &Network) -> usize {
        self.inner.musical_delay(model)
    }
    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        self.vacant.unwrap_or_else(|| self.inner.vacant())
    }
}

impl<D: NetDesign + RealizationCase> RealizationCase for Mutant<D> {
    fn probes(&self) -> Vec<D::Controls> {
        self.inner.probes()
    }
    fn extremes(&self) -> Vec<D::Controls> {
        self.inner.extremes()
    }
    fn steps(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.steps()
    }
    fn static_probes(&self) -> Vec<D::Controls> {
        self.inner.static_probes()
    }
}

impl<D: NetDesign + LtiCase> LtiCase for Mutant<D> {
    fn anchors(&self) -> Vec<Anchor<D::Controls>> {
        self.inner.anchors()
    }
    fn marginal(&self, c: &D::Controls) -> bool {
        self.inner.marginal(c)
    }
    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        self.inner.fill(rate, trips)
    }
    fn low_tone(&self, rate: SampleRate) -> f32 {
        self.inner.low_tone(rate)
    }
    fn response_tolerance(&self) -> f64 {
        self.inner.response_tolerance()
    }
    fn decay_tolerance(&self) -> f64 {
        self.inner.decay_tolerance()
    }
    fn tail_slack(&self) -> f64 {
        self.inner.tail_slack()
    }
    fn hinf_slack(&self) -> f64 {
        self.inner.hinf_slack()
    }
}

impl<D: NetDesign + ReverbCase> ReverbCase for Mutant<D> {
    fn requested_rt60(&self, c: &D::Controls, rate: SampleRate, band: Band) -> Option<Seconds> {
        self.inner.requested_rt60(c, rate, band)
    }
    fn frozen(&self, c: &D::Controls) -> bool {
        self.inner.frozen(c)
    }
    fn pre_delay(&self, c: &D::Controls) -> Seconds {
        self.inner.pre_delay(c)
    }
    fn pre_delay_pairs(&self) -> Vec<(D::Controls, D::Controls, Seconds)> {
        self.inner.pre_delay_pairs()
    }
    fn decay_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.decay_pairs()
    }
    fn size_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.size_pairs()
    }
    fn diffusion_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.diffusion_pairs()
    }
    fn wet_only(&self) -> bool {
        self.inner.wet_only()
    }
    fn tolerance(&self) -> f32 {
        self.inner.tolerance()
    }
    fn fit_depth(&self) -> (f64, f64) {
        self.inner.fit_depth()
    }
    fn tone(&self, band: Band, rate: SampleRate) -> f32 {
        self.inner.tone(band, rate)
    }
    fn freeze_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.freeze_pairs()
    }
    fn width_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.width_pairs()
    }
    fn wet_pairs(&self) -> Vec<(D::Controls, D::Controls, f32)> {
        self.inner.wet_pairs()
    }
}

/// Applies `f` to every [`Absorption`] of the network's recirculating stages.
pub(crate) fn map_absorption(n: &mut Network, f: impl Fn(Absorption) -> Absorption) {
    for stage in n.stages.as_mut_slice() {
        if let Stage::Fdn(fdn) = stage {
            for line in fdn.lines.as_mut_slice() {
                line.absorption = f(line.absorption);
            }
        }
    }
}

/// Applies `f` to every [`Fdn`] of the network.
pub(crate) fn map_fdn(n: &mut Network, f: impl Fn(&mut Fdn)) {
    for stage in n.stages.as_mut_slice() {
        if let Stage::Fdn(fdn) = stage {
            f(fdn);
        }
    }
}

// ---- ToyFdn: a test-only design with every control ---------------------------

/// The controls of [`ToyFdn`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ToyControls {
    pub(crate) decay: [Seconds; 3],
    pub(crate) size: Mix,
    pub(crate) diffusion: Mix,
    pub(crate) pre_delay: Seconds,
    pub(crate) freeze: bool,
}

impl ToyControls {
    pub(crate) const BASE: ToyControls = ToyControls {
        decay: [Seconds(1.0); 3],
        size: Mix(0.5),
        diffusion: Mix(1.0),
        pre_delay: Seconds(0.0),
        freeze: false,
    };
}

/// Eight lines, band decay from three RT60s, a size that scales the lengths, a
/// diffusion that switches the matrix from diagonal to Hadamard, a pre-delay
/// and a freeze: the controls Freeverb lacks, so the contract's cases have
/// something to say. Not shipped; the FDN of step 1c replaces it as the second
/// user.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToyFdn;

const TOY_LENGTHS: [f32; 8] = [
    1031.0, 1327.0, 1523.0, 1801.0, 2053.0, 2287.0, 2621.0, 2999.0,
];
const TOY_MAX_PRE_DELAY: Seconds = Seconds(1.0);

impl ToyFdn {
    fn decay(s: Seconds) -> Seconds {
        let v = s.get();
        Seconds(if v.is_nan() {
            1.0
        } else {
            v.clamp(0.05, MaxDecay::DEFAULT.get().get())
        })
    }
}

/// The realization of a design that is never run: [`ToyFdn`] is analysed, not
/// heard.
#[derive(Clone, Debug)]
pub(crate) struct Unrun;

impl Realization<Network> for Unrun {
    type Bounds = Bounds;
    fn prepare(&mut self, _: &Bounds, _: &Setup) {}
    fn retarget(&mut self, _: &Network) {}
    fn reset(&mut self) {}
    fn fresh(&self) -> Self {
        Unrun
    }
}

impl Effect<Network> for Unrun {
    fn process(&mut self, _: &[&[f32]], output: &mut [&mut [f32]]) {
        output.iter_mut().for_each(|c| c.fill(0.0));
    }
}

impl Design for ToyFdn {
    type Controls = ToyControls;
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = Unrun;

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    fn channels(&self) -> Channels {
        Channels::new(BusTopology::MONO, BusTopology::STEREO)
    }

    fn realization(&self) -> Unrun {
        Unrun
    }

    fn design_into(&self, c: &ToyControls, rate: SampleRate, out: &mut Network) {
        let size = if c.size.get().is_nan() {
            0.5
        } else {
            c.size.get().clamp(0.0, 1.0)
        };
        let diffusion = if c.diffusion.get().is_nan() {
            1.0
        } else {
            c.diffusion.get().clamp(0.0, 1.0)
        };
        let pre = if c.pre_delay.get().is_nan() {
            Seconds(0.0)
        } else {
            Seconds(c.pre_delay.get().clamp(0.0, TOY_MAX_PRE_DELAY.get()))
        };
        let rt60 = if c.freeze {
            [Seconds(f64::INFINITY); 3]
        } else {
            c.decay.map(Self::decay)
        };
        let scale = (0.5 + size) * (rate.get() / 44_100.0) as f32;
        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for base in TOY_LENGTHS {
            let length = base * scale;
            let _ = lines.push(Line {
                length,
                modulation: Modulation::NONE,
                absorption: {
                    // As `FdnDesign` does: scaled to peak at most 0.9999.
                    let Absorption::Bands {
                        low,
                        mid,
                        high,
                        crossover,
                    } = Absorption::from_rt60(rt60, [Hz(250.0), Hz(4000.0)], length, rate)
                    else {
                        unreachable!()
                    };
                    let pole = crossover.map(|f| {
                        libm::exp(-std::f64::consts::TAU * f64::from(f.get()) / rate.get()) as f32
                    });
                    let [low, mid, high] =
                        crate::fdn::rescale_bands([low.get(), mid.get(), high.get()], pole);
                    Absorption::Bands {
                        low: Amplitude(low),
                        mid: Amplitude(mid),
                        high: Amplitude(high),
                        crossover,
                    }
                },
                diffuser: None,
            });
        }
        let n = lines.len();
        let mut even = Gains::filled(0.0, n).unwrap_or_default();
        let mut odd = Gains::filled(0.0, n).unwrap_or_default();
        for (i, (e, o)) in even
            .as_mut_slice()
            .iter_mut()
            .zip(odd.as_mut_slice())
            .enumerate()
        {
            if i % 2 == 0 { *e = 0.5 } else { *o = 0.5 }
        }
        let mut rows = Inline::new();
        let _ = rows.push(even);
        let _ = rows.push(odd);
        let mut network = Network::new(rate);
        network.pre_delay = pre_delay_frames(pre, rate);
        let _ = network.stages.push(Stage::Fdn(Fdn {
            lines,
            matrix: if diffusion >= 0.5 {
                Matrix::Hadamard
            } else {
                Matrix::Diagonal
            },
            input: Gains::filled(0.35, n).unwrap_or_default(),
            output: OutputMap { rows },
        }));
        let ap = AllPass {
            delay: 142.0 * scale,
            gain: 0.5,
            modulation: Modulation::NONE,
            form: AllPassForm::Lattice,
        };
        let mut channels = Inline::new();
        let _ = channels.push(ap);
        let _ = channels.push(ap);
        let _ = network.stages.push(Stage::AllPass(channels));
        network.width = StereoWidth::NATURAL;
        network.wet = Db(0.0);
        *out = network;
    }

    /// The largest design (size 1, the longest decay, the longest pre-delay);
    /// the tail is unbounded because freeze is reachable.
    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        let worst = ToyControls {
            decay: [ceiling.max_decay(); 3],
            size: Mix(1.0),
            diffusion: Mix(1.0),
            pre_delay: TOY_MAX_PRE_DELAY,
            freeze: false,
        };
        let mut b = Bounds::measure(&self.design(&worst, ceiling.rate()), ceiling.floor());
        b.tail = Ring::Unbounded;
        b
    }
}

impl DesignCase for ToyFdn {
    fn reachable(&self) -> Vec<ToyControls> {
        let mut all = Vec::new();
        for low in [0.2, 1.0, 8.0] {
            for mid in [0.2, 1.0, 8.0] {
                for high in [0.2, 1.0, 8.0] {
                    for size in [0.0, 0.5, 1.0] {
                        for diffusion in [0.0, 1.0] {
                            for freeze in [false, true] {
                                for pre in [0.0, 0.1] {
                                    all.push(ToyControls {
                                        decay: [Seconds(low), Seconds(mid), Seconds(high)],
                                        size: Mix(size),
                                        diffusion: Mix(diffusion),
                                        pre_delay: Seconds(pre),
                                        freeze,
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
            all.push(ToyControls {
                decay: [0; 3].map(|_| Seconds(rng.gen_range(0.05..8.0))),
                size: Mix(rng.gen_range(0.0..=1.0)),
                diffusion: Mix(rng.gen_range(0.0..=1.0)),
                pre_delay: Seconds(rng.gen_range(0.0..=1.0)),
                freeze: rng.gen_bool(0.2),
            });
        }
        all
    }

    fn hostile(&self) -> Vec<ToyControls> {
        let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 40.0, 0.0];
        nasty
            .iter()
            .map(|&v| ToyControls {
                decay: [Seconds::from_f32(v); 3],
                size: Mix(v),
                diffusion: Mix(v),
                pre_delay: Seconds::from_f32(v),
                freeze: false,
            })
            .collect()
    }

    fn musical_delay(&self, n: &Network) -> usize {
        n.min_delay() as usize
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[(Hook::Anchors, "the toy has no closed-form anchor")]
    }
}

/// The toy is analysed, not heard (its realization renders silence), so the
/// measured cases do not run on it; the design cases that read the controls the
/// measured cases would use (`no_rescale`) read these.
impl RealizationCase for ToyFdn {
    fn probes(&self) -> Vec<ToyControls> {
        vec![
            ToyControls::BASE,
            ToyControls {
                decay: [Seconds(3.0), Seconds(2.0), Seconds(1.0)],
                ..ToyControls::BASE
            },
        ]
    }

    fn extremes(&self) -> Vec<ToyControls> {
        vec![ToyControls {
            decay: [Seconds(8.0); 3],
            size: Mix(1.0),
            ..ToyControls::BASE
        }]
    }

    fn steps(&self) -> Vec<(ToyControls, ToyControls)> {
        vec![(
            ToyControls::BASE,
            ToyControls {
                size: Mix(0.9),
                ..ToyControls::BASE
            },
        )]
    }
}

impl LtiCase for ToyFdn {
    fn marginal(&self, c: &ToyControls) -> bool {
        c.freeze
    }
}

impl ReverbCase for ToyFdn {
    fn requested_rt60(&self, c: &ToyControls, rate: SampleRate, band: Band) -> Option<Seconds> {
        // Not where the asked bands would exceed 1: the design scales them.
        let d = |i: usize| ToyFdn::decay(c.decay[i]);
        let n = self.design(c, rate);
        if let Some(Stage::Fdn(f)) = n.stages.first()
            && !c.freeze
            && let Some(l) = f.lines.first()
        {
            let g = [0, 1, 2].map(|i| crate::loop_gain(l.length, d(i), rate).get());
            let pole = [Hz(250.0), Hz(4000.0)].map(|f| {
                libm::exp(-std::f64::consts::TAU * f64::from(f.get()) / rate.get()) as f32
            });
            if crate::fdn::peak_response(g, pole) > 1.0 {
                return None;
            }
        }
        Some(if c.freeze {
            Seconds(f64::INFINITY)
        } else {
            c.decay[band.index()]
        })
    }

    fn frozen(&self, c: &ToyControls) -> bool {
        c.freeze
    }

    fn pre_delay(&self, c: &ToyControls) -> Seconds {
        c.pre_delay
    }

    fn pre_delay_pairs(&self) -> Vec<(ToyControls, ToyControls, Seconds)> {
        let with = ToyControls {
            pre_delay: Seconds(0.1),
            ..ToyControls::BASE
        };
        vec![(ToyControls::BASE, with, Seconds(0.1))]
    }

    fn decay_pairs(&self) -> Vec<(ToyControls, ToyControls)> {
        [(0.2, 1.0), (1.0, 8.0)]
            .map(|(a, b)| {
                (
                    ToyControls {
                        decay: [Seconds(a); 3],
                        ..ToyControls::BASE
                    },
                    ToyControls {
                        decay: [Seconds(b); 3],
                        ..ToyControls::BASE
                    },
                )
            })
            .to_vec()
    }

    fn size_pairs(&self) -> Vec<(ToyControls, ToyControls)> {
        [(0.0, 0.5), (0.5, 1.0)]
            .map(|(a, b)| {
                (
                    ToyControls {
                        size: Mix(a),
                        ..ToyControls::BASE
                    },
                    ToyControls {
                        size: Mix(b),
                        ..ToyControls::BASE
                    },
                )
            })
            .to_vec()
    }

    fn diffusion_pairs(&self) -> Vec<(ToyControls, ToyControls)> {
        vec![(
            ToyControls {
                diffusion: Mix(0.0),
                ..ToyControls::BASE
            },
            ToyControls {
                diffusion: Mix(1.0),
                ..ToyControls::BASE
            },
        )]
    }
}

// ---- the defective realizations the cases must fail ------------------------------------

/// One thing wrong with a realization, added around a real one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Defect {
    /// `retarget` scales every loop gain by this factor.
    LoopGain(f32),
    /// `retarget` drops the damping: every band decays like the lowest.
    NoDamping,
    /// The output is one frame late and `latency` does not say so.
    LateByOne,
    /// `latency` claims one frame more than the realization delays.
    FalseLatency,
    /// The input is added to the first output channel.
    DryLeak,
    /// The second output channel is a copy of the first.
    MonoOutput,
    /// From the second `retarget` on, the output is scaled by this factor, whole.
    UnslewedGain(f32),
    /// `reset` does nothing.
    NoReset,
    /// `fresh` keeps the state.
    FreshKeepsState,
    /// The output is scaled by `1 + 1e-6 * block length`.
    BlockDependent,
    /// Nothing wrong in the wrapper (the realization itself may be tweaked).
    None,
    /// The output is scaled by a gain that grows 0.01% a frame, for ever.
    Runaway,
    /// `retarget` makes the last line 5% longer than the network says.
    LastLineLong,
    /// `retarget` drops the pre-delay.
    PreDelayIgnored,
    /// `retarget` gives every line one length, whatever the size.
    FixedLines,
    /// `retarget` mixes no line into another.
    NoMixing,
    /// `retarget` opens the input of every line, frozen or not.
    OpenInput,
    /// `retarget` sets the stereo width to its natural one.
    WidthIgnored,
    /// `retarget` sets the wet gain to 0 dB.
    WetIgnored,
    /// The first `retarget` goes to a model that is not the target (its wet gain 12
    /// dB down, its lines 20% short and its loop gains halved) and the target is
    /// retargeted to straight after: it ramps from there instead of landing. Only a realization that ramps
    /// (the FDN) shows it.
    RampFirst,
    /// The output goes through `tanh(4 x) / 4`: not linear.
    Tanh,
    /// The output is scaled by `1 + 0.2 sin(2 pi n / 700)` for the frame `n` since
    /// the start: linear and not shift invariant.
    PeriodicGain,
}

/// A real realization with one [`Defect`].
#[derive(Clone, Debug)]
pub(crate) struct Flawed<K> {
    inner: K,
    defect: Defect,
    retargets: usize,
    /// Whether no retarget has come since the realization was built or forked.
    first: bool,
    /// Frames rendered so far, for `PeriodicGain`.
    samples: usize,
    /// The last frame of the previous block, per channel, for `LateByOne`.
    held: [f32; 8],
    /// The gain of `Runaway`.
    runaway: f32,
}

impl<K: Effect<Network, Bounds = Bounds> + Clone> Flawed<K> {
    /// Retargets the inner realization to `network` as `edit` leaves it.
    fn edited(&mut self, network: &Network, edit: impl FnOnce(&mut Network)) {
        let mut n = Box::new(network.clone());
        edit(&mut n);
        self.inner.retarget(&n);
    }
}

impl<K: Effect<Network, Bounds = Bounds> + Clone> Realization<Network> for Flawed<K> {
    type Bounds = Bounds;

    fn prepare(&mut self, bounds: &Bounds, setup: &Setup) {
        self.inner.prepare(bounds, setup);
    }

    fn retarget(&mut self, network: &Network) {
        self.retargets += 1;
        let first = std::mem::replace(&mut self.first, false);
        match self.defect {
            Defect::RampFirst if first => {
                let mut off = network.clone();
                // What a ramp shows: a wet gain and a size that sound before the
                // 20 ms the gains take, and lengths that move at half a frame a
                // chunk (seconds).
                off.wet = Db(off.wet.get() - 12.0);
                map_fdn(&mut off, |f| {
                    f.lines
                        .as_mut_slice()
                        .iter_mut()
                        .for_each(|l| l.length *= 0.8);
                });
                map_absorption(&mut off, |a| match a {
                    Absorption::Bands {
                        low,
                        mid,
                        high,
                        crossover,
                    } => Absorption::Bands {
                        low: low * 0.5,
                        mid: mid * 0.5,
                        high: high * 0.5,
                        crossover,
                    },
                    Absorption::OnePole { gain, damp } => Absorption::OnePole {
                        gain: gain * 0.5,
                        damp,
                    },
                    Absorption::Gain(g) => Absorption::Gain(g * 0.5),
                });
                self.inner.retarget(&off);
                self.inner.retarget(network);
            }
            Defect::LoopGain(factor) => {
                let mut n = network.clone();
                map_absorption(&mut n, |a| match a {
                    Absorption::OnePole { gain, damp } => Absorption::OnePole {
                        gain: gain * factor,
                        damp,
                    },
                    Absorption::Gain(g) => Absorption::Gain(g * factor),
                    Absorption::Bands {
                        low,
                        mid,
                        high,
                        crossover,
                    } => Absorption::Bands {
                        low: low * factor,
                        mid: mid * factor,
                        high: high * factor,
                        crossover,
                    },
                });
                self.inner.retarget(&n);
            }
            Defect::NoDamping => {
                let mut n = network.clone();
                map_absorption(&mut n, |a| match a {
                    Absorption::OnePole { gain, .. } => Absorption::OnePole { gain, damp: 0.0 },
                    // Every band like the lowest.
                    Absorption::Bands { low, crossover, .. } => Absorption::Bands {
                        low,
                        mid: low,
                        high: low,
                        crossover,
                    },
                    a => a,
                });
                self.inner.retarget(&n);
            }
            Defect::LastLineLong => self.edited(network, |n| {
                map_fdn(n, |f| {
                    if let Some(l) = f.lines.as_mut_slice().last_mut() {
                        l.length *= 1.05;
                    }
                })
            }),
            Defect::PreDelayIgnored => self.edited(network, |n| n.pre_delay = Samples::ZERO),
            Defect::FixedLines => self.edited(network, |n| {
                map_fdn(n, |f| {
                    f.lines
                        .as_mut_slice()
                        .iter_mut()
                        .for_each(|l| l.length = 1500.0)
                })
            }),
            Defect::NoMixing => self.edited(network, |n| {
                map_fdn(n, |f| f.matrix = crate::network::Matrix::Diagonal)
            }),
            Defect::OpenInput => self.edited(network, |n| {
                map_fdn(n, |f| f.input.as_mut_slice().fill(0.35))
            }),
            Defect::WidthIgnored => {
                self.edited(network, |n| n.width = tutti_types::StereoWidth::NATURAL)
            }
            Defect::WetIgnored => self.edited(network, |n| n.wet = Db(0.0)),
            _ => self.inner.retarget(network),
        }
    }

    fn reset(&mut self) {
        if self.defect != Defect::NoReset {
            self.inner.reset();
        }
    }

    fn latency(&self) -> Samples {
        let l = self.inner.latency();
        if self.defect == Defect::FalseLatency {
            Samples(l.get() + 1)
        } else {
            l
        }
    }

    fn effort(&self) -> crate::Effort {
        self.inner.effort()
    }

    fn fresh(&self) -> Self {
        if self.defect == Defect::FreshKeepsState {
            return self.clone();
        }
        Self {
            inner: self.inner.fresh(),
            defect: self.defect,
            retargets: self.retargets,
            first: true,
            samples: 0,
            held: [0.0; 8],
            runaway: 1.0,
        }
    }
}

impl<K: Effect<Network, Bounds = Bounds> + Clone> Effect<Network> for Flawed<K> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        self.inner.process(input, output);
        match self.defect {
            Defect::LateByOne => {
                for (c, ch) in output.iter_mut().enumerate() {
                    for s in ch.iter_mut() {
                        std::mem::swap(s, &mut self.held[c]);
                    }
                }
            }
            Defect::DryLeak => {
                for (o, i) in output[0].iter_mut().zip(input[0]) {
                    *o += i;
                }
            }
            Defect::MonoOutput => {
                let (first, rest) = output.split_at_mut(1);
                rest[0].copy_from_slice(first[0]);
            }
            Defect::UnslewedGain(factor) if self.retargets >= 2 => {
                output
                    .iter_mut()
                    .flat_map(|c| c.iter_mut())
                    .for_each(|s| *s *= factor);
            }
            Defect::Tanh => {
                for s in output.iter_mut().flat_map(|c| c.iter_mut()) {
                    *s = libm::tanhf(4.0 * *s) / 4.0;
                }
            }
            Defect::PeriodicGain => {
                for (i, n) in (0..output[0].len()).map(|i| (i, self.samples + i)) {
                    let g = 1.0 + 0.2 * libm::sin(std::f64::consts::TAU * n as f64 / 700.0) as f32;
                    for ch in output.iter_mut() {
                        ch[i] *= g;
                    }
                }
                self.samples += output[0].len();
            }
            Defect::BlockDependent => {
                let len = output[0].len() as f32;
                output
                    .iter_mut()
                    .flat_map(|c| c.iter_mut())
                    .for_each(|s| *s *= 1.0 + 1e-6 * len);
            }
            Defect::Runaway => {
                for i in 0..output[0].len() {
                    self.runaway *= 1.0001;
                    for ch in output.iter_mut() {
                        ch[i] *= self.runaway;
                    }
                }
            }
            _ => {}
        }
    }
}

/// A design whose realization has one [`Defect`].
#[derive(Clone)]
pub(crate) struct Defective<D: NetDesign> {
    pub(crate) inner: D,
    pub(crate) defect: Defect,
    /// Applied to the realization as it is built: a defect in the realization's own state.
    pub(crate) tweak: fn(&mut D::Realization),
}

impl<D: NetDesign> Design for Defective<D> {
    type Controls = D::Controls;
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = Flawed<D::Realization>;

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    fn channels(&self) -> Channels {
        self.inner.channels()
    }

    fn design_into(&self, c: &D::Controls, rate: SampleRate, out: &mut Network) {
        self.inner.design_into(c, rate, out);
    }

    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        self.inner.bounds(ceiling)
    }

    fn realization(&self) -> Flawed<D::Realization> {
        let mut inner = self.inner.realization();
        (self.tweak)(&mut inner);
        Flawed {
            inner,
            defect: self.defect,
            retargets: 0,
            first: true,
            held: [0.0; 8],
            runaway: 1.0,
            samples: 0,
        }
    }
}

impl<D: NetDesign + DesignCase> DesignCase for Defective<D> {
    fn rates(&self) -> Vec<SampleRate> {
        self.inner.rates()
    }
    fn reachable(&self) -> Vec<D::Controls> {
        self.inner.reachable()
    }
    fn hostile(&self) -> Vec<D::Controls> {
        self.inner.hostile()
    }
    fn ceiling(&self, rate: SampleRate) -> Ceiling<MaxDecay> {
        self.inner.ceiling(rate)
    }
    fn musical_delay(&self, model: &Network) -> usize {
        self.inner.musical_delay(model)
    }
    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        self.inner.vacant()
    }
}

impl<D: NetDesign + ReverbCase> ReverbCase for Defective<D> {
    fn requested_rt60(&self, c: &D::Controls, rate: SampleRate, band: Band) -> Option<Seconds> {
        self.inner.requested_rt60(c, rate, band)
    }
    fn frozen(&self, c: &D::Controls) -> bool {
        self.inner.frozen(c)
    }
    fn pre_delay(&self, c: &D::Controls) -> Seconds {
        self.inner.pre_delay(c)
    }
    fn pre_delay_pairs(&self) -> Vec<(D::Controls, D::Controls, Seconds)> {
        self.inner.pre_delay_pairs()
    }
    fn decay_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.decay_pairs()
    }
    fn size_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.size_pairs()
    }
    fn diffusion_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.diffusion_pairs()
    }
    fn wet_only(&self) -> bool {
        self.inner.wet_only()
    }
    fn tolerance(&self) -> f32 {
        self.inner.tolerance()
    }
    fn fit_depth(&self) -> (f64, f64) {
        self.inner.fit_depth()
    }
    fn tone(&self, band: Band, rate: SampleRate) -> f32 {
        self.inner.tone(band, rate)
    }
    fn freeze_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.freeze_pairs()
    }
    fn width_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.width_pairs()
    }
    fn wet_pairs(&self) -> Vec<(D::Controls, D::Controls, f32)> {
        self.inner.wet_pairs()
    }
}

impl<D: NetDesign + RealizationCase> RealizationCase for Defective<D> {
    fn probes(&self) -> Vec<D::Controls> {
        self.inner.probes()
    }
    fn extremes(&self) -> Vec<D::Controls> {
        self.inner.extremes()
    }
    fn steps(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.steps()
    }
    fn static_probes(&self) -> Vec<D::Controls> {
        self.inner.static_probes()
    }
}

impl<D: NetDesign + LtiCase> LtiCase for Defective<D> {
    fn anchors(&self) -> Vec<Anchor<D::Controls>> {
        self.inner.anchors()
    }
    fn marginal(&self, c: &D::Controls) -> bool {
        self.inner.marginal(c)
    }
    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        self.inner.fill(rate, trips)
    }
    fn low_tone(&self, rate: SampleRate) -> f32 {
        self.inner.low_tone(rate)
    }
    fn response_tolerance(&self) -> f64 {
        self.inner.response_tolerance()
    }
    fn decay_tolerance(&self) -> f64 {
        self.inner.decay_tolerance()
    }
    fn tail_slack(&self) -> f64 {
        self.inner.tail_slack()
    }
    fn hinf_slack(&self) -> f64 {
        self.inner.hinf_slack()
    }
}

// ---- a model that lies about its bounds ---------------------------------------

/// A model whose `tail`, `hinf_norm` and `l1_norm` are the inner model's times a factor: a
/// model that lies about a query while its response is the truth, to show the
/// cases that hold a query to the measured system fail.
#[derive(Clone, Debug)]
pub(crate) struct Scaled<M> {
    pub(crate) inner: M,
    pub(crate) tail: f64,
    pub(crate) hinf: f64,
    pub(crate) l1: f64,
}

impl<M: System> System for Scaled<M> {
    fn latency(&self) -> Samples {
        self.inner.latency()
    }

    fn tail(&self, floor: Db) -> Ring {
        match self.inner.tail(floor) {
            Ring::Finite(t) => Ring::Finite(Seconds(t.get() * self.tail)),
            other => other,
        }
    }
}

impl<M> std::borrow::Borrow<M> for Scaled<M> {
    fn borrow(&self) -> &M {
        &self.inner
    }
}

impl<M: Lti> Lti for Scaled<M> {
    fn inputs(&self) -> usize {
        self.inner.inputs()
    }
    fn outputs(&self) -> usize {
        self.inner.outputs()
    }
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [crate::Complex64]) {
        self.inner.frequency_response(output, freqs, gain);
    }
    fn spectral_radius(&self) -> f64 {
        self.inner.spectral_radius()
    }
    fn l1_norm(&self) -> f64 {
        self.inner.l1_norm() * self.l1
    }
    fn hinf_norm(&self) -> f64 {
        self.inner.hinf_norm() * self.hinf
    }
    fn is_stable(&self) -> bool {
        self.inner.is_stable()
    }
}

/// The realization of a [`Lying`] design: the inner one, retargeted to the model
/// inside.
#[derive(Clone, Debug)]
pub(crate) struct Lie<K>(K);

impl<K: Realization<Network>> Realization<Scaled<Network>> for Lie<K> {
    type Bounds = K::Bounds;

    fn prepare(&mut self, bounds: &K::Bounds, setup: &Setup) {
        self.0.prepare(bounds, setup);
    }
    fn retarget(&mut self, model: &Scaled<Network>) {
        self.0.retarget(&model.inner);
    }
    fn reset(&mut self) {
        self.0.reset();
    }
    fn latency(&self) -> Samples {
        self.0.latency()
    }
    fn effort(&self) -> crate::Effort {
        self.0.effort()
    }
    fn fresh(&self) -> Self {
        Lie(self.0.fresh())
    }
}

impl<K: Effect<Network>> Effect<Scaled<Network>> for Lie<K> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        self.0.process(input, output);
    }
}

/// A design whose model's `tail`, `hinf_norm` and `l1_norm` are off by a factor each.
#[derive(Clone)]
pub(crate) struct Lying<D> {
    pub(crate) inner: D,
    pub(crate) tail: f64,
    pub(crate) hinf: f64,
    pub(crate) l1: f64,
}

impl<D: NetDesign> Design for Lying<D> {
    type Controls = D::Controls;
    type Model = Scaled<Network>;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = Lie<D::Realization>;

    fn blank(&self, rate: SampleRate) -> Scaled<Network> {
        Scaled {
            inner: self.inner.blank(rate),
            tail: self.tail,
            hinf: self.hinf,
            l1: self.l1,
        }
    }
    fn channels(&self) -> Channels {
        self.inner.channels()
    }
    fn design_into(&self, c: &D::Controls, rate: SampleRate, out: &mut Scaled<Network>) {
        self.inner.design_into(c, rate, &mut out.inner);
        (out.tail, out.hinf, out.l1) = (self.tail, self.hinf, self.l1);
    }
    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        self.inner.bounds(ceiling)
    }
    fn realization(&self) -> Lie<D::Realization> {
        Lie(self.inner.realization())
    }
}

impl<D: NetDesign + DesignCase> DesignCase for Lying<D> {
    fn rates(&self) -> Vec<SampleRate> {
        self.inner.rates()
    }
    fn reachable(&self) -> Vec<D::Controls> {
        self.inner.reachable()
    }
    fn hostile(&self) -> Vec<D::Controls> {
        self.inner.hostile()
    }
    fn ceiling(&self, rate: SampleRate) -> Ceiling<MaxDecay> {
        self.inner.ceiling(rate)
    }
    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        self.inner.vacant()
    }
}

impl<D: NetDesign + RealizationCase> RealizationCase for Lying<D> {
    fn probes(&self) -> Vec<D::Controls> {
        self.inner.probes()
    }
    fn extremes(&self) -> Vec<D::Controls> {
        self.inner.extremes()
    }
    fn steps(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.steps()
    }
    fn static_probes(&self) -> Vec<D::Controls> {
        self.inner.static_probes()
    }
}

impl<D: NetDesign + LtiCase> LtiCase for Lying<D> {
    fn anchors(&self) -> Vec<Anchor<D::Controls>> {
        self.inner.anchors()
    }
    fn marginal(&self, c: &D::Controls) -> bool {
        self.inner.marginal(c)
    }
    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        self.inner.fill(rate, trips)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::cases::design as cases;

    crate::design_contract!(ToyFdn, Lti, reverb);

    /// Asserts a case failed, with a message containing `why`.
    fn fails(result: Result<(), String>, why: &str) {
        let e = result.expect_err("the case should fail on the defective design");
        println!("{e}");
        assert!(e.contains(why), "{e}");
    }

    fn toy(f: fn(&mut Network)) -> Mutant<ToyFdn> {
        Mutant::new(ToyFdn, f)
    }

    /// Each case fails on a design with the defect its mutation names. These
    /// are the mutation records of the cases, run on every test run.
    #[test]
    fn a_defective_design_fails_its_case() {
        // stable: a gain above 1.
        let m = toy(|n| map_absorption(n, |_| Absorption::Gain(Amplitude(1.02))));
        fails(cases::stable(&m), "unstable");

        // rt60 matches: every gain scaled by 0.98.
        let m = toy(|n| {
            map_absorption(n, |a| match a {
                Absorption::Bands {
                    low,
                    mid,
                    high,
                    crossover,
                } => Absorption::Bands {
                    low: low * 0.98,
                    mid: mid * 0.98,
                    high: high * 0.98,
                    crossover,
                },
                a => a,
            })
        });
        fails(cases::rt60_matches(&m), "rt60");

        // band order: low and high gains swapped.
        let m = toy(|n| {
            map_absorption(n, |a| match a {
                Absorption::Bands {
                    low,
                    mid,
                    high,
                    crossover,
                } => Absorption::Bands {
                    low: high,
                    mid,
                    high: low,
                    crossover,
                },
                a => a,
            })
        });
        fails(cases::band_order(&m), "bands");

        // tail monotone in decay: the gains run backwards (1 - g).
        let m = toy(|n| {
            map_absorption(n, |a| match a {
                Absorption::Bands {
                    low,
                    mid,
                    high,
                    crossover,
                } => Absorption::Bands {
                    low: Amplitude(0.99 - low.get().min(0.99)),
                    mid: Amplitude(0.99 - mid.get().min(0.99)),
                    high: Amplitude(0.99 - high.get().min(0.99)),
                    crossover,
                },
                a => a,
            })
        });
        fails(cases::tail_monotone(&m), "tail fell");

        // lossless iff frozen: one band of a frozen network at 0.999.
        let m = toy(|n| {
            map_absorption(n, |a| match a {
                Absorption::Bands {
                    low,
                    mid,
                    high: Amplitude(1.0),
                    crossover,
                } => Absorption::Bands {
                    low,
                    mid,
                    high: Amplitude(0.999),
                    crossover,
                },
                a => a,
            })
        });
        fails(cases::lossless_iff_frozen(&m), "lossless");

        // density, size: lengths ignore size.
        let m = toy(|n| {
            map_fdn(n, |f| {
                for (l, base) in f.lines.as_mut_slice().iter_mut().zip(TOY_LENGTHS) {
                    l.length = base;
                }
            });
            for s in n.stages.as_mut_slice() {
                if let Stage::AllPass(a) = s {
                    for ap in a.as_mut_slice() {
                        ap.delay = 142.0;
                    }
                }
            }
        });
        fails(cases::density(&m), "modal density");

        // density, diffusion: the matrix is always diagonal.
        let m = toy(|n| map_fdn(n, |f| f.matrix = Matrix::Diagonal));
        fails(cases::density(&m), "echo density");

        // bounds: one line short.
        let mut m = toy(|_| {});
        m.bounds = |b| Bounds {
            lines: b.lines - 1,
            ..b
        };
        fails(cases::bounds_dominate(&m), "does not dominate");

        // pure: the design reads a counter, as a clock would be read.
        let m = toy(|n| {
            thread_local! { static CALLS: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) }; }
            n.wet = Db(CALLS.with(|c| {
                c.set(c.get() + 1.0);
                c.get()
            }));
        });
        fails(cases::pure(&m), "differ");

        // no direct path: a line of length 0.
        let m = toy(|n| map_fdn(n, |f| f.lines.as_mut_slice()[0].length = 0.0));
        fails(cases::no_direct_path(&m), "direct path");

        // pre-delay: one frame more.
        let m = toy(|n| n.pre_delay = Samples(n.pre_delay.get() + 1));
        fails(cases::pre_delay(&m), "pre-delay");
    }

    /// The cases are not vacuous on `ToyFdn`: it asks for RT60s, bands, pairs.
    #[test]
    fn the_toy_design_exercises_every_hook() {
        let d = ToyFdn;
        assert!(!d.decay_pairs().is_empty() && !d.size_pairs().is_empty());
        assert!(!d.diffusion_pairs().is_empty() && !d.pre_delay_pairs().is_empty());
        assert!(d.reachable().iter().any(|c| c.freeze));
        assert!(
            d.requested_rt60(&ToyControls::BASE, SampleRate(48_000.0), Band::Mid)
                .is_some()
        );
    }

    // `loop_gain` is used by `Absorption::from_rt60`; keep the import honest.
    #[test]
    fn the_toy_gains_are_the_formula() {
        let n = ToyFdn.design(&ToyControls::BASE, SampleRate(48_000.0));
        let Stage::Fdn(f) = &n.stages[0] else {
            panic!()
        };
        let l = f.lines[0];
        assert_eq!(
            l.absorption.gain(Band::Mid),
            loop_gain(l.length, Seconds(1.0), SampleRate(48_000.0))
        );
    }
}
