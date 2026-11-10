//! The state-variable filter as a model, a design and a realization (design 024
//! step 2): [`SvfModel`] is the system the controls describe, [`SvfDesign`] the
//! pure map from [`SvfControls`] to it, and [`SvfRealization`] one structure that
//! computes it (Simper's trapezoidal form, `svf_step`).
//!
//! The model is a transfer function and nothing else: its response is read from
//! the coefficients' `g` and `k` (the analog prototype `1 / (s^2 + k s + 1)` under
//! the bilinear map), never from a realization. A transposed direct form II and a
//! lattice compute the same `H(z)` (`Biquad`), which is what makes the model the
//! equivalence class and the realization one member of it.

use core::f64::consts::PI;
use core::marker::PhantomData;

use tutti_types::{ChannelLayout, Db, Hz, Q, SampleRate, Samples, Seconds};

use super::{Biquad, Real, SvfCoefficients, SvfCoeffs, SvfType, svf_step};
use crate::filter::biquad::section_l1;
use crate::{
    Cadence, Ceiling, Channels, Complex64, Design, Effect, Effort, Lti, Realization, Ring, Setup,
    Sizes, System, lane_group,
};

/// What an SVF's controls are: its response, cutoff, resonance and the gain of a
/// bell or shelf (read only by those three).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfControls {
    /// The response.
    pub ty: SvfType,
    /// The cutoff, centre or corner.
    pub freq: Hz,
    /// The resonance.
    pub q: Q,
    /// The gain of a bell or a shelf.
    pub gain: Db,
}

impl SvfControls {
    /// The largest `Q` a design asks the coefficients for. Past it the poles are
    /// indistinguishable from the unit circle in `f64` at a low cutoff, and a model
    /// whose radius rounds to 1 could not be called stable.
    pub const MAX_Q: f32 = 1_000.0;

    /// The controls a non-finite value falls back to: a 1 kHz Butterworth
    /// low-pass at 0 dB.
    pub const DEFAULT: SvfControls = SvfControls {
        ty: SvfType::LowPass,
        freq: Hz(1_000.0),
        q: Q::BUTTERWORTH,
        gain: Db(0.0),
    };

    /// These controls with every non-finite figure replaced by the default's and
    /// `q` held in `0.01..=MAX_Q`. A cutoff and a gain out of range are
    /// [`SvfCoeffs::new`]'s to clamp.
    fn sanitized(self) -> Self {
        let finite = |v: f32, d: f32| if v.is_finite() { v } else { d };
        Self {
            ty: self.ty,
            freq: Hz(finite(self.freq.get(), Self::DEFAULT.freq.get())),
            q: Q(finite(self.q.get(), Self::DEFAULT.q.get()).clamp(0.01, Self::MAX_Q)),
            gain: Db(if self.gain.get().is_nan() {
                0.0
            } else {
                self.gain.get()
            }),
        }
    }
}

/// The system an [`SvfControls`] describes at a rate: a two-pole, two-zero
/// section, an [`Lti`] with one input and one output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfModel {
    coeffs: SvfCoeffs,
    controls: SvfControls,
    rate: SampleRate,
}

impl SvfModel {
    /// The model of `controls` at `rate`. Total: a non-finite control takes the
    /// default's and the rest are clamped as the coefficients clamp them.
    #[must_use]
    pub fn new(controls: SvfControls, rate: SampleRate) -> Self {
        let controls = controls.sanitized();
        Self {
            coeffs: SvfCoeffs::new(controls.ty, controls.freq, controls.q, controls.gain, rate),
            controls,
            rate,
        }
    }

    /// The controls it is the model of (sanitised).
    #[must_use]
    pub fn controls(&self) -> SvfControls {
        self.controls
    }

    /// The coefficients of the trapezoidal form, which carry `g`, `k` and the
    /// output weights.
    #[must_use]
    pub fn coeffs(&self) -> &SvfCoeffs {
        &self.coeffs
    }

    /// The rate it is designed at.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The same `H(z)` as direct-form coefficients: what a transposed direct form
    /// II or a lattice realizes. Under `s = (z - 1) / (g (z + 1))` the denominator
    /// `s^2 + k s + 1` times `g^2 (z + 1)^2` is `(1 + g (g + k)) z^2 + (2 g^2 - 2) z
    /// + (1 - g (k - g))`, and the numerator `m0 + m1 s / D + m2 / D` is `m0` times
    /// that plus `m1 g (z^2 - 1)` and `m2 g^2 (z + 1)^2`; both are divided by the
    /// leading term.
    #[must_use]
    pub fn biquad(&self) -> Biquad {
        let SvfCoeffs {
            g, k, m0, m1, m2, ..
        } = self.coeffs;
        let d0 = 1.0 + g * (g + k);
        let d1 = 2.0 * g * g - 2.0;
        let d2 = 1.0 - g * k + g * g;
        Biquad {
            b0: (m0 * d0 + m1 * g + m2 * g * g) / d0,
            b1: (m0 * d1 + 2.0 * m2 * g * g) / d0,
            b2: (m0 * d2 - m1 * g + m2 * g * g) / d0,
            a1: d1 / d0,
            a2: d2 / d0,
        }
    }

    /// The two poles, from the prototype's roots of `s^2 + k s + 1` through
    /// `z = (1 + g s) / (1 - g s)`. The roots are found in the cancellation-free
    /// form (a real pair `q` and `1 / q`).
    #[must_use]
    pub fn poles(&self) -> [Complex64; 2] {
        let (g, k) = (self.coeffs.g, self.coeffs.k);
        let map = |s: Complex64| (1.0 + g * s) / (1.0 - g * s);
        let disc = k * k - 4.0;
        if disc < 0.0 {
            let s = Complex64::new(-k / 2.0, (-disc).sqrt() / 2.0);
            [map(s), map(s.conj())]
        } else {
            let fast = -(k + disc.sqrt()) / 2.0;
            [
                map(Complex64::new(fast, 0.0)),
                map(Complex64::new(1.0 / fast, 0.0)),
            ]
        }
    }

    /// The slowest pole's `-ln |z|`, per frame, to full precision however close
    /// the pole is to the unit circle (`1 - |z|` is formed without cancelling).
    fn decay_rate(&self) -> f64 {
        let (g, k) = (self.coeffs.g, self.coeffs.k);
        let disc = k * k - 4.0;
        if disc < 0.0 {
            // |z|^2 = (1 - g k + g^2) / (1 + g k + g^2) for a conjugate pair.
            -0.5 * libm::log1p(-2.0 * g * k / (1.0 + g * k + g * g))
        } else {
            // The slow real root, |s| = 1 / |q|, |z| = |1 - g |s|| / (1 + g |s|).
            let s = 2.0 / (k + disc.sqrt());
            let gs = g * s;
            if gs < 1.0 {
                -libm::log1p(-2.0 * gs / (1.0 + gs))
            } else {
                -libm::log((gs - 1.0) / (gs + 1.0))
            }
        }
    }

    /// `|H(e^jw)|` and the response itself, from the trapezoidal form (accurate
    /// down to DC: no cancellation between the polynomial's terms).
    fn at(&self, w: f64) -> Complex64 {
        let SvfCoeffs {
            g, k, m0, m1, m2, ..
        } = self.coeffs;
        // The warped frequency in units of the prototype's cutoff: tan(w/2) / g.
        let t = libm::tan(w / 2.0) / g;
        let d = Complex64::new(1.0 - t * t, k * t);
        m0 + Complex64::new(m2, m1 * t) / d
    }

    /// `g_n`, the impulse response of the all-pole part `1 / (1 + a1 z^-1 + a2
    /// z^-2)`, in closed form from the poles.
    fn all_pole(&self, n: i64) -> f64 {
        if n < 0 {
            return 0.0;
        }
        let [p, q] = self.poles();
        let m = (n + 1) as f64;
        if p.im.abs() > 0.0 {
            let (r, th) = (p.norm(), p.arg());
            return libm::pow(r, n as f64) * libm::sin(m * th) / libm::sin(th);
        }
        let (a, b) = (p.re, q.re);
        let pow = |x: f64, e: f64| {
            let sign = if x < 0.0 && (e as i64) % 2 != 0 {
                -1.0
            } else {
                1.0
            };
            sign * libm::pow(x.abs(), e)
        };
        if (a - b).abs() >= 1e-9 * a.abs().max(b.abs()) {
            (pow(a, m) - pow(b, m)) / (a - b)
        } else {
            m * pow(0.5 * (a + b), n as f64)
        }
    }

    /// The envelope constant `K` of `|g_n| <= min(n + 1, K) r^n`.
    fn envelope_k(&self) -> f64 {
        let [p, q] = self.poles();
        let gap = (p - q).norm();
        if gap > 0.0 {
            2.0 * p.norm().max(q.norm()) / gap
        } else {
            f64::INFINITY
        }
    }
}

impl System for SvfModel {
    /// Zero: no lookahead, and the filter's response starts at its first frame.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The frames until `|h[n]|` is at most `10^(floor / 20)` of its largest, from
    /// the envelope `(|b0| + |b1| + |b2|) min(n - 1, K) r^(n - 2)` of the numerator over the
    /// all-pole part (`r` the slowest pole, `K = 2r / |p1 - p2|`) against the
    /// largest `|h|` found on a geometric grid and refined at its peak, so it covers
    /// the true ring and sits within a small factor of it.
    fn tail(&self, floor: Db) -> Ring {
        let nln = self.decay_rate();
        if !(nln > 0.0) {
            return Ring::Unbounded;
        }
        let b = self.biquad();
        let h = |n: i64| {
            b.b0 * self.all_pole(n) + b.b1 * self.all_pole(n - 1) + b.b2 * self.all_pole(n - 2)
        };
        // The largest |h| we can find: a lower bound of the true one, so the
        // figure below is not short.
        let mut best = (0i64, h(0).abs());
        let mut n = 1.0f64;
        while n < 1e9 {
            let at = n as i64;
            let v = h(at).abs();
            if v > best.1 {
                best = (at, v);
            }
            n = (n * 1.189_207_115).max(n + 1.0);
        }
        let step = (best.0 / 64).max(1);
        let mut at = (best.0 - best.0 / 6).max(0);
        while at <= best.0 + best.0 / 5 + 4 {
            best.1 = best.1.max(h(at).abs());
            at += step;
        }
        let eps = libm::pow(10.0, f64::from(floor.get()) / 20.0);
        let target = eps * best.1;
        if !(target > 0.0) {
            return Ring::Finite(Seconds::new(0.0));
        }
        let sum_b = b.b0.abs() + b.b1.abs() + b.b2.abs();
        let (r, k) = (libm::exp(-nln), self.envelope_k());
        let bound = |n: i64| sum_b * k.min((n - 1) as f64) * libm::pow(r, (n - 2) as f64);
        // The envelope falls from its peak, at about one time constant on.
        let mut lo = ((1.0 / nln) as i64 + 3).max(3);
        if bound(lo) <= target {
            return Ring::Finite(Seconds::new(lo as f64 / self.rate.get()));
        }
        let mut hi = lo;
        while bound(hi) > target {
            lo = hi;
            hi = hi.saturating_mul(2);
            if hi > (1 << 52) {
                return Ring::Unbounded;
            }
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if bound(mid) > target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ring::Finite(Seconds::new(hi as f64 / self.rate.get()))
    }
}

impl Lti for SvfModel {
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        for (g, f) in gain.iter_mut().zip(freqs) {
            *g = if output == 0 {
                self.at(2.0 * PI * f64::from(f.get()) / self.rate.get())
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The slowest pole's modulus, exactly (it is the figure, and so also a
    /// bound).
    fn spectral_radius(&self) -> f64 {
        libm::exp(-self.decay_rate())
    }

    fn l1_norm(&self) -> f64 {
        section_l1(&self.biquad())
    }

    /// The peak of `|H|`, found on a grid of 1024 frequencies and a log-spaced
    /// run around the resonance (the pole's angle) and refined by golden section
    /// at the largest, then raised by a part in a million: a sufficient bound to
    /// that figure, which the contract holds against its own grid.
    fn hinf_norm(&self) -> f64 {
        let nyquist = PI;
        let mut pts: Vec<f64> = (0..=1024)
            .map(|i| nyquist * f64::from(i) / 1024.0)
            .collect();
        let [p, _] = self.poles();
        let width = self.decay_rate();
        if p.im != 0.0 {
            let w0 = p.arg().abs();
            for i in 0..80 {
                let d = width * libm::pow(10.0, (f64::from(i) - 40.0) / 8.0);
                pts.extend([w0 + d, w0 - d]);
            }
        }
        pts.retain(|w| (0.0..=nyquist).contains(w));
        pts.sort_by(f64::total_cmp);
        pts.dedup();
        let mag = |w: f64| self.at(w).norm();
        let best = pts
            .iter()
            .enumerate()
            .max_by(|a, b| mag(*a.1).total_cmp(&mag(*b.1)))
            .map_or(0, |(i, _)| i);
        let (mut a, mut b) = (
            pts[best.saturating_sub(1)],
            pts[(best + 1).min(pts.len() - 1)],
        );
        let phi = 0.618_033_988_749_894_9;
        for _ in 0..80 {
            let (x, y) = (b - phi * (b - a), a + phi * (b - a));
            if mag(x) < mag(y) {
                a = x;
            } else {
                b = y;
            }
        }
        mag(pts[best]).max(mag(0.5 * (a + b))) * (1.0 + 1e-6)
    }
}

/// The worst case an [`SvfDesign`] has: how long it may ring.
///
/// An SVF holds no storage a control sizes, so this is only the ring figure the
/// node declares as its ceiling. Its worst case is unbounded: a cutoff of 1 Hz at
/// the largest `Q`, or a bell with 200 dB of gain, has poles at the unit circle to
/// within what `f64` resolves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfBounds {
    tail: Ring,
}

impl Sizes for SvfBounds {
    fn tail(&self) -> Ring {
        self.tail
    }

    fn dominates(&self, other: &Self) -> bool {
        self.tail.covers(other.tail)
    }
}

/// The pure map from [`SvfControls`] to an [`SvfModel`], over the realization `R`
/// (the trapezoidal [`SvfRealization`] unless another is named: the model is the
/// same).
pub struct SvfDesign<R = SvfRealization> {
    channels: ChannelLayout,
    realization: PhantomData<fn() -> R>,
}

impl<R> SvfDesign<R> {
    /// An SVF over `channels`.
    #[must_use]
    pub fn new(channels: ChannelLayout) -> Self {
        Self {
            channels,
            realization: PhantomData,
        }
    }
}

impl<R> Clone for SvfDesign<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for SvfDesign<R> {}

impl<R> core::fmt::Debug for SvfDesign<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SvfDesign")
            .field("channels", &self.channels)
            .finish()
    }
}

impl<R> Design for SvfDesign<R>
where
    R: Realization<SvfModel, Bounds = SvfBounds> + From<ChannelLayout>,
{
    type Controls = SvfControls;
    type Model = SvfModel;
    type Bounds = SvfBounds;
    type Range = ();
    type Realization = R;

    /// A design every 16 frames inside a block: a lane may move the cutoff or
    /// the Q there. Design 024 Q21.
    const CADENCE: Cadence =
        Cadence::Every(match core::num::NonZeroUsize::new(crate::COEFF_INTERVAL) {
            Some(n) => n,
            None => unreachable!(),
        });

    fn channels(&self) -> Channels {
        Channels {
            input: self.channels,
            output: self.channels,
        }
    }

    fn blank(&self, rate: SampleRate) -> SvfModel {
        SvfModel::new(SvfControls::DEFAULT, rate)
    }

    fn design_into(&self, controls: &SvfControls, rate: SampleRate, out: &mut SvfModel) {
        *out = SvfModel::new(*controls, rate);
    }

    fn bounds(&self, _ceiling: &Ceiling<()>) -> SvfBounds {
        SvfBounds {
            tail: Ring::Unbounded,
        }
    }

    fn measure(&self, model: &SvfModel, floor: Db) -> SvfBounds {
        SvfBounds {
            tail: model.tail(floor),
        }
    }

    fn realization(&self) -> R {
        R::from(self.channels)
    }
}

/// Simper's trapezoidal SVF (`svf_step`) as a realization of [`SvfModel`]: one
/// coefficient set shared across the channels and one integrator pair each.
///
/// Its cadence is [`Cadence::Every`]: [`retarget`](Realization::retarget) sets the
/// target and the next [`process`](Effect::process) call ramps the coefficients
/// linearly from the previous target to it across the call, landing exactly on it
/// at the call's last frame (`SvfCoefficients::lerp`, as the filter node always
/// has). The call is the node's segment, so the ramp's clock is the node's, not the
/// stream's: the same blocks give the same render, blocks cut elsewhere do not.
/// The first retarget after `prepare` lands at once, and `reset` and `fresh` land
/// on the target.
#[derive(Clone, Debug)]
pub struct SvfRealization<F: Real = f64> {
    /// The coefficients the last rendered sample ran at.
    pub(super) prev: SvfCoefficients<F>,
    /// The target.
    pub(super) next: SvfCoefficients<F>,
    /// Whether `prev` is a retarget's (false after `prepare`: the next lands at once).
    primed: bool,
    /// Integrator state, one pair per channel; its length is the width.
    pub(super) state: Vec<[F; 2]>,
}

impl<F: Real> From<ChannelLayout> for SvfRealization<F> {
    fn from(channels: ChannelLayout) -> Self {
        let model = SvfModel::new(SvfControls::DEFAULT, SampleRate::DEFAULT);
        let c = Self::coefficients(&model);
        Self {
            prev: c,
            next: c,
            primed: false,
            state: vec![[F::from_f64(0.0); 2]; usize::from(channels.count()).max(1)],
        }
    }
}

impl<F: Real> SvfRealization<F> {
    fn coefficients(model: &SvfModel) -> SvfCoefficients<F> {
        let c = model.controls();
        SvfCoefficients::from_coeffs(model.coeffs(), c.freq, c.q, c.gain)
    }

    /// Renders with one set held for the whole call, the channels in groups of
    /// up to `LANES` sample-inner (a recursive filter is latency-bound on one
    /// channel, so a group fills the latency with the others' chains).
    fn run_held(&mut self, size: usize, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let width = self.state.len();
        let mut first = 0;
        while first < width {
            let n = lane_group(width - first);
            match n {
                8 => self.held_group::<8>(first, size, input, output),
                7 => self.held_group::<7>(first, size, input, output),
                6 => self.held_group::<6>(first, size, input, output),
                5 => self.held_group::<5>(first, size, input, output),
                4 => self.held_group::<4>(first, size, input, output),
                3 => self.held_group::<3>(first, size, input, output),
                2 => self.held_group::<2>(first, size, input, output),
                _ => self.held_group::<1>(first, size, input, output),
            }
            first += n;
        }
    }

    #[inline]
    fn held_group<const N: usize>(
        &mut self,
        first: usize,
        size: usize,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
    ) {
        let c = self.next;
        let xs: [&[f32]; N] = core::array::from_fn(|k| &input[first + k][..size]);
        let mut group = output.iter_mut().skip(first);
        let ys: [&mut [f32]; N] = core::array::from_fn(|_| {
            &mut group.next().expect("the group is inside the width")[..size]
        });
        let mut st: [[F; 2]; N] = core::array::from_fn(|k| self.state[first + k]);
        for i in 0..size {
            for k in 0..N {
                ys[k][i] = svf_step(&c, &mut st[k], F::from_f32(xs[k][i])).to_f32();
            }
        }
        self.state[first..first + N].copy_from_slice(&st);
    }

    /// Renders with the coefficients moving from `prev` to `next` across the call.
    fn run_swept(&mut self, size: usize, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let n = F::from_f64(size as f64);
        for i in 0..size {
            let k = if i + 1 == size {
                self.next
            } else {
                self.prev.lerp(&self.next, F::from_f64((i + 1) as f64) / n)
            };
            for (ch, st) in self.state.iter_mut().enumerate() {
                let x = F::from_f32(input[ch][i]);
                output[ch][i] = svf_step(&k, st, x).to_f32();
            }
        }
    }
}

impl<F: Real> Realization<SvfModel> for SvfRealization<F> {
    type Bounds = SvfBounds;

    fn prepare(&mut self, _bounds: &SvfBounds, setup: &Setup) {
        let width = usize::from(setup.output.count()).max(1);
        self.state = vec![[F::from_f64(0.0); 2]; width];
        self.primed = false;
    }

    fn retarget(&mut self, model: &SvfModel) {
        self.next = Self::coefficients(model);
        if !self.primed {
            self.prev = self.next;
            self.primed = true;
        }
    }

    fn reset(&mut self) {
        self.state.fill([F::from_f64(0.0); 2]);
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

impl<F: Real> Effect<SvfModel> for SvfRealization<F> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let size = output.first().map_or(0, |c| c.len());
        if size == 0 {
            return;
        }
        if self.prev == self.next {
            self.run_held(size, input, output);
        } else {
            self.run_swept(size, input, output);
            self.prev = self.next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(ty: SvfType, freq: f32, q: f32, gain: f32, rate: f64) -> SvfModel {
        SvfModel::new(
            SvfControls {
                ty,
                freq: Hz(freq),
                q: Q(q),
                gain: Db(gain),
            },
            SampleRate(rate),
        )
    }

    /// The RBJ cookbook biquad of a response (`Q` form), as coefficients: an
    /// independent derivation of the same `H(z)` (the shelves use `2 sqrt(A) alpha`).
    fn rbj(ty: SvfType, fc: f64, q: f64, gain_db: f64, rate: f64) -> Biquad {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * fc / rate;
        let (sn, cs) = w0.sin_cos();
        let al = sn / (2.0 * q);
        let sa = 2.0 * a.sqrt() * al;
        let (b, d) = match ty {
            SvfType::LowPass => (
                [(1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0],
                [1.0 + al, -2.0 * cs, 1.0 - al],
            ),
            SvfType::HighPass => (
                [(1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0],
                [1.0 + al, -2.0 * cs, 1.0 - al],
            ),
            SvfType::BandPass => ([sn / 2.0, 0.0, -sn / 2.0], [1.0 + al, -2.0 * cs, 1.0 - al]),
            SvfType::Notch => ([1.0, -2.0 * cs, 1.0], [1.0 + al, -2.0 * cs, 1.0 - al]),
            SvfType::Allpass => (
                [1.0 - al, -2.0 * cs, 1.0 + al],
                [1.0 + al, -2.0 * cs, 1.0 - al],
            ),
            SvfType::Bell => (
                [1.0 + al * a, -2.0 * cs, 1.0 - al * a],
                [1.0 + al / a, -2.0 * cs, 1.0 - al / a],
            ),
            SvfType::LowShelf => (
                [
                    a * ((a + 1.0) - (a - 1.0) * cs + sa),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                    a * ((a + 1.0) - (a - 1.0) * cs - sa),
                ],
                [
                    (a + 1.0) + (a - 1.0) * cs + sa,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                    (a + 1.0) + (a - 1.0) * cs - sa,
                ],
            ),
            SvfType::HighShelf => (
                [
                    a * ((a + 1.0) + (a - 1.0) * cs + sa),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                    a * ((a + 1.0) + (a - 1.0) * cs - sa),
                ],
                [
                    (a + 1.0) - (a - 1.0) * cs + sa,
                    2.0 * ((a - 1.0) - (a + 1.0) * cs),
                    (a + 1.0) - (a - 1.0) * cs - sa,
                ],
            ),
            _ => unreachable!("a new response needs its RBJ row"),
        };
        Biquad {
            b0: b[0] / d[0],
            b1: b[1] / d[0],
            b2: b[2] / d[0],
            a1: d[1] / d[0],
            a2: d[2] / d[0],
        }
    }

    const TYPES: [SvfType; 8] = [
        SvfType::LowPass,
        SvfType::HighPass,
        SvfType::BandPass,
        SvfType::Notch,
        SvfType::Allpass,
        SvfType::Bell,
        SvfType::LowShelf,
        SvfType::HighShelf,
    ];

    /// The model's complex response is the RBJ cookbook's, the closed form of each
    /// response, at DC, the cutoff, Nyquist and a log sweep between: magnitude and
    /// phase, to 1e-9 of the larger. The trapezoidal form that `SvfModel` reads
    /// and the direct form of the cookbook are two derivations of one `H(z)`.
    ///
    /// Mutations (run): `k = 1 / q` for a bell in `SvfCoeffs::new`, a shelf's `g`
    /// left at `g0`, the `m1` of the band-pass doubled and `tan(w / 2)` taken as
    /// `tan(w / 2.1)` in `at` each fail it.
    #[test]
    fn the_response_is_the_rbj_closed_form() {
        let rate = 48_000.0;
        for ty in TYPES {
            for (fc, q, gain) in [
                (1_000.0f32, 0.707f32, 9.0f32),
                (250.0, 4.0, -12.0),
                (7_000.0, 1.3, 6.0),
            ] {
                // The controls are `f32`: the closed form is read at the same figures.
                let (fc, q, gain) = (f64::from(fc), f64::from(q), f64::from(gain));
                let want = rbj(ty, fc, q, gain, rate);
                let m = model(ty, fc as f32, q as f32, gain as f32, rate);
                let mut freqs = vec![Hz(0.0), Hz(fc as f32), Hz(24_000.0)];
                freqs
                    .extend((0..40).map(|i| {
                        Hz(libm::exp(libm::log(10.0) + 7.0 * f64::from(i) / 39.0) as f32)
                    }));
                let mut got = vec![Complex64::new(0.0, 0.0); freqs.len()];
                m.frequency_response(0, &freqs, &mut got);
                for (f, g) in freqs.iter().zip(&got) {
                    let w = 2.0 * PI * f64::from(f.get()) / rate;
                    let h = want.response(w);
                    let scale = h.norm().max(g.norm()).max(1e-12);
                    assert!(
                        (g - h).norm() <= 1e-9 * scale + 1e-12,
                        "{ty:?} fc {fc} Q {q} {gain} dB at {f:?}: {g} against {h}"
                    );
                }
            }
        }
    }

    /// `biquad()` is the same `H(z)` as `frequency_response`: the direct form that
    /// the two other realizations compute.
    ///
    /// Mutation (run): the sign of `m1 g` in `b2` flipped fails it.
    #[test]
    fn the_direct_form_is_the_same_response() {
        for ty in TYPES {
            let m = model(ty, 1_500.0, 2.5, 7.0, 44_100.0);
            let b = m.biquad();
            for f in [30.0f32, 500.0, 1_500.0, 9_000.0, 20_000.0] {
                let mut g = [Complex64::new(0.0, 0.0)];
                m.frequency_response(0, &[Hz(f)], &mut g);
                let h = b.response(2.0 * PI * f64::from(f) / 44_100.0);
                assert!(
                    (g[0] - h).norm() <= 1e-9,
                    "{ty:?} at {f}: {} against {h}",
                    g[0]
                );
            }
        }
    }

    /// The spectral radius is the slowest pole of the direct form's own
    /// denominator, found independently from `a1` and `a2` (a conjugate pair has
    /// `|p|^2 = a2`; a real pair is the quadratic's roots), and the model's poles are
    /// its roots.
    ///
    /// Mutation (run): the slow real root `2 / (k + sqrt(disc))` taken as the fast
    /// one fails the Q 0.1 rows; the conjugate branch's `1 + g k + g^2` as
    /// `1 - g k + g^2` fails the rest.
    #[test]
    fn the_radius_is_the_slowest_root_of_the_denominator() {
        for (q, fc) in [
            (0.1, 500.0),
            (0.4, 2_000.0),
            (0.707, 1_000.0),
            (5.0, 80.0),
            (300.0, 40.0),
        ] {
            let m = model(SvfType::LowPass, fc, q, 0.0, 48_000.0);
            let b = m.biquad();
            let disc = b.a1 * b.a1 - 4.0 * b.a2;
            let want = if disc < 0.0 {
                b.a2.sqrt()
            } else {
                let s = disc.sqrt();
                ((-b.a1 + s) / 2.0).abs().max(((-b.a1 - s) / 2.0).abs())
            };
            assert!(
                (m.spectral_radius() - want).abs() <= 1e-12 + 1e-9 * (1.0 - want),
                "Q {q}: {} against {want}",
                m.spectral_radius()
            );
            for p in m.poles() {
                assert!((p * p + b.a1 * p + b.a2).norm() < 1e-9, "Q {q}: {p}");
            }
        }
    }

    /// The first frame of a ramped call runs one step in, at `1 / n` of the way
    /// from the previous coefficients to the target, not on the previous ones: an
    /// impulse at frame 0 through zero state gives `m0 + m1 a2 + m2 a3` of the
    /// blended set, computed here from the two models' coefficients.
    ///
    /// Mutation (run): the blend at `i / n` instead of `(i + 1) / n` in `run_swept`
    /// gives the previous set's value and fails.
    #[test]
    fn the_first_frame_of_a_ramp_is_one_step_in() {
        let (a, b) = (
            model(SvfType::LowPass, 500.0, 1.0, 0.0, 48_000.0),
            model(SvfType::LowPass, 4_000.0, 3.0, 0.0, 48_000.0),
        );
        let n = 16usize;
        let mut k = SvfRealization::<f64>::from(ChannelLayout::MONO);
        k.retarget(&a);
        k.retarget(&b);
        let mut out = vec![0.0f32; n];
        let mut x = vec![0.0f32; n];
        x[0] = 1.0;
        k.process(&[&x], &mut [&mut out]);
        let h0 = |m: &SvfModel, t: f64| {
            let c = m.coeffs();
            (c.m0 + c.m1 * c.a2 + c.m2 * c.a3, t)
        };
        let (pa, pb) = (a.coeffs(), b.coeffs());
        let blend = |from: f64, to: f64| from + (to - from) / n as f64;
        let _ = h0;
        let (a2, a3) = (blend(pa.a2, pb.a2), blend(pa.a3, pb.a3));
        let (m0, m1, m2) = (
            blend(pa.m0, pb.m0),
            blend(pa.m1, pb.m1),
            blend(pa.m2, pb.m2),
        );
        let want = (m0 + m1 * a2 + m2 * a3) as f32;
        assert_eq!(out[0], want);
    }

    /// A retarget the call has not yet run is landed by a `reset`: an impulse
    /// after it sees the target from its first frame (`b0` of the target's direct
    /// form), not a ramp from the previous one.
    ///
    /// Mutation (run): `reset` leaving `prev` alone fails.
    #[test]
    fn a_reset_between_a_retarget_and_its_call_lands() {
        let (a, b) = (
            model(SvfType::HighPass, 500.0, 1.0, 0.0, 48_000.0),
            model(SvfType::HighPass, 4_000.0, 3.0, 0.0, 48_000.0),
        );
        let mut k = SvfRealization::<f64>::from(ChannelLayout::MONO);
        k.retarget(&a);
        k.retarget(&b);
        k.reset();
        let mut out = [0.0f32; 8];
        let mut x = [0.0f32; 8];
        x[0] = 1.0;
        k.process(&[&x], &mut [&mut out[..]]);
        assert_eq!(out[0], b.biquad().b0 as f32);
    }

    /// Non-finite controls take the family's default and a huge `Q` is held at
    /// `MAX_Q`, so every model is one that can be run: finite coefficients, a
    /// radius under 1.
    ///
    /// Mutations (run): dropping the finite guard on the cutoff fails the NaN row;
    /// dropping the `Q` clamp fails the `1e30` row (the radius rounds to 1).
    #[test]
    fn a_hostile_control_still_gives_a_stable_model() {
        let rate = SampleRate(48_000.0);
        for (f, q, g) in [
            (f32::NAN, 1.0, 0.0),
            (1_000.0, f32::NAN, 0.0),
            (1_000.0, f32::INFINITY, 6.0),
            (1.0, 1e30, 1e30),
            (-5.0, -1.0, f32::NEG_INFINITY),
        ] {
            let m = SvfModel::new(
                SvfControls {
                    ty: SvfType::Bell,
                    freq: Hz(f),
                    q: Q(q),
                    gain: Db(g),
                },
                rate,
            );
            let c = m.coeffs();
            assert!(
                [c.g, c.k, c.a1, c.a2, c.a3, c.m0, c.m1, c.m2]
                    .iter()
                    .all(|v| v.is_finite())
            );
            assert!(m.spectral_radius() < 1.0, "{f} {q} {g}");
        }
    }
}
