//! The design contract (design 022 section 8.1): cases every [`Design`] must
//! pass, with no kernel and no audio.
//!
//! A design implements [`DesignCase`] (the controls to sweep and the figures it
//! promises, each from physics or the spec, never from another render) and
//! runs [`design_contract!`], which makes one `#[test]` per case. Each case is a
//! plain function from a design to a `Result`, so a test can also hand a case a
//! deliberately broken design ([`Mutant`]) and assert it fails: the mutation
//! record of a case is a test, not only a comment.
//!
//! A case that has nothing to say about a design (Freeverb has no pre-delay, no
//! size and no freeze) is vacuous for it, and the design says so by returning
//! no pairs from the hook. [`ToyFdn`], a test-only design with every control,
//! runs the same cases non-vacuously.
//!
//! Allocation-freedom of `design` is the one case that cannot live here: it
//! needs a counting global allocator, which `unsafe` makes impossible in this
//! `forbid(unsafe_code)` crate's lib tests. `tests/network_alloc.rs` runs it.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{Db, Hz, Mix, SampleRate, Seconds, StereoWidth};

use super::{
    Absorption, AllPass, Band, Bounds, Ceiling, Design, Fdn, Gains, Inline, Line, MAX_LINES,
    Matrix, Modulation, Network, OutputMap, Ring, Stage, loop_gain, pre_delay_frames,
};

/// A design plus what the contract needs to know to test it.
pub(crate) trait DesignCase: Design {
    /// The rates to design at.
    fn rates(&self) -> Vec<SampleRate> {
        vec![
            SampleRate(44_100.0),
            SampleRate(48_000.0),
            SampleRate(96_000.0),
        ]
    }

    /// Controls in range: a grid with the corners, and seeded random draws.
    fn reachable(&self) -> Vec<Self::Controls>;

    /// Controls out of range or non-finite, which `design` must still turn
    /// into a stable network.
    fn hostile(&self) -> Vec<Self::Controls> {
        Vec::new()
    }

    /// The RT60 the controls ask for in `band`, from the formula and the
    /// controls alone (`None`: the controls do not ask).
    fn requested_rt60(
        &self,
        _c: &Self::Controls,
        _rate: SampleRate,
        _band: Band,
    ) -> Option<Seconds> {
        None
    }

    /// Whether the controls ask for the lossless freeze.
    fn frozen(&self, _c: &Self::Controls) -> bool {
        false
    }

    /// The pre-delay the controls ask for.
    fn pre_delay(&self, _c: &Self::Controls) -> Seconds {
        Seconds(0.0)
    }

    /// `(without, with, by)`: the same controls without a pre-delay and with
    /// one of `by`.
    fn pre_delay_pairs(&self) -> Vec<(Self::Controls, Self::Controls, Seconds)> {
        Vec::new()
    }

    /// `(a, b)`: `b` asks for at least the decay of `a`, nothing else changed.
    fn decay_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(a, b)`: `b` is a larger room than `a`, nothing else changed.
    fn size_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(a, b)`: `b` diffuses more than `a`, nothing else changed.
    fn diffusion_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// Whether the design says it is wet-only (no direct path).
    fn wet_only(&self) -> bool {
        true
    }

    /// The ceiling the design is bounded against at `rate`.
    fn ceiling(&self, rate: SampleRate) -> Ceiling {
        Ceiling::at(rate).expect("a positive finite rate")
    }
}

fn every<D: DesignCase>(d: &D) -> Vec<D::Controls> {
    let mut all = d.reachable();
    all.extend(d.hostile());
    all
}

fn close(a: f32, b: f32, relative: f32) -> bool {
    a == b || (a - b).abs() <= relative * a.abs().max(b.abs())
}

pub(crate) mod cases {
    use super::*;

    /// Every reachable (and hostile) control gives a stable network.
    ///
    /// Mutation (a `Mutant` test): a gain above 1 on every line.
    pub(crate) fn stable<D: DesignCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in every(d) {
                let n = d.design(&c, rate);
                if !n.is_stable() {
                    return Err(format!("unstable at {rate:?}: {n:?}"));
                }
            }
        }
        Ok(())
    }

    /// `rt60(band)` equals the RT60 the controls ask for, per
    /// `g = 10^(-3m/(RT60 fs))`, within 1e-3 relative. The design must ask for
    /// at least one.
    ///
    /// Mutation: every gain scaled by 1.02.
    pub(crate) fn rt60_matches<D: DesignCase>(d: &D) -> Result<(), String> {
        let mut asked = 0;
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                for band in Band::ALL {
                    if let Some(want) = d.requested_rt60(&c, rate, band) {
                        asked += 1;
                        let got = n.rt60(band);
                        if !close(got.get(), want.get(), 1e-3) {
                            return Err(format!(
                                "{band:?} at {rate:?}: rt60 {got:?}, asked {want:?}"
                            ));
                        }
                    }
                }
            }
        }
        if asked == 0 {
            Err("no control asks for an RT60".into())
        } else {
            Ok(())
        }
    }

    /// The bands' RT60s are ordered as the controls ask: for every pair of
    /// bands, the order of the requested figures is the order of the network's.
    ///
    /// Mutation: two bands' gains swapped.
    pub(crate) fn band_order<D: DesignCase>(d: &D) -> Result<(), String> {
        let mut checked = 0;
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let want: Vec<Option<Seconds>> = Band::ALL
                    .iter()
                    .map(|&b| d.requested_rt60(&c, rate, b))
                    .collect();
                if want.iter().any(Option::is_none) {
                    continue;
                }
                let want: Vec<f32> = want.iter().map(|w| w.unwrap().get()).collect();
                let got: Vec<f32> = Band::ALL.iter().map(|&b| n.rt60(b).get()).collect();
                for (i, j) in [(0, 1), (1, 2), (0, 2)] {
                    checked += 1;
                    let ordered = |v: &[f32]| {
                        if close(v[i], v[j], 1e-3) {
                            std::cmp::Ordering::Equal
                        } else {
                            v[i].total_cmp(&v[j])
                        }
                    };
                    if ordered(&want) != ordered(&got) {
                        return Err(format!(
                            "bands {i},{j} at {rate:?}: asked {want:?}, got {got:?}"
                        ));
                    }
                }
            }
        }
        if checked == 0 {
            Err("no control asks for band RT60s".into())
        } else {
            Ok(())
        }
    }

    /// The tail is non-decreasing in decay and in size, a lower floor is longer,
    /// and a pre-delay adds its length to the tail and no more.
    ///
    /// Mutations: the decay term of `tail` dropped (pairs); the pre-delay term
    /// of `tail` dropped (run by hand in `queries.rs`).
    pub(crate) fn tail_monotone<D: DesignCase>(d: &D) -> Result<(), String> {
        let floor = Db(-60.0);
        for rate in d.rates() {
            let tail = |c: &D::Controls, f: Db| d.design(c, rate).tail(f);
            for (a, b) in d.decay_pairs().into_iter().chain(d.size_pairs()) {
                let (ta, tb) = (tail(&a, floor), tail(&b, floor));
                match (ta, tb) {
                    (Ring::Finite(x), Ring::Finite(y)) if y.get() >= x.get() * (1.0 - 1e-6) => {}
                    (_, Ring::Unbounded) => {}
                    _ => return Err(format!("tail fell from {ta:?} to {tb:?} at {rate:?}")),
                }
            }
            for c in d.reachable() {
                let (t40, t60, t90) = (tail(&c, Db(-40.0)), tail(&c, floor), tail(&c, Db(-90.0)));
                if !(t60.covers(t40) && t90.covers(t60)) {
                    return Err(format!("floors not ordered: {t40:?} {t60:?} {t90:?}"));
                }
            }
            for (without, with, by) in d.pre_delay_pairs() {
                let (Ring::Finite(x), Ring::Finite(y)) =
                    (tail(&without, floor), tail(&with, floor))
                else {
                    return Err("an unbounded tail in a pre-delay pair".into());
                };
                let added = f64::from(y.get()) - f64::from(x.get());
                let want = f64::from(by.to_samples(rate).get() as u32) / rate.get();
                if (added - want).abs() > 2e-6 + 1e-6 * f64::from(y.get()) {
                    return Err(format!("pre-delay added {added} s, not {want} s"));
                }
            }
        }
        Ok(())
    }

    /// The network is lossless exactly when the controls freeze it, and its tail
    /// is unbounded then and only then (a freeze is still stable). Vacuous in
    /// the lossless direction for a design that cannot freeze: it still asserts
    /// that no control gives a lossless loop.
    ///
    /// Mutation: one band left at 0.999 in a frozen network.
    pub(crate) fn lossless_iff_frozen<D: DesignCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let frozen = d.frozen(&c);
                if n.is_lossless() != frozen {
                    return Err(format!("lossless {} but frozen {frozen}", n.is_lossless()));
                }
                if (n.tail(Db(-60.0)) == Ring::Unbounded) != frozen {
                    return Err(format!("tail {:?} but frozen {frozen}", n.tail(Db(-60.0))));
                }
                if !n.is_stable() {
                    return Err("unstable".into());
                }
            }
        }
        Ok(())
    }

    /// Modal density is positive, finite and grows with size; echo density never
    /// falls with time and grows with diffusion: orderings, not absolute figures.
    ///
    /// Mutations: line lengths made independent of size; a mixing matrix made
    /// diagonal.
    pub(crate) fn density<D: DesignCase>(d: &D) -> Result<(), String> {
        let at = |s: f32| Seconds(s);
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let m = n.modal_density().get();
                if !(m.is_finite() && m > 0.0) {
                    return Err(format!("modal density {m}"));
                }
                let mut last = 0.0;
                for t in [0.001, 0.01, 0.1, 1.0, 10.0] {
                    let e = n.echo_density(at(t)).get();
                    if e < last {
                        return Err(format!("echo density fell to {e} at {t} s"));
                    }
                    last = e;
                }
            }
            for (a, b) in d.size_pairs() {
                let (ma, mb) = (
                    d.design(&a, rate).modal_density(),
                    d.design(&b, rate).modal_density(),
                );
                if mb.get() <= ma.get() {
                    return Err(format!("modal density {ma:?} -> {mb:?} for a bigger room"));
                }
            }
            for (a, b) in d.diffusion_pairs() {
                let (ea, eb) = (
                    d.design(&a, rate).echo_density(at(1.0)),
                    d.design(&b, rate).echo_density(at(1.0)),
                );
                if eb.get() <= ea.get() {
                    return Err(format!("echo density {ea:?} -> {eb:?} for more diffusion"));
                }
            }
        }
        Ok(())
    }

    /// `bounds()` dominates `design()` in every figure for every control.
    ///
    /// Mutation: `bounds` one line short.
    pub(crate) fn bounds_dominate<D: DesignCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            let ceiling = d.ceiling(rate);
            let bound = d.bounds(&ceiling);
            for c in every(d) {
                let n = d.design(&c, rate);
                let size = Bounds::of(&n, ceiling.floor());
                if !bound.dominates(&size) {
                    return Err(format!("{bound:?} does not dominate {size:?}"));
                }
            }
        }
        Ok(())
    }

    /// The same controls give the same network, however often they are asked.
    ///
    /// Mutation: the design reads a counter (a stand-in for a clock). The
    /// no-allocation half is `tests/network_alloc.rs`.
    pub(crate) fn pure<D: DesignCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in every(d) {
                let (a, b) = (d.design(&c, rate), d.design(&c, rate));
                if a != b || a != d.clone().design(&c, rate) {
                    return Err("two designs of the same controls differ".into());
                }
            }
        }
        Ok(())
    }

    /// A design that says it is wet-only has no path from input to output
    /// shorter than a frame.
    ///
    /// Mutation: a line of length 0.
    pub(crate) fn no_direct_path<D: DesignCase>(d: &D) -> Result<(), String> {
        if !d.wet_only() {
            return Ok(());
        }
        for rate in d.rates() {
            for c in every(d) {
                let n = d.design(&c, rate);
                if n.has_direct_path() {
                    return Err(format!("direct path, shortest {} frames", n.min_delay()));
                }
            }
        }
        Ok(())
    }

    /// `pre_delay` is `round(pre_delay seconds * rate)` frames, for every control
    /// in range (a hostile one is clamped to the design's own range).
    ///
    /// Mutation: one frame more.
    pub(crate) fn pre_delay<D: DesignCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let asked = d.pre_delay(&c);
                let want = if asked.get().is_finite() && asked.get() > 0.0 {
                    (f64::from(asked.get()) * rate.get()).round() as f32
                } else {
                    0.0
                };
                if n.pre_delay != want {
                    return Err(format!(
                        "pre-delay {} frames, asked {want} ({asked:?})",
                        n.pre_delay
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Makes one `#[test]` per case of the design contract for `$design`, an
/// expression of a [`DesignCase`] type.
macro_rules! design_contract {
    ($design:expr) => {
        #[test]
        fn contract_stable() {
            $crate::network::contract::cases::stable(&$design).unwrap();
        }
        #[test]
        fn contract_rt60_matches() {
            $crate::network::contract::cases::rt60_matches(&$design).unwrap();
        }
        #[test]
        fn contract_band_order() {
            $crate::network::contract::cases::band_order(&$design).unwrap();
        }
        #[test]
        fn contract_tail_monotone() {
            $crate::network::contract::cases::tail_monotone(&$design).unwrap();
        }
        #[test]
        fn contract_lossless_iff_frozen() {
            $crate::network::contract::cases::lossless_iff_frozen(&$design).unwrap();
        }
        #[test]
        fn contract_density() {
            $crate::network::contract::cases::density(&$design).unwrap();
        }
        #[test]
        fn contract_bounds_dominate() {
            $crate::network::contract::cases::bounds_dominate(&$design).unwrap();
        }
        #[test]
        fn contract_pure() {
            $crate::network::contract::cases::pure(&$design).unwrap();
        }
        #[test]
        fn contract_no_direct_path() {
            $crate::network::contract::cases::no_direct_path(&$design).unwrap();
        }
        #[test]
        fn contract_pre_delay() {
            $crate::network::contract::cases::pre_delay(&$design).unwrap();
        }
    };
}
pub(crate) use design_contract;

/// A design with one deliberate defect, to show a case fails on it.
#[derive(Clone)]
pub(crate) struct Mutant<D> {
    pub(crate) inner: D,
    pub(crate) network: fn(&mut Network),
    pub(crate) bounds: fn(Bounds) -> Bounds,
}

impl<D> Mutant<D> {
    pub(crate) fn new(inner: D, network: fn(&mut Network)) -> Self {
        Self {
            inner,
            network,
            bounds: |b| b,
        }
    }
}

impl<D: Design> Design for Mutant<D> {
    type Controls = D::Controls;

    fn design(&self, controls: &D::Controls, rate: SampleRate) -> Network {
        let mut n = self.inner.design(controls, rate);
        (self.network)(&mut n);
        n
    }

    fn bounds(&self, ceiling: &Ceiling) -> Bounds {
        (self.bounds)(self.inner.bounds(ceiling))
    }
}

impl<D: DesignCase> DesignCase for Mutant<D> {
    fn rates(&self) -> Vec<SampleRate> {
        self.inner.rates()
    }
    fn reachable(&self) -> Vec<D::Controls> {
        self.inner.reachable()
    }
    fn hostile(&self) -> Vec<D::Controls> {
        self.inner.hostile()
    }
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
    fn decay(s: Seconds) -> f32 {
        let v = s.get();
        if v.is_nan() {
            1.0
        } else {
            v.clamp(0.05, Ceiling::MAX_DECAY.get())
        }
    }
}

impl Design for ToyFdn {
    type Controls = ToyControls;

    fn design(&self, c: &ToyControls, rate: SampleRate) -> Network {
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
            [Seconds(f32::INFINITY); 3]
        } else {
            c.decay.map(|s| Seconds(Self::decay(s)))
        };
        let scale = (0.5 + size) * (rate.get() / 44_100.0) as f32;
        let mut lines: Inline<Line, MAX_LINES> = Inline::new();
        for base in TOY_LENGTHS {
            let length = base * scale;
            let _ = lines.push(Line {
                length,
                modulation: Modulation::NONE,
                absorption: Absorption::from_rt60(rt60, [Hz(250.0), Hz(4000.0)], length, rate),
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
        };
        let mut channels = Inline::new();
        let _ = channels.push(ap);
        let _ = channels.push(ap);
        let _ = network.stages.push(Stage::AllPass(channels));
        network.width = StereoWidth::NATURAL;
        network.wet = Db(0.0);
        network
    }

    /// The largest design (size 1, the longest decay, the longest pre-delay);
    /// the tail is unbounded because freeze is reachable.
    fn bounds(&self, ceiling: &Ceiling) -> Bounds {
        let worst = ToyControls {
            decay: [ceiling.max_decay(); 3],
            size: Mix(1.0),
            diffusion: Mix(1.0),
            pre_delay: TOY_MAX_PRE_DELAY,
            freeze: false,
        };
        let mut b = Bounds::of(&self.design(&worst, ceiling.rate()), ceiling.floor());
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
                decay: [Seconds(v); 3],
                size: Mix(v),
                diffusion: Mix(v),
                pre_delay: Seconds(v),
                freeze: false,
            })
            .collect()
    }

    fn requested_rt60(&self, c: &ToyControls, _rate: SampleRate, band: Band) -> Option<Seconds> {
        Some(if c.freeze {
            Seconds(f32::INFINITY)
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

#[cfg(test)]
mod tests {
    use super::*;

    design_contract!(ToyFdn);

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
        let m = toy(|n| map_absorption(n, |_| Absorption::Gain(1.02)));
        fails(cases::stable(&m), "unstable");

        // rt60 matches: every gain scaled by 1.02.
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
                    low: 0.99 - low.min(0.99),
                    mid: 0.99 - mid.min(0.99),
                    high: 0.99 - high.min(0.99),
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
                    high: 1.0,
                    crossover,
                } => Absorption::Bands {
                    low,
                    mid,
                    high: 0.999,
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
        let m = toy(|n| n.pre_delay += 1.0);
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
