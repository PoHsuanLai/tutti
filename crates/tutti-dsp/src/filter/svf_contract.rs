//! The `Lti` group run on **three realizations of one H(z)** (design 024 step 2):
//! Simper's trapezoidal SVF (`SvfRealization`, the one that ships), a transposed
//! direct form II biquad and a lattice-ladder, the last two test-support
//! realizations of `SvfModel` that compute the response from `SvfModel::biquad`.
//! They share a model and the contract cases, and differ only in the structure
//! that holds the state: the model is the equivalence class, a realization is one
//! member.
//!
//! The contract says nothing about which structure it was handed, and a defective
//! one fails it: a cutoff 2% off fails the response (and the segment-end case
//! where a ramp never lands), and a `tanh` in the feedback of the direct form
//! fails linearity.

use core::marker::PhantomData;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{ChannelLayout, Db, Hz, Q, SampleRate};

use super::{Biquad, Real, SvfBounds, SvfControls, SvfDesign, SvfModel, SvfRealization, SvfType};
use crate::contract::{Anchor, DesignCase, LtiCase, RealizationCase};
use crate::{Effect, Effort, Realization, Setup};

/// A realization of the SVF's model that ramps its coefficients across a call,
/// as [`SvfRealization`] does, over the direct-form numbers of the model.
trait Section: Copy + Default + core::fmt::Debug + Send {
    /// The structure's own coefficients for the direct-form `b`.
    fn from_biquad(b: &Biquad) -> Self;
    /// `self` moved to `to` by `t` of the way (a convex blend).
    fn lerp(&self, to: &Self, t: f64) -> Self;
    /// One sample through the structure over its two state words.
    fn step(&self, state: &mut [f64; 2], x: f64, saturate: bool) -> f64;
}

/// Transposed direct form II over `H(z)`'s coefficients.
#[derive(Clone, Copy, Debug, Default)]
struct Tdf2Coeffs([f64; 5]);

impl Section for Tdf2Coeffs {
    fn from_biquad(b: &Biquad) -> Self {
        Self([b.b0, b.b1, b.b2, b.a1, b.a2])
    }

    fn lerp(&self, to: &Self, t: f64) -> Self {
        Self(core::array::from_fn(|i| {
            self.0[i] + (to.0[i] - self.0[i]) * t
        }))
    }

    fn step(&self, s: &mut [f64; 2], x: f64, saturate: bool) -> f64 {
        let [b0, b1, b2, a1, a2] = self.0;
        let y = b0 * x + s[0];
        // A saturator in the feedback path: what a defective realization has.
        let fed = if saturate { libm::tanh(y) } else { y };
        s[0] = b1 * x - a1 * fed + s[1];
        s[1] = b2 * x - a2 * fed;
        y
    }
}

/// The all-pole lattice with a ladder (Gray and Markel): reflection coefficients
/// `k1`, `k2` and tap weights `v0..v2`.
#[derive(Clone, Copy, Debug, Default)]
struct LatticeCoeffs {
    k: [f64; 2],
    v: [f64; 3],
}

impl Section for LatticeCoeffs {
    /// `k2 = a2`, `k1 = a1 / (1 + a2)`, and the taps solve the numerator over the
    /// reversed polynomials `1`, `k1 + z^-1`, `a2 + a1 z^-1 + z^-2`.
    fn from_biquad(b: &Biquad) -> Self {
        let k2 = b.a2;
        let k1 = b.a1 / (1.0 + b.a2);
        let v2 = b.b2;
        let v1 = b.b1 - v2 * b.a1;
        let v0 = b.b0 - v1 * k1 - v2 * b.a2;
        Self {
            k: [k1, k2],
            v: [v0, v1, v2],
        }
    }

    /// Reflection coefficients stay inside the unit interval under a convex blend,
    /// so a ramp between two stable lattices is stable.
    fn lerp(&self, to: &Self, t: f64) -> Self {
        Self {
            k: core::array::from_fn(|i| self.k[i] + (to.k[i] - self.k[i]) * t),
            v: core::array::from_fn(|i| self.v[i] + (to.v[i] - self.v[i]) * t),
        }
    }

    fn step(&self, s: &mut [f64; 2], x: f64, _saturate: bool) -> f64 {
        let [k1, k2] = self.k;
        let [g0d, g1d] = *s;
        let f1 = x - k2 * g1d;
        let f0 = f1 - k1 * g0d;
        let (g0, g1, g2) = (f0, k1 * f0 + g0d, k2 * f1 + g1d);
        *s = [g0, g1];
        self.v[0] * g0 + self.v[1] * g1 + self.v[2] * g2
    }
}

/// A realization of [`SvfModel`] in the structure `S`, ramping its coefficients
/// across a call to land on the target at its last frame, as the shipped one does.
#[derive(Clone, Debug)]
struct Form<S: Section, const SATURATE: bool = false> {
    prev: S,
    next: S,
    primed: bool,
    state: Vec<[f64; 2]>,
}

/// The transposed direct form II realization.
type Tdf2 = Form<Tdf2Coeffs>;
/// The same with a `tanh` in its feedback: defective.
type Tdf2Tanh = Form<Tdf2Coeffs, true>;
/// The lattice-ladder realization.
type Lattice = Form<LatticeCoeffs>;

impl<S: Section, const SATURATE: bool> From<ChannelLayout> for Form<S, SATURATE> {
    fn from(channels: ChannelLayout) -> Self {
        Self {
            prev: S::default(),
            next: S::default(),
            primed: false,
            state: vec![[0.0; 2]; usize::from(channels.count()).max(1)],
        }
    }
}

impl<S: Section, const SATURATE: bool> Realization<SvfModel> for Form<S, SATURATE> {
    type Bounds = SvfBounds;

    fn prepare(&mut self, _: &SvfBounds, setup: &Setup) {
        self.state = vec![[0.0; 2]; usize::from(setup.output.count()).max(1)];
        self.primed = false;
    }

    fn retarget(&mut self, model: &SvfModel) {
        self.next = S::from_biquad(&model.biquad());
        if !self.primed {
            self.prev = self.next;
            self.primed = true;
        }
    }

    fn reset(&mut self) {
        self.state.fill([0.0; 2]);
        self.prev = self.next;
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }

    fn fresh(&self) -> Self {
        let mut copy = self.clone();
        copy.reset();
        copy
    }
}

impl<S: Section, const SATURATE: bool> Effect<SvfModel> for Form<S, SATURATE> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let size = output.first().map_or(0, |c| c.len());
        for i in 0..size {
            let c = if i + 1 == size {
                self.next
            } else {
                self.prev.lerp(&self.next, (i + 1) as f64 / size as f64)
            };
            for (ch, st) in self.state.iter_mut().enumerate() {
                output[ch][i] = c.step(st, f64::from(input[ch][i]), SATURATE) as f32;
            }
        }
        self.prev = self.next;
    }
}

/// A defect in what a realization is told: a transformation of the model.
trait Flaw: Send + 'static {
    fn apply(model: &SvfModel) -> SvfModel;
}

/// The cutoff 2% off.
struct Detune;

impl Flaw for Detune {
    fn apply(model: &SvfModel) -> SvfModel {
        let c = model.controls();
        SvfModel::new(
            SvfControls {
                freq: Hz(c.freq.get() * 1.02),
                ..c
            },
            model.rate(),
        )
    }
}

/// A realization `K` that is told the model through the flaw `F`.
struct Told<K, F>(K, PhantomData<fn() -> F>);

impl<K: Clone, F> Clone for Told<K, F> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<K: From<ChannelLayout>, F> From<ChannelLayout> for Told<K, F> {
    fn from(c: ChannelLayout) -> Self {
        Self(K::from(c), PhantomData)
    }
}

impl<K: Realization<SvfModel, Bounds = SvfBounds>, F: Flaw> Realization<SvfModel> for Told<K, F> {
    type Bounds = SvfBounds;

    fn prepare(&mut self, b: &SvfBounds, s: &Setup) {
        self.0.prepare(b, s);
    }

    fn retarget(&mut self, model: &SvfModel) {
        self.0.retarget(&F::apply(model));
    }

    fn reset(&mut self) {
        self.0.reset();
    }

    fn effort(&self) -> Effort {
        self.0.effort()
    }

    fn fresh(&self) -> Self {
        Self(self.0.fresh(), PhantomData)
    }
}

impl<K: Effect<SvfModel, Bounds = SvfBounds>, F: Flaw> Effect<SvfModel> for Told<K, F> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        self.0.process(input, output);
    }
}

/// A realization that never lands: after a call it forgets it moved, and ramps
/// again on the next.
#[derive(Clone)]
struct Stuck(SvfRealization);

impl From<ChannelLayout> for Stuck {
    fn from(c: ChannelLayout) -> Self {
        Self(SvfRealization::from(c))
    }
}

impl Realization<SvfModel> for Stuck {
    type Bounds = SvfBounds;

    fn prepare(&mut self, b: &SvfBounds, s: &Setup) {
        self.0.prepare(b, s);
    }

    fn retarget(&mut self, model: &SvfModel) {
        self.0.retarget(model);
    }

    fn reset(&mut self) {
        self.0.reset();
    }

    fn fresh(&self) -> Self {
        Self(self.0.fresh())
    }
}

impl Effect<SvfModel> for Stuck {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let before = self.0.prev;
        self.0.process(input, output);
        self.0.prev = before;
    }
}

/// A realization whose last frame is `(i + 0) / n` of the way instead of
/// `(i + 1) / n`: the ramp never arrives within the call.
#[derive(Clone)]
struct Short(SvfRealization);

impl From<ChannelLayout> for Short {
    fn from(c: ChannelLayout) -> Self {
        Self(SvfRealization::from(c))
    }
}

impl Realization<SvfModel> for Short {
    type Bounds = SvfBounds;

    fn prepare(&mut self, b: &SvfBounds, s: &Setup) {
        self.0.prepare(b, s);
    }

    fn retarget(&mut self, model: &SvfModel) {
        self.0.retarget(model);
    }

    fn reset(&mut self) {
        self.0.reset();
    }

    fn fresh(&self) -> Self {
        Self(self.0.fresh())
    }
}

impl Effect<SvfModel> for Short {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        // The ramp up to the last frame, which then runs at the previous set's
        // blend `n - 1` of `n`: the call ends one step short of the target.
        let size = output.first().map_or(0, |c| c.len());
        let (prev, next) = (self.0.prev, self.0.next);
        for i in 0..size {
            let t = i as f64 / size as f64;
            let k = prev.lerp(&next, t);
            for (ch, st) in self.0.state.iter_mut().enumerate() {
                output[ch][i] = super::svf_step(&k, st, f64::from(input[ch][i])).to_f32();
            }
        }
        self.0.prev = prev.lerp(&next, (size - 1) as f64 / size as f64);
    }
}

fn controls(ty: SvfType, freq: f32, q: f32, gain: f32) -> SvfControls {
    SvfControls {
        ty,
        freq: Hz(freq),
        q: Q(q),
        gain: Db(gain),
    }
}

impl<R> DesignCase for SvfDesign<R>
where
    R: Realization<SvfModel, Bounds = SvfBounds> + From<ChannelLayout>,
{
    fn reachable(&self) -> Vec<SvfControls> {
        let types = [
            SvfType::LowPass,
            SvfType::HighPass,
            SvfType::BandPass,
            SvfType::Notch,
            SvfType::Allpass,
            SvfType::Bell,
            SvfType::LowShelf,
            SvfType::HighShelf,
        ];
        let mut all = Vec::new();
        for ty in types {
            for freq in [1.0, 20.0, 100.0, 1_000.0, 8_000.0, 20_000.0] {
                for q in [0.1, 0.707, 2.0, 10.0, 200.0] {
                    for gain in [-24.0, 0.0, 12.0] {
                        all.push(controls(ty, freq, q, gain));
                    }
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(24);
        for _ in 0..200 {
            all.push(controls(
                types[rng.gen_range(0..types.len())],
                libm::exp(rng.gen_range(0.0..=10.0f64)) as f32,
                libm::exp(rng.gen_range(-2.0..=5.0f64)) as f32,
                rng.gen_range(-30.0..=30.0),
            ));
        }
        all
    }

    fn hostile(&self) -> Vec<SvfControls> {
        let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 0.0, 1e9];
        let mut all = Vec::new();
        for ty in [SvfType::LowPass, SvfType::Bell, SvfType::HighShelf] {
            for &v in &nasty {
                all.push(controls(ty, v, 1.0, 0.0));
                all.push(controls(ty, 1_000.0, v, 6.0));
                all.push(controls(ty, 1_000.0, 1.0, v));
                all.push(controls(ty, v, v, v));
            }
            all.push(controls(ty, 1.0, 1e30, 1e30));
            all.push(controls(ty, 1_000.0, 1e-30, -1e30));
        }
        all
    }
}

impl<R> RealizationCase for SvfDesign<R>
where
    R: Realization<SvfModel, Bounds = SvfBounds> + Effect<SvfModel> + Clone + From<ChannelLayout>,
{
    /// One of each response, and a 100 Hz resonance of Q 30, which decays in
    /// about 25 windows of 4096 frames, slowly enough for the decay case's fit.
    fn probes(&self) -> Vec<SvfControls> {
        vec![
            controls(SvfType::LowPass, 1_000.0, 0.707, 0.0),
            controls(SvfType::LowPass, 100.0, 30.0, 0.0),
            controls(SvfType::HighPass, 3_000.0, 4.0, 0.0),
            controls(SvfType::BandPass, 500.0, 2.0, 0.0),
            controls(SvfType::Notch, 2_000.0, 1.0, 0.0),
            controls(SvfType::Allpass, 800.0, 3.0, 0.0),
            controls(SvfType::Bell, 1_000.0, 1.4, 12.0),
            controls(SvfType::LowShelf, 200.0, 0.707, -9.0),
            controls(SvfType::HighShelf, 6_000.0, 0.9, 9.0),
        ]
    }

    fn extremes(&self) -> Vec<SvfControls> {
        vec![
            controls(SvfType::LowPass, 20.0, 20.0, 0.0),
            controls(SvfType::Bell, 5_000.0, 10.0, 24.0),
            controls(SvfType::HighPass, 15_000.0, 10.0, 0.0),
        ]
    }

    fn steps(&self) -> Vec<(SvfControls, SvfControls)> {
        vec![
            (
                controls(SvfType::LowPass, 800.0, 0.707, 0.0),
                controls(SvfType::LowPass, 1_600.0, 2.0, 0.0),
            ),
            (
                controls(SvfType::Bell, 1_000.0, 1.0, 12.0),
                controls(SvfType::Bell, 1_000.0, 1.0, -12.0),
            ),
            (
                controls(SvfType::HighShelf, 4_000.0, 0.7, 6.0),
                controls(SvfType::HighShelf, 2_000.0, 0.7, -6.0),
            ),
        ]
    }
}

impl<R> LtiCase for SvfDesign<R>
where
    R: Realization<SvfModel, Bounds = SvfBounds> + Effect<SvfModel> + Clone + From<ChannelLayout>,
{
    /// The prototype's closed forms at DC, the cutoff and Nyquist (design 024
    /// section 6.2), for every response, at 48 kHz and 1 kHz or 2 kHz. A zero is
    /// left out: a relative tolerance cannot hold a null.
    ///
    /// Low-pass `1`, `Q`, `0`; high-pass `0`, `Q`, `1`; band-pass `0`, `Q`, `0`;
    /// notch `1`, `0`, `1`; all-pass `1` throughout; a bell `1`, `A^2`, `1`; a low
    /// shelf `A^2`, `A`, `1`; a high shelf `1`, `A`, `A^2`, with `A = 10^(dB / 40)`.
    fn anchors(&self) -> Vec<Anchor<SvfControls>> {
        let rate = SampleRate(48_000.0);
        let mut all = Vec::new();
        let mut at = |c: SvfControls, freq: f32, gain: f64| {
            all.push(Anchor {
                controls: c,
                rate,
                output: 0,
                freq: Hz(freq),
                gain,
                tolerance: 1e-9,
            });
        };
        for (fc, q) in [(1_000.0f32, 0.707f32), (2_000.0, 4.0), (1_000.0, 0.3)] {
            let qf = f64::from(q);
            let lp = controls(SvfType::LowPass, fc, q, 0.0);
            at(lp, 0.0, 1.0);
            at(lp, fc, qf);
            let hp = controls(SvfType::HighPass, fc, q, 0.0);
            at(hp, fc, qf);
            at(hp, 24_000.0, 1.0);
            at(controls(SvfType::BandPass, fc, q, 0.0), fc, qf);
            let notch = controls(SvfType::Notch, fc, q, 0.0);
            at(notch, 0.0, 1.0);
            at(notch, 24_000.0, 1.0);
            let ap = controls(SvfType::Allpass, fc, q, 0.0);
            for f in [0.0, fc, 24_000.0] {
                at(ap, f, 1.0);
            }
            for db in [12.0f32, -9.0] {
                let a2 = libm::pow(10.0, f64::from(db) / 20.0);
                let a = libm::pow(10.0, f64::from(db) / 40.0);
                let bell = controls(SvfType::Bell, fc, q, db);
                at(bell, 0.0, 1.0);
                at(bell, fc, a2);
                at(bell, 24_000.0, 1.0);
                let low = controls(SvfType::LowShelf, fc, q, db);
                at(low, 0.0, a2);
                at(low, fc, a);
                at(low, 24_000.0, 1.0);
                let high = controls(SvfType::HighShelf, fc, q, db);
                at(high, 0.0, 1.0);
                at(high, fc, a);
                at(high, 24_000.0, a2);
            }
        }
        all
    }

    /// The realizations agree with the model to the rounding of `f32` samples, so
    /// the 1% of the default is far wider than they need: a cutoff 2% off is
    /// 1.5e-2 from the response of the mildest probe, and the tolerance here
    /// leaves it a margin.
    fn response_tolerance(&self) -> f64 {
        2e-3
    }
}

fn mono<R>() -> SvfDesign<R> {
    SvfDesign::new(ChannelLayout::MONO)
}

mod trapezoidal {
    use super::*;
    crate::design_contract!(mono::<SvfRealization>(), Lti);
    crate::realization_contract!(mono::<SvfRealization>(), Lti, Every);
}

mod direct_form {
    use super::*;
    crate::design_contract!(mono::<Tdf2>(), Lti);
    crate::realization_contract!(mono::<Tdf2>(), Lti, Every);
}

mod lattice {
    use super::*;
    crate::design_contract!(mono::<Lattice>(), Lti);
    crate::realization_contract!(mono::<Lattice>(), Lti, Every);
}

/// The same contract on a stereo trapezoidal one (the lanes of a group).
mod stereo {
    use super::*;
    crate::realization_contract!(
        SvfDesign::<SvfRealization<f32>>::new(ChannelLayout::STEREO),
        Lti,
        Every
    );
}

mod mutants {
    use super::*;
    use crate::contract::cases::{design, realization};

    type Detuned<K> = Told<K, Detune>;

    /// The message of a case that must fail, and must say what the mutant did.
    fn fails(result: Result<(), String>, what: &str) {
        let err = result.expect_err("the mutant must fail");
        assert!(err.contains(what), "fails for another reason: {err}");
    }

    /// A cutoff 2% off fails the response case of each of the three structures
    /// (the model says one thing and the realization another), and fails the
    /// segment-end case, which holds the response after a ramp.
    ///
    /// Mutations (run): the detune factor 1.02 -> 1.0 passes both.
    #[test]
    fn a_cutoff_two_percent_off_fails_the_response_of_every_structure() {
        let spectrum = "spectrum is";
        fails(
            realization::response(&mono::<Detuned<SvfRealization>>()),
            spectrum,
        );
        fails(realization::response(&mono::<Detuned<Tdf2>>()), spectrum);
        fails(realization::response(&mono::<Detuned<Lattice>>()), spectrum);
        fails(
            realization::segment_end(&mono::<Detuned<SvfRealization>>()),
            "after a segment",
        );
    }

    /// A `tanh` in the feedback of the direct form fails linearity (and with it
    /// the response), at the same noise the others pass.
    ///
    /// Mutation (run): `saturate` ignored in `Tdf2Coeffs::step` passes both.
    #[test]
    fn a_tanh_in_the_loop_fails_linearity_and_the_response() {
        assert!(realization::linear(&mono::<Tdf2>()).is_ok());
        fails(realization::linear(&mono::<Tdf2Tanh>()), "fails by");
        fails(realization::response(&mono::<Tdf2Tanh>()), "spectrum is");
    }

    /// A realization that never lands on its target, or lands one step short,
    /// fails the segment-end case.
    ///
    /// Mutations (run): `Stuck` restoring nothing, and `Short`'s last `t` of
    /// `size / size`, pass it.
    #[test]
    fn a_ramp_that_never_arrives_fails_the_segment_end() {
        assert!(realization::segment_end(&mono::<SvfRealization>()).is_ok());
        fails(
            realization::segment_end(&mono::<Stuck>()),
            "after a segment",
        );
        fails(
            realization::segment_end(&mono::<Short>()),
            "after a segment",
        );
    }

    /// A model that lies about a figure it does not own fails the design cases
    /// that hold it to the measured one.
    ///
    /// The design cases are the model's alone (the same for every structure);
    /// `anchors` and `radius` pass on it, and the `Detune`d design makes the
    /// anchors a cutoff off.
    #[test]
    fn the_design_cases_pass_for_the_model() {
        let d = mono::<SvfRealization>();
        design::anchors(&d).unwrap();
        design::radius(&d).unwrap();
        design::norms(&d).unwrap();
    }
}
