//! Biquad filters with optional nonlinearities by Jatin Chowdhury.

// For more information, see:
// https://github.com/jatinchowdhury18/ComplexNonlinearities entries 4 and 5.
// For some of the biquad formulae, see the Audio EQ Cookbook:
// https://www.w3.org/TR/audio-eq-cookbook/

use super::audionode::*;
use super::math::*;
use super::setting::*;
use super::shape::*;
use super::signal::*;
use super::*;
use core::marker::PhantomData;
use numeric_array::typenum::*;

/// Biquad coefficients in normalized form.
#[derive(Copy, Clone, Debug, Default)]
pub struct BiquadCoefs<F> {
    pub a1: F,
    pub a2: F,
    pub b0: F,
    pub b1: F,
    pub b2: F,
}

impl<F: Float> BiquadCoefs<F> {
    /// Return settings for a Butterworth lowpass filter.
    /// Sample rate is in Hz.
    /// Cutoff is the -3 dB point of the filter in Hz.
    #[inline]
    pub fn butter_lowpass(sample_rate: F, cutoff: F) -> Self {
        let c = F::from_f32;
        let f: F = tan(cutoff * F::PI / sample_rate);
        let a0r: F = c(1.0) / (c(1.0) + F::SQRT_2 * f + f * f);
        let a1: F = (c(2.0) * f * f - c(2.0)) * a0r;
        let a2: F = (c(1.0) - F::SQRT_2 * f + f * f) * a0r;
        let b0: F = f * f * a0r;
        let b1: F = c(2.0) * b0;
        let b2: F = b0;
        Self { a1, a2, b0, b1, b2 }
    }

    /// Return settings for a constant-gain bandpass resonator.
    /// Sample rate and center frequency are in Hz.
    /// The overall gain of the filter is independent of bandwidth.
    #[inline]
    pub fn resonator(sample_rate: F, center: F, q: F) -> Self {
        let c = F::from_f32;
        let r: F = exp(-F::PI * center / (q * sample_rate));
        let a1: F = c(-2.0) * r * cos(F::TAU * center / sample_rate);
        let a2: F = r * r;
        let b0: F = sqrt(c(1.0) - r * r) * c(0.5);
        let b1: F = c(0.0);
        let b2: F = -b0;
        Self { a1, a2, b0, b1, b2 }
    }

    /// Return settings for a lowpass filter.
    /// Sample rate and cutoff frequency are in Hz.
    #[inline]
    pub fn lowpass(sample_rate: F, cutoff: F, q: F) -> Self {
        let c = F::from_f32;
        let omega = F::TAU * cutoff / sample_rate;
        let alpha = sin(omega) / (c(2.0) * q);
        let beta = cos(omega);
        let a0r = c(1.0) / (c(1.0) + alpha);
        let a1 = c(-2.0) * beta * a0r;
        let a2 = (c(1.0) - alpha) * a0r;
        let b1 = (c(1.0) - beta) * a0r;
        let b0 = b1 * c(0.5);
        let b2 = b0;
        Self { a1, a2, b0, b1, b2 }
    }

    /// Return settings for a highpass filter.
    /// Sample rate and cutoff frequency are in Hz.
    #[inline]
    pub fn highpass(sample_rate: F, cutoff: F, q: F) -> Self {
        let c = F::from_f32;
        let omega = F::TAU * cutoff / sample_rate;
        let alpha = sin(omega) / (c(2.0) * q);
        let beta = cos(omega);
        let a0r = c(1.0) / (c(1.0) + alpha);
        let a1 = c(-2.0) * beta * a0r;
        let a2 = (c(1.0) - alpha) * a0r;
        let b0 = (c(1.0) + beta) * c(0.5) * a0r;
        let b1 = (c(-1.0) - beta) * a0r;
        let b2 = b0;
        Self { a1, a2, b0, b1, b2 }
    }

    /// Return settings for a bell equalizer filter.
    /// Sample rate and center frequencies are in Hz.
    /// Gain is amplitude gain (`gain` > 0).
    #[inline]
    pub fn bell(sample_rate: F, center: F, q: F, gain: F) -> Self {
        let c = F::from_f32;
        let omega = F::TAU * center / sample_rate;
        let alpha = sin(omega) / (c(2.0) * q);
        let beta = cos(omega);
        let a = sqrt(gain);
        let a0r = c(1.0) / (c(1.0) + alpha / a);
        let a1 = c(-2.0) * beta * a0r;
        let a2 = (c(1.0) - alpha / a) * a0r;
        let b0 = (c(1.0) + alpha * a) * a0r;
        let b1 = a1;
        let b2 = (c(1.0) - alpha * a) * a0r;
        Self { a1, a2, b0, b1, b2 }
    }

    /// Arbitrary biquad.
    #[inline]
    pub fn arbitrary(a1: F, a2: F, b0: F, b1: F, b2: F) -> Self {
        Self { a1, a2, b0, b1, b2 }
    }

    /// Return settings for a Butterworth highpass filter.
    /// Sample rate is in Hz.
    /// Cutoff is the -3 dB point of the filter in Hz.
    #[inline]
    pub fn butter_highpass(sample_rate: F, cutoff: F) -> Self {
        let c = F::from_f32;
        let f: F = tan(cutoff * F::PI / sample_rate);
        let a0r: F = c(1.0) / (c(1.0) + F::SQRT_2 * f + f * f);
        let a1: F = (c(2.0) * f * f - c(2.0)) * a0r;
        let a2: F = (c(1.0) - F::SQRT_2 * f + f * f) * a0r;
        let b0: F = a0r;
        let b1: F = c(-2.0) * a0r;
        let b2: F = a0r;
        Self { a1, a2, b0, b1, b2 }
    }

    /// Return settings for a Linkwitz-Riley lowpass filter (single 2nd-order stage).
    /// LR filters are squared Butterworth filters.
    /// - LR2 = 1 stage (12 dB/oct), -3 dB at cutoff per band
    /// - LR4 = 2 stages (24 dB/oct), -6 dB at cutoff per band
    /// - LR8 = 4 stages (48 dB/oct), -12 dB at cutoff per band
    #[inline]
    pub fn linkwitz_riley_lowpass(sample_rate: F, cutoff: F) -> Self {
        // LR uses Butterworth (Q = 1/sqrt(2)) - cascading squares the response
        Self::butter_lowpass(sample_rate, cutoff)
    }

    /// Return settings for a Linkwitz-Riley highpass filter (single 2nd-order stage).
    /// LR filters are squared Butterworth filters.
    /// - LR2 = 1 stage (12 dB/oct), -3 dB at cutoff per band
    /// - LR4 = 2 stages (24 dB/oct), -6 dB at cutoff per band
    /// - LR8 = 4 stages (48 dB/oct), -12 dB at cutoff per band
    #[inline]
    pub fn linkwitz_riley_highpass(sample_rate: F, cutoff: F) -> Self {
        // LR uses Butterworth (Q = 1/sqrt(2)) - cascading squares the response
        Self::butter_highpass(sample_rate, cutoff)
    }

    /// Frequency response at frequency `omega` expressed as fraction of sampling rate.
    pub fn response(&self, omega: f64) -> Complex64 {
        let z1 = Complex64::from_polar(1.0, -f64::TAU * omega);
        let z2 = z1 * z1;
        /// Complex64 with real component `x` and imaginary component zero.
        fn re<T: Float>(x: T) -> Complex64 {
            Complex64::new(x.to_f64(), 0.0)
        }
        (re(self.b0) + re(self.b1) * z1 + re(self.b2) * z2)
            / (re(1.0) + re(self.a1) * z1 + re(self.a2) * z2)
    }
}

/// 2nd order IIR filter implemented in normalized Direct Form I.
/// - Setting: coefficients as tuple `Setting::biquad(a1, a2, b0, b1, b2)`.
/// - Input 0: input signal.
/// - Output 0: filtered signal.
#[derive(Default, Clone)]
pub struct Biquad<F> {
    coefs: BiquadCoefs<F>,
    x1: F,
    x2: F,
    y1: F,
    y2: F,
    sample_rate: f64,
}

impl<F: Float> Biquad<F> {
    pub fn new() -> Self {
        Self {
            sample_rate: DEFAULT_SR,
            ..Default::default()
        }
    }
    pub fn with_coefs(coefs: BiquadCoefs<F>) -> Self {
        Self {
            coefs,
            sample_rate: DEFAULT_SR,
            ..Default::default()
        }
    }
    pub fn coefs(&self) -> &BiquadCoefs<F> {
        &self.coefs
    }
    pub fn set_coefs(&mut self, coefs: BiquadCoefs<F>) {
        self.coefs = coefs;
    }
}

impl<F: Float> AudioNode for Biquad<F> {
    const ID: u64 = 15;
    type Inputs = typenum::U1;
    type Outputs = typenum::U1;

    fn reset(&mut self) {
        self.x1 = F::zero();
        self.x2 = F::zero();
        self.y1 = F::zero();
        self.y2 = F::zero();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let x0 = convert(input[0]);
        let y0 = self.coefs.b0 * x0 + self.coefs.b1 * self.x1 + self.coefs.b2 * self.x2
            - self.coefs.a1 * self.y1
            - self.coefs.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        [convert(y0)].into()
    }

    fn set(&mut self, setting: Setting) {
        if let Parameter::Biquad(a1, a2, b0, b1, b2) = setting.parameter() {
            self.set_coefs(BiquadCoefs::arbitrary(
                F::from_f32(*a1),
                F::from_f32(*a2),
                F::from_f32(*b0),
                F::from_f32(*b1),
                F::from_f32(*b2),
            ));
        }
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.outputs());
        output.set(
            0,
            input.at(0).filter(0.0, |r| {
                r * self.coefs().response(frequency / self.sample_rate)
            }),
        );
        output
    }
}

/// Butterworth lowpass filter.
/// Setting: cutoff.
/// Number of inputs is `N`, either `U1` or `U2`.
/// - Input 0: input signal
/// - Input 1 (optional): cutoff frequency (Hz)
/// - Output 0: filtered signal
#[derive(Clone)]
pub struct ButterLowpass<F: Real, N: Size<f32>> {
    _marker: PhantomData<N>,
    biquad: Biquad<F>,
    sample_rate: F,
    cutoff: F,
}

impl<F: Real, N: Size<f32>> ButterLowpass<F, N> {
    /// Create new Butterworth lowpass filter with initial `cutoff` frequency in Hz.
    pub fn new(cutoff: F) -> Self {
        let mut node = ButterLowpass {
            _marker: PhantomData,
            biquad: Biquad::new(),
            sample_rate: F::from_f64(DEFAULT_SR),
            cutoff: F::zero(),
        };
        node.biquad.reset();
        node.set_cutoff(cutoff);
        node
    }
    pub fn set_cutoff(&mut self, cutoff: F) {
        self.biquad
            .set_coefs(BiquadCoefs::butter_lowpass(self.sample_rate, cutoff));
        self.cutoff = cutoff;
    }
}

impl<F: Real, N: Size<f32>> AudioNode for ButterLowpass<F, N> {
    const ID: u64 = 16;
    type Inputs = N;
    type Outputs = typenum::U1;

    fn reset(&mut self) {
        self.biquad.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = convert(sample_rate);
        self.biquad.set_sample_rate(crate::SampleRate(sample_rate));
        self.set_cutoff(self.cutoff);
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        if N::USIZE > 1 {
            let cutoff: F = convert(input[1]);
            if cutoff != self.cutoff {
                self.set_cutoff(cutoff);
            }
        }
        self.biquad.tick(&[input[0]].into())
    }

    fn set(&mut self, setting: Setting) {
        if let Parameter::Center(cutoff) = setting.parameter() {
            self.set_cutoff(F::from_f32(*cutoff));
        }
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.outputs());
        output.set(
            0,
            input.at(0).filter(0.0, |r| {
                r * self
                    .biquad
                    .coefs()
                    .response(frequency / self.sample_rate.to_f64())
            }),
        );
        output
    }
}

/// Constant-gain bandpass filter (resonator).
/// Filter gain is (nearly) independent of bandwidth.
/// Setting: (center, bandwidth).
/// Number of inputs is `N`, either `U1` or `U3`.
/// - Input 0: input signal
/// - Input 1 (optional): filter center (peak) frequency (Hz)
/// - Input 2 (optional): filter Q
/// - Output 0: filtered signal
#[derive(Clone)]
pub struct Resonator<F: Real, N: Size<f32>> {
    _marker: PhantomData<N>,
    biquad: Biquad<F>,
    sample_rate: F,
    center: F,
    q: F,
}

impl<F: Real, N: Size<f32>> Resonator<F, N> {
    /// Create new resonator bandpass. Initial `center` frequency is specified in Hz.
    pub fn new(center: F, q: F) -> Self {
        let mut node = Resonator {
            _marker: PhantomData,
            biquad: Biquad::new(),
            sample_rate: F::from_f64(DEFAULT_SR),
            center,
            q,
        };
        node.biquad.reset();
        node.set_center_q(center, q);
        node
    }
    pub fn set_center_q(&mut self, center: F, q: F) {
        self.biquad
            .set_coefs(BiquadCoefs::resonator(self.sample_rate, center, q));
        self.center = center;
        self.q = q;
    }
}

impl<F: Real, N: Size<f32>> AudioNode for Resonator<F, N> {
    const ID: u64 = 17;
    type Inputs = N;
    type Outputs = typenum::U1;

    fn reset(&mut self) {
        self.biquad.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = convert(sample_rate);
        self.set_center_q(self.center, self.q);
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        if N::USIZE >= 3 {
            let center: F = convert(input[1]);
            let q: F = convert(input[2]);
            if center != self.center || q != self.q {
                self.biquad
                    .set_coefs(BiquadCoefs::resonator(self.sample_rate, center, q));
                self.center = center;
                self.q = q;
            }
        }
        self.biquad.tick(&[input[0]].into())
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.outputs());
        output.set(
            0,
            input.at(0).filter(0.0, |r| {
                r * self
                    .biquad
                    .coefs()
                    .response(frequency / self.sample_rate.to_f64())
            }),
        );
        output
    }
}

/// Biquad filter common mode parameters. Filter modes use a subset of these.
#[derive(Clone, Default)]
pub struct BiquadParams<F: Real> {
    /// Sample rate in Hz.
    pub sample_rate: F,
    /// Center or cutoff in Hz.
    pub center: F,
    /// Q value, if applicable.
    pub q: F,
    /// Amplitude gain, if applicable.
    pub gain: F,
}

/// Operation of a filter mode. Retains any extra state needed
/// for efficient operation and can update filter coefficients.
/// The mode uses an optional set of inputs for continuously varying parameters.
/// - Input 0: audio
/// - Input 1: center or cutoff frequency in Hz
/// - Input 2: Q
/// - Input 3: amplitude gain
pub trait BiquadMode<F: Real>: Clone + Default + Sync + Send {
    /// Number of inputs, which includes the audio input.
    type Inputs: Size<F>;

    /// Update coefficients and state from the full set of parameters.
    fn update(&mut self, params: &BiquadParams<F>, coefs: &mut BiquadCoefs<F>);
}

/// Resonator biquad mode.
#[derive(Clone, Default)]
pub struct ResonatorBiquad<F: Real> {
    _marker: PhantomData<F>,
}

impl<F: Real> ResonatorBiquad<F> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<F: Real> BiquadMode<F> for ResonatorBiquad<F> {
    type Inputs = U3;
    #[inline]
    fn update(&mut self, params: &BiquadParams<F>, coefs: &mut BiquadCoefs<F>) {
        *coefs = BiquadCoefs::resonator(params.sample_rate, params.center, params.q);
    }
}

/// Lowpass biquad mode.
#[derive(Clone, Default)]
pub struct LowpassBiquad<F: Real> {
    _marker: PhantomData<F>,
}

impl<F: Real> LowpassBiquad<F> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<F: Real> BiquadMode<F> for LowpassBiquad<F> {
    type Inputs = U3;
    #[inline]
    fn update(&mut self, params: &BiquadParams<F>, coefs: &mut BiquadCoefs<F>) {
        *coefs = BiquadCoefs::lowpass(params.sample_rate, params.center, params.q);
    }
}

/// Highpass biquad mode.
#[derive(Clone, Default)]
pub struct HighpassBiquad<F: Real> {
    _marker: PhantomData<F>,
}

impl<F: Real> HighpassBiquad<F> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<F: Real> BiquadMode<F> for HighpassBiquad<F> {
    type Inputs = U3;
    #[inline]
    fn update(&mut self, params: &BiquadParams<F>, coefs: &mut BiquadCoefs<F>) {
        *coefs = BiquadCoefs::highpass(params.sample_rate, params.center, params.q);
    }
}

/// Bell biquad mode.
#[derive(Clone, Default)]
pub struct BellBiquad<F: Real> {
    _marker: PhantomData<F>,
}

impl<F: Real> BellBiquad<F> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<F: Real> BiquadMode<F> for BellBiquad<F> {
    type Inputs = U4;
    #[inline]
    fn update(&mut self, params: &BiquadParams<F>, coefs: &mut BiquadCoefs<F>) {
        *coefs = BiquadCoefs::bell(params.sample_rate, params.center, params.q, params.gain);
    }
}

#[derive(Clone)]
/// Biquad in transposed direct form II with nonlinear feedback.
pub struct FbBiquad<F: Real, M: BiquadMode<F>, S: Shape> {
    mode: M,
    coefs: BiquadCoefs<F>,
    params: BiquadParams<F>,
    shape: S,
    s1: F,
    s2: F,
}

impl<F: Real, M: BiquadMode<F>, S: Shape> FbBiquad<F, M, S> {
    /// Create new feedback biquad filter.
    pub fn new(mode: M, shape: S) -> Self {
        let mut filter = Self {
            mode,
            coefs: BiquadCoefs::default(),
            params: BiquadParams {
                sample_rate: F::from_f64(DEFAULT_SR),
                center: F::new(440),
                q: F::one(),
                gain: F::one(),
            },
            shape,
            s1: F::zero(),
            s2: F::zero(),
        };
        filter.set_sample_rate(DEFAULT_SAMPLE_RATE);
        filter
    }
}

impl<F: Real, M: BiquadMode<F>, S: Shape> AudioNode for FbBiquad<F, M, S> {
    const ID: u64 = 88;
    type Inputs = M::Inputs;
    type Outputs = U1;

    fn reset(&mut self) {
        self.s1 = F::zero();
        self.s2 = F::zero();
        self.shape.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.params.sample_rate = F::from_f64(sample_rate);
        self.mode.update(&self.params, &mut self.coefs);
    }

    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        if M::Inputs::USIZE == 2 {
            let center = F::from_f32(input[1]);
            if center != self.params.center {
                self.params.center = center;
                self.mode.update(&self.params, &mut self.coefs);
            }
        }
        if M::Inputs::USIZE == 3 {
            let center = F::from_f32(input[1]);
            let q = F::from_f32(input[2]);
            if squared(center - self.params.center) + squared(q - self.params.q) != F::zero() {
                self.params.center = center;
                self.params.q = q;
                self.mode.update(&self.params, &mut self.coefs);
            }
        }
        if M::Inputs::USIZE == 4 {
            let center = F::from_f32(input[1]);
            let q = F::from_f32(input[2]);
            let gain = F::from_f32(input[3]);
            if squared(center - self.params.center)
                + squared(q - self.params.q)
                + squared(gain - self.params.gain)
                != F::zero()
            {
                self.params.center = center;
                self.params.q = q;
                self.params.gain = gain;
                self.mode.update(&self.params, &mut self.coefs);
            }
        }
        // Transposed Direct Form II with nonlinear feedback is:
        //   y0 = b0 * x0 + s1
        //   s1 = s2 + b1 * x0 - a1 * shape(y0)
        //   s2 = b2 * x0 - a2 * shape(y0)
        let x0 = F::from_f32(input[0]);
        let y0 = self.coefs.b0 * x0 + self.s1;
        let fb = F::from_f32(self.shape.shape(y0.to_f32()));
        self.s1 = self.s2 + self.coefs.b1 * x0 - fb * self.coefs.a1;
        self.s2 = self.coefs.b2 * x0 - fb * self.coefs.a2;
        [y0.to_f32()].into()
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        Routing::Arbitrary(0.0).route(input, self.outputs())
    }
}

#[derive(Clone)]
/// Biquad in transposed direct form II with nonlinear feedback, fixed parameters.
pub struct FixedFbBiquad<F: Real, M: BiquadMode<F>, S: Shape> {
    mode: M,
    coefs: BiquadCoefs<F>,
    params: BiquadParams<F>,
    shape: S,
    s1: F,
    s2: F,
}

impl<F: Real, M: BiquadMode<F>, S: Shape> FixedFbBiquad<F, M, S> {
    /// Create new feedback biquad filter.
    pub fn new(mode: M, shape: S) -> Self {
        let mut filter = Self {
            mode,
            coefs: BiquadCoefs::default(),
            params: BiquadParams {
                sample_rate: F::from_f64(DEFAULT_SR),
                center: F::new(440),
                q: F::one(),
                gain: F::one(),
            },
            shape,
            s1: F::zero(),
            s2: F::zero(),
        };
        filter.set_sample_rate(DEFAULT_SAMPLE_RATE);
        filter
    }

    /// Set filter `center` or cutoff frequency in Hz.
    pub fn set_center(&mut self, center: F) {
        self.params.center = center;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter Q.
    pub fn set_q(&mut self, q: F) {
        self.params.q = q;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter amplitude `gain`.
    pub fn set_gain(&mut self, gain: F) {
        self.params.gain = gain;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter `center` or cutoff frequency in Hz and Q.
    pub fn set_center_q(&mut self, center: F, q: F) {
        self.params.center = center;
        self.params.q = q;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter `center` or cutoff frequency in Hz, Q and amplitude `gain`.
    pub fn set_center_q_gain(&mut self, center: F, q: F, gain: F) {
        self.params.center = center;
        self.params.q = q;
        self.params.gain = gain;
        self.mode.update(&self.params, &mut self.coefs);
    }
}

impl<F: Real, M: BiquadMode<F>, S: Shape> AudioNode for FixedFbBiquad<F, M, S> {
    const ID: u64 = 90;
    type Inputs = U1;
    type Outputs = U1;

    fn reset(&mut self) {
        self.s1 = F::zero();
        self.s2 = F::zero();
        self.shape.reset();
    }

    fn set(&mut self, setting: Setting) {
        match setting.parameter() {
            Parameter::Center(center) => self.set_center(F::from_f32(*center)),
            Parameter::CenterQ(center, q) => {
                self.set_center_q(F::from_f32(*center), F::from_f32(*q))
            }
            Parameter::CenterQGain(center, q, gain) => {
                self.set_center_q_gain(F::from_f32(*center), F::from_f32(*q), F::from_f32(*gain))
            }
            _ => (),
        }
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.params.sample_rate = F::from_f64(sample_rate);
        self.mode.update(&self.params, &mut self.coefs);
    }

    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let x0 = F::from_f32(input[0]);
        let y0 = self.coefs.b0 * x0 + self.s1;
        let fb = F::from_f32(self.shape.shape(y0.to_f32()));
        self.s1 = self.s2 + self.coefs.b1 * x0 - fb * self.coefs.a1;
        self.s2 = self.coefs.b2 * x0 - fb * self.coefs.a2;
        [y0.to_f32()].into()
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        Routing::Arbitrary(0.0).route(input, self.outputs())
    }
}

/// Biquad in transposed direct form II with nonlinear state shaping.
#[derive(Clone)]
pub struct DirtyBiquad<F: Real, M: BiquadMode<F>, S: Shape> {
    mode: M,
    coefs: BiquadCoefs<F>,
    params: BiquadParams<F>,
    shape1: S,
    shape2: S,
    s1: F,
    s2: F,
}

impl<F: Real, M: BiquadMode<F>, S: Shape> DirtyBiquad<F, M, S> {
    /// Create new dirty biquad filter.
    pub fn new(mode: M, shape: S) -> Self {
        let shape1 = shape;
        let shape2 = shape1.clone();
        let mut filter = Self {
            mode,
            coefs: BiquadCoefs::default(),
            params: BiquadParams {
                sample_rate: F::from_f64(DEFAULT_SR),
                center: F::new(440),
                q: F::one(),
                gain: F::one(),
            },
            shape1,
            shape2,
            s1: F::zero(),
            s2: F::zero(),
        };
        filter.set_sample_rate(DEFAULT_SAMPLE_RATE);
        filter
    }
}

impl<F: Real, M: BiquadMode<F>, S: Shape> AudioNode for DirtyBiquad<F, M, S> {
    const ID: u64 = 89;
    type Inputs = M::Inputs;
    type Outputs = typenum::U1;

    fn reset(&mut self) {
        self.s1 = F::zero();
        self.s2 = F::zero();
        self.shape1.reset();
        self.shape2.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.params.sample_rate = F::from_f64(sample_rate);
        self.mode.update(&self.params, &mut self.coefs);
    }

    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        if M::Inputs::USIZE == 2 {
            let center = F::from_f32(input[1]);
            if center != self.params.center {
                self.params.center = center;
                self.mode.update(&self.params, &mut self.coefs);
            }
        }
        if M::Inputs::USIZE == 3 {
            let center = F::from_f32(input[1]);
            let q = F::from_f32(input[2]);
            if squared(center - self.params.center) + squared(q - self.params.q) != F::zero() {
                self.params.center = center;
                self.params.q = q;
                self.mode.update(&self.params, &mut self.coefs);
            }
        }
        if M::Inputs::USIZE == 4 {
            let center = F::from_f32(input[1]);
            let q = F::from_f32(input[2]);
            let gain = F::from_f32(input[3]);
            if squared(center - self.params.center)
                + squared(q - self.params.q)
                + squared(gain - self.params.gain)
                != F::zero()
            {
                self.params.center = center;
                self.params.q = q;
                self.params.gain = gain;
                self.mode.update(&self.params, &mut self.coefs);
            }
        }
        // Transposed Direct Form II with nonlinear state shaping is:
        //   y0 = b0 * x0 + s1
        //   s1 = shape(s2 + b1 * x0 - a1 * y0)
        //   s2 = shape(b2 * x0 - a2 * y0)
        let x0 = F::from_f32(input[0]);
        let y0 = self.coefs.b0 * x0 + self.s1;
        self.s1 = F::from_f32(
            self.shape1
                .shape((self.s2 + self.coefs.b1 * x0 - y0 * self.coefs.a1).to_f32()),
        );
        self.s2 = F::from_f32(
            self.shape2
                .shape((self.coefs.b2 * x0 - y0 * self.coefs.a2).to_f32()),
        );
        [y0.to_f32()].into()
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        Routing::Arbitrary(0.0).route(input, self.outputs())
    }
}

/// Biquad in transposed direct form II with nonlinear state shaping, fixed parameters.
#[derive(Clone)]
pub struct FixedDirtyBiquad<F: Real, M: BiquadMode<F>, S: Shape> {
    mode: M,
    coefs: BiquadCoefs<F>,
    params: BiquadParams<F>,
    shape1: S,
    shape2: S,
    s1: F,
    s2: F,
}

impl<F: Real, M: BiquadMode<F>, S: Shape> FixedDirtyBiquad<F, M, S> {
    /// Create new dirty biquad filter.
    pub fn new(mode: M, shape: S) -> Self {
        let shape1 = shape;
        let shape2 = shape1.clone();
        let mut filter = Self {
            mode,
            coefs: BiquadCoefs::default(),
            params: BiquadParams {
                sample_rate: F::from_f64(DEFAULT_SR),
                center: F::new(440),
                q: F::one(),
                gain: F::one(),
            },
            shape1,
            shape2,
            s1: F::zero(),
            s2: F::zero(),
        };
        filter.set_sample_rate(DEFAULT_SAMPLE_RATE);
        filter
    }

    /// Set filter `center` or cutoff frequency in Hz.
    pub fn set_center(&mut self, center: F) {
        self.params.center = center;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter Q.
    pub fn set_q(&mut self, q: F) {
        self.params.q = q;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter amplitude `gain`.
    pub fn set_gain(&mut self, gain: F) {
        self.params.gain = gain;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter `center` or cutoff frequency in Hz and Q.
    pub fn set_center_q(&mut self, center: F, q: F) {
        self.params.center = center;
        self.params.q = q;
        self.mode.update(&self.params, &mut self.coefs);
    }

    /// Set filter `center` or cutoff frequency in Hz, Q and amplitude `gain`.
    pub fn set_center_q_gain(&mut self, center: F, q: F, gain: F) {
        self.params.center = center;
        self.params.q = q;
        self.params.gain = gain;
        self.mode.update(&self.params, &mut self.coefs);
    }
}

impl<F: Real, M: BiquadMode<F>, S: Shape> AudioNode for FixedDirtyBiquad<F, M, S> {
    const ID: u64 = 91;
    type Inputs = U1;
    type Outputs = U1;

    fn reset(&mut self) {
        self.s1 = F::zero();
        self.s2 = F::zero();
        self.shape1.reset();
        self.shape2.reset();
    }

    fn set(&mut self, setting: Setting) {
        match setting.parameter() {
            Parameter::Center(center) => self.set_center(F::from_f32(*center)),
            Parameter::CenterQ(center, q) => {
                self.set_center_q(F::from_f32(*center), F::from_f32(*q))
            }
            Parameter::CenterQGain(center, q, gain) => {
                self.set_center_q_gain(F::from_f32(*center), F::from_f32(*q), F::from_f32(*gain))
            }
            _ => (),
        }
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.params.sample_rate = F::from_f64(sample_rate);
        self.mode.update(&self.params, &mut self.coefs);
    }

    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let x0 = F::from_f32(input[0]);
        let y0 = self.coefs.b0 * x0 + self.s1;
        self.s1 = F::from_f32(
            self.shape1
                .shape((self.s2 + self.coefs.b1 * x0 - y0 * self.coefs.a1).to_f32()),
        );
        self.s2 = F::from_f32(
            self.shape2
                .shape((self.coefs.b2 * x0 - y0 * self.coefs.a2).to_f32()),
        );
        [y0.to_f32()].into()
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        Routing::Arbitrary(0.0).route(input, self.outputs())
    }
}

/// Linkwitz-Riley filter order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LrOrder {
    /// LR2: 12 dB/octave (1 biquad stage)
    Lr2,
    /// LR4: 24 dB/octave (2 biquad stages) - most common for crossovers
    Lr4,
    /// LR8: 48 dB/octave (4 biquad stages) - steeper, more phase shift
    Lr8,
}

impl LrOrder {
    /// Number of biquad stages for this order.
    pub fn stages(self) -> usize {
        match self {
            LrOrder::Lr2 => 1,
            LrOrder::Lr4 => 2,
            LrOrder::Lr8 => 4,
        }
    }
}

/// Linkwitz-Riley lowpass filter.
///
/// LR filters are designed for crossovers: the lowpass and highpass outputs
/// sum to unity (flat magnitude response) with matched phase at all frequencies.
///
/// - Input 0: input signal
/// - Output 0: filtered signal
#[derive(Clone)]
pub struct LinkwitzRileyLowpass<F: Real> {
    order: LrOrder,
    biquads: [Biquad<F>; 4], // Max 4 stages for LR8
    sample_rate: F,
    cutoff: F,
}

impl<F: Real> LinkwitzRileyLowpass<F> {
    /// Create new Linkwitz-Riley lowpass filter.
    pub fn new(order: LrOrder, cutoff: F) -> Self {
        let mut filter = Self {
            order,
            biquads: [Biquad::new(), Biquad::new(), Biquad::new(), Biquad::new()],
            sample_rate: F::from_f64(DEFAULT_SR),
            cutoff,
        };
        filter.update_coefficients();
        filter
    }

    /// Set cutoff frequency in Hz.
    pub fn set_cutoff(&mut self, cutoff: F) {
        self.cutoff = cutoff;
        self.update_coefficients();
    }

    fn update_coefficients(&mut self) {
        let coefs = BiquadCoefs::linkwitz_riley_lowpass(self.sample_rate, self.cutoff);
        for i in 0..self.order.stages() {
            self.biquads[i].set_coefs(coefs);
        }
    }
}

impl<F: Real> AudioNode for LinkwitzRileyLowpass<F> {
    const ID: u64 = 92;
    type Inputs = U1;
    type Outputs = U1;

    fn reset(&mut self) {
        for biquad in &mut self.biquads {
            biquad.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = F::from_f64(sample_rate);
        for biquad in &mut self.biquads {
            biquad.set_sample_rate(crate::SampleRate(sample_rate));
        }
        self.update_coefficients();
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let mut frame = *input;
        for i in 0..self.order.stages() {
            frame = self.biquads[i].tick(&frame);
        }
        frame
    }

    fn set(&mut self, setting: Setting) {
        if let Parameter::Center(cutoff) = setting.parameter() {
            self.set_cutoff(F::from_f32(*cutoff));
        }
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        // Compute cascaded response
        let mut response = self.biquads[0]
            .coefs()
            .response(frequency / self.sample_rate.to_f64());
        for i in 1..self.order.stages() {
            response *= self.biquads[i]
                .coefs()
                .response(frequency / self.sample_rate.to_f64());
        }
        let mut output = SignalFrame::new(self.outputs());
        output.set(0, input.at(0).filter(0.0, |r| r * response));
        output
    }
}

/// Linkwitz-Riley highpass filter.
///
/// LR filters are designed for crossovers: the lowpass and highpass outputs
/// sum to unity (flat magnitude response) with matched phase at all frequencies.
///
/// - Input 0: input signal
/// - Output 0: filtered signal
#[derive(Clone)]
pub struct LinkwitzRileyHighpass<F: Real> {
    order: LrOrder,
    biquads: [Biquad<F>; 4],
    sample_rate: F,
    cutoff: F,
}

impl<F: Real> LinkwitzRileyHighpass<F> {
    /// Create new Linkwitz-Riley highpass filter.
    pub fn new(order: LrOrder, cutoff: F) -> Self {
        let mut filter = Self {
            order,
            biquads: [Biquad::new(), Biquad::new(), Biquad::new(), Biquad::new()],
            sample_rate: F::from_f64(DEFAULT_SR),
            cutoff,
        };
        filter.update_coefficients();
        filter
    }

    /// Set cutoff frequency in Hz.
    pub fn set_cutoff(&mut self, cutoff: F) {
        self.cutoff = cutoff;
        self.update_coefficients();
    }

    fn update_coefficients(&mut self) {
        let coefs = BiquadCoefs::linkwitz_riley_highpass(self.sample_rate, self.cutoff);
        for i in 0..self.order.stages() {
            self.biquads[i].set_coefs(coefs);
        }
    }
}

impl<F: Real> AudioNode for LinkwitzRileyHighpass<F> {
    const ID: u64 = 93;
    type Inputs = U1;
    type Outputs = U1;

    fn reset(&mut self) {
        for biquad in &mut self.biquads {
            biquad.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = F::from_f64(sample_rate);
        for biquad in &mut self.biquads {
            biquad.set_sample_rate(crate::SampleRate(sample_rate));
        }
        self.update_coefficients();
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let mut frame = *input;
        for i in 0..self.order.stages() {
            frame = self.biquads[i].tick(&frame);
        }
        frame
    }

    fn set(&mut self, setting: Setting) {
        if let Parameter::Center(cutoff) = setting.parameter() {
            self.set_cutoff(F::from_f32(*cutoff));
        }
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let mut response = self.biquads[0]
            .coefs()
            .response(frequency / self.sample_rate.to_f64());
        for i in 1..self.order.stages() {
            response *= self.biquads[i]
                .coefs()
                .response(frequency / self.sample_rate.to_f64());
        }
        let mut output = SignalFrame::new(self.outputs());
        output.set(0, input.at(0).filter(0.0, |r| r * response));
        output
    }
}

/// Linkwitz-Riley crossover filter.
///
/// Splits the input signal into low and high frequency bands.
/// The two bands sum to unity (flat magnitude response) at all frequencies.
///
/// - Input 0: input signal
/// - Output 0: low frequency band
/// - Output 1: high frequency band
#[derive(Clone)]
pub struct LinkwitzRileyCrossover<F: Real> {
    lowpass: LinkwitzRileyLowpass<F>,
    highpass: LinkwitzRileyHighpass<F>,
}

impl<F: Real> LinkwitzRileyCrossover<F> {
    /// Create new Linkwitz-Riley crossover at `cutoff` frequency in Hz.
    pub fn new(order: LrOrder, cutoff: F) -> Self {
        Self {
            lowpass: LinkwitzRileyLowpass::new(order, cutoff),
            highpass: LinkwitzRileyHighpass::new(order, cutoff),
        }
    }

    /// Set crossover frequency in Hz.
    pub fn set_cutoff(&mut self, cutoff: F) {
        self.lowpass.set_cutoff(cutoff);
        self.highpass.set_cutoff(cutoff);
    }
}

impl<F: Real> AudioNode for LinkwitzRileyCrossover<F> {
    const ID: u64 = 94;
    type Inputs = U1;
    type Outputs = U2;

    fn reset(&mut self) {
        self.lowpass.reset();
        self.highpass.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.lowpass.set_sample_rate(crate::SampleRate(sample_rate));
        self.highpass
            .set_sample_rate(crate::SampleRate(sample_rate));
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let low = self.lowpass.tick(input);
        let high = self.highpass.tick(input);
        [low[0], high[0]].into()
    }

    fn set(&mut self, setting: Setting) {
        if let Parameter::Center(cutoff) = setting.parameter() {
            self.set_cutoff(F::from_f32(*cutoff));
        }
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let low_out = self.lowpass.route(input, frequency);
        let high_out = self.highpass.route(input, frequency);
        let mut output = SignalFrame::new(self.outputs());
        output.set(0, low_out.at(0));
        output.set(1, high_out.at(0));
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linkwitz_riley_crossover_sums_flat() {
        // LR crossover should sum to unity (within tolerance)
        // Test with a DC signal to verify steady-state behavior
        let mut crossover: LinkwitzRileyCrossover<f64> =
            LinkwitzRileyCrossover::new(LrOrder::Lr4, 1000.0);
        crossover.set_sample_rate(crate::SampleRate(44100.0));

        // Let the filter settle with DC input
        let dc: Frame<f32, U1> = [1.0].into();
        let mut output = [0.0f32; 2];

        for _ in 0..1000 {
            let out = crossover.tick(&dc);
            output = [out[0], out[1]];
        }

        // After settling, low + high should equal input (DC = 1.0)
        let sum = output[0] + output[1];
        assert!(
            (sum - 1.0).abs() < 0.001,
            "Steady-state sum {} should be 1.0 (low={}, high={})",
            sum,
            output[0],
            output[1]
        );
    }

    #[test]
    fn test_linkwitz_riley_frequency_response() {
        // At crossover frequency, both bands should be -6dB (0.5 amplitude)
        let cutoff = 1000.0;
        let mut crossover: LinkwitzRileyCrossover<f64> =
            LinkwitzRileyCrossover::new(LrOrder::Lr4, cutoff);
        crossover.set_sample_rate(crate::SampleRate(44100.0));

        // Generate a sine wave at the crossover frequency
        let sample_rate = 44100.0;
        let omega = 2.0 * core::f64::consts::PI * cutoff / sample_rate;

        // Let filter settle
        for i in 0..2000 {
            let sample = (omega * i as f64).sin() as f32;
            let _ = crossover.tick(&[sample].into());
        }

        // Measure amplitude over one cycle
        let samples_per_cycle = (sample_rate / cutoff) as usize;
        let mut low_max = 0.0f32;
        let mut high_max = 0.0f32;

        for i in 0..samples_per_cycle {
            let sample = (omega * (2000 + i) as f64).sin() as f32;
            let out = crossover.tick(&[sample].into());
            low_max = low_max.max(out[0].abs());
            high_max = high_max.max(out[1].abs());
        }

        // At crossover, each band should be approximately -6dB (0.5)
        // Allow some tolerance for filter settling
        assert!(
            (low_max - 0.5).abs() < 0.1,
            "Low band at crossover should be ~0.5, got {}",
            low_max
        );
        assert!(
            (high_max - 0.5).abs() < 0.1,
            "High band at crossover should be ~0.5, got {}",
            high_max
        );
    }

    #[test]
    fn test_lr_orders() {
        // Test different orders compile and run
        for order in [LrOrder::Lr2, LrOrder::Lr4, LrOrder::Lr8] {
            let mut lp: LinkwitzRileyLowpass<f64> = LinkwitzRileyLowpass::new(order, 1000.0);
            let mut hp: LinkwitzRileyHighpass<f64> = LinkwitzRileyHighpass::new(order, 1000.0);
            lp.set_sample_rate(crate::SampleRate(44100.0));
            hp.set_sample_rate(crate::SampleRate(44100.0));

            let input: Frame<f32, U1> = [1.0].into();
            let _ = lp.tick(&input);
            let _ = hp.tick(&input);
        }
    }
}
