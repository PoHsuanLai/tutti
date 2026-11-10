//! The reverb model (design 022): a reverb as a plain value with analytic
//! queries.
//!
//! A [`Network`] describes what a reverb *is*, in the state-space form of a
//! feedback delay network: delay lines whose outputs are mixed by a matrix,
//! filtered by per-line absorption and fed back. It is a description for design
//! and analysis, never an instruction stream: no code interprets every network,
//! so an algorithm's realization stays free to run its own shape. Everything here is
//! a pure function of the value (no realization, no signal, no clock), fixed in
//! capacity and allocation-free to build.
//!
//! **A `Network` is a control-side value.** It holds room for [`MAX_STAGES`]
//! stages of about 7 KB each, some 56 KB in all, which is too large to build or
//! copy on an audio thread (whose stack may be 512 KB). It is allocation-free
//! (no heap, so building one never waits on the allocator), not small.
//!
//! Units: whole frames are [`Samples`] (the pre-delay); non-negative gains per
//! trip are [`Amplitude`] (an absorption's). Plain `f32` is kept where nothing
//! covers the quantity, one reason per kind: delays and modulation depths in
//! *fractional* frames ([`Samples`] is whole); *signed* coefficients, which
//! [`Amplitude`] (floored at zero) cannot hold (tap and all-pass gains, the
//! matrix, the input and output gains of an FDN); and `Feedback`, which caps at
//! 0.99, cannot hold a loop gain of 1. The queries ([`Network::rt60`], [`tail`](crate::System::tail),
//! [`is_stable`](crate::Lti::is_stable), the densities) read **centre** lengths:
//! modulation moves a line by a few frames around its centre and the queries do
//! not model that.

mod bounds;
mod gain;
mod inline;
mod queries;
mod response;

pub use bounds::Bounds;
pub use gain::{loop_gain, rt60_from_gain};
pub use inline::{Full, Inline};

use tutti_types::{Amplitude, Db, Hz, SampleRate, Samples, Seconds, StereoWidth};

/// The most delay lines one [`Fdn`] holds.
pub const MAX_LINES: usize = 64;
/// The most stages in series in a [`Network`].
pub const MAX_STAGES: usize = 8;
/// The most channels an [`Fdn`] outputs or an all-pass stage runs on.
pub const MAX_CHANNELS: usize = 8;
/// The most taps in one [`Taps`] stage.
pub const MAX_TAPS: usize = 64;
/// The most entries of a [`Matrix::Dense`]: a 16 by 16 matrix.
pub const MAX_DENSE: usize = 256;

/// A frequency band of the decay: below the lower crossover, between, above the
/// upper.
///
/// For an absorption that is not given by bands ([`Absorption::OnePole`]) the
/// three are three points of its response: DC, a quarter of the sample rate and
/// Nyquist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Band {
    /// The lowest band.
    Low,
    /// The middle band.
    Mid,
    /// The highest band.
    High,
}

impl Band {
    /// Every band, lowest first.
    pub const ALL: [Band; 3] = [Band::Low, Band::Mid, Band::High];

    /// The band's place in an array of three.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Band::Low => 0,
            Band::Mid => 1,
            Band::High => 2,
        }
    }
}

/// A reverb as a value (design 022 section 3).
///
/// The stages run in series, input to output, after the pre-delay. The network
/// is wet-only: it has no direct path from input to output (a network that has
/// one says so, [`has_direct_path`](Self::has_direct_path)); the node owns the
/// dry mix.
#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    /// The rate the frame counts are in.
    pub rate: SampleRate,
    /// The pre-delay in whole frames: `round(seconds * rate)`.
    pub pre_delay: Samples,
    /// The stages, in series.
    pub stages: Inline<Stage, MAX_STAGES>,
    /// The stereo width of the wet signal.
    pub width: StereoWidth,
    /// The wet gain.
    pub wet: Db,
}

impl Network {
    /// An empty network at `rate`: no stages, natural width, 0 dB.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        Self {
            rate,
            pre_delay: Samples::ZERO,
            stages: Inline::new(),
            width: StereoWidth::NATURAL,
            wet: Db(0.0),
        }
    }

    /// Makes this an empty network at `rate`, in place: what
    /// [`Design::design_into`](crate::Design::design_into) starts from. Unlike [`new`](Self::new) it builds
    /// no value of the network's size.
    pub fn clear(&mut self, rate: SampleRate) {
        self.rate = rate;
        self.pre_delay = Samples::ZERO;
        self.stages.clear();
        self.width = StereoWidth::NATURAL;
        self.wet = Db(0.0);
    }
}

/// The pre-delay in whole frames, for [`Network::pre_delay`].
#[must_use]
pub fn pre_delay_frames(pre_delay: Seconds, rate: SampleRate) -> Samples {
    pre_delay.to_samples(rate)
}

/// One stage of a [`Network`].
// Boxing the large variant would allocate: the size is the price of a network
// built without the heap. Every `Stage` is about 7 KB whatever its variant (an
// `Inline` array of `Stage` is as large as its largest member), so a short
// all-pass stage costs as much as an `Fdn`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// Sparse taps: early reflections. No feedback.
    Taps(Taps),
    /// A series Schroeder all-pass on each output channel.
    AllPass(Inline<AllPass, MAX_CHANNELS>),
    /// Delay lines with feedback.
    Fdn(Fdn),
    /// A dense FIR, the limiting case: what analysis needs of it, not its data.
    Fir(FirSummary),
}

impl Default for Stage {
    fn default() -> Self {
        Stage::Taps(Taps::default())
    }
}

/// A sparse tap set: early reflections.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Taps {
    /// The taps.
    pub taps: Inline<Tap, MAX_TAPS>,
}

/// One tap of [`Taps`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tap {
    /// The delay in frames. A tap at less than one frame is a direct path.
    pub delay: f32,
    /// The gain: signed, so not an `Amplitude`.
    pub gain: f32,
    /// The output channel it feeds.
    pub channel: u8,
}

/// The two all-pass filters a network describes, which differ in their
/// transfer function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AllPassForm {
    /// The Schroeder (lattice) all-pass `H(z) = (z^-D - g) / (1 - g z^-D)`:
    /// `|H| = 1` at every frequency, with a direct term `-g`.
    #[default]
    Lattice,
    /// Freeverb's: `out = buf - in; buf = in + g buf` (the buffer read `D`
    /// frames late), that is `H(z) = ((1 + g) z^-D - 1) / (1 - g z^-D)`. It is
    /// not unit magnitude: `|H|` is 1 at DC for `g = 0.5` and reaches
    /// `(2 + g) / (1 + g)` at Nyquist for an even `D`. Its direct term is always
    /// `-1`.
    Freeverb,
}

/// An all-pass of delay `D` frames and coefficient `g`, `|g| < 1`, in one of
/// two [`AllPassForm`]s.
///
/// The impulse response of either form after its first echoes falls by `g`
/// every `D` frames, so [`tail`](crate::System::tail) treats them alike.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AllPass {
    /// The delay in fractional frames.
    pub delay: f32,
    /// The coefficient: signed, so not an `Amplitude`.
    pub gain: f32,
    /// How its delay is modulated.
    pub modulation: Modulation,
    /// Which transfer function.
    pub form: AllPassForm,
}

impl AllPass {
    /// Whether the output depends on the input of the same frame: the
    /// [`Lattice`](AllPassForm::Lattice) form has the direct term `-g` (none
    /// at `g = 0`, a plain delay), the [`Freeverb`](AllPassForm::Freeverb) form
    /// always has `-1`.
    #[must_use]
    pub fn has_direct_term(&self) -> bool {
        match self.form {
            AllPassForm::Lattice => self.gain != 0.0,
            AllPassForm::Freeverb => true,
        }
    }

    /// The group delay at DC of a [`Lattice`](AllPassForm::Lattice) all-pass,
    /// `D (1 + g) / (1 - g)`, in frames: what it adds to a loop's length at low
    /// frequency. Infinity when `|g| >= 1`, and for the
    /// [`Freeverb`](AllPassForm::Freeverb) form, which is not unit magnitude
    /// and whose DC gain `g / (1 - g)` the loop analysis does not model.
    #[must_use]
    pub fn low_frequency_delay(&self) -> f32 {
        if self.form == AllPassForm::Lattice && self.gain.abs() < 1.0 {
            self.delay * (1.0 + self.gain) / (1.0 - self.gain)
        } else {
            f32::INFINITY
        }
    }
}

/// How a delay moves around its centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Modulation {
    /// The excursion either side of the centre, in frames.
    pub depth: f32,
    /// The rate.
    pub rate: Hz,
}

impl Modulation {
    /// No modulation.
    pub const NONE: Modulation = Modulation {
        depth: 0.0,
        rate: Hz(0.0),
    };
}

impl Default for Modulation {
    fn default() -> Self {
        Self::NONE
    }
}

/// One signed coefficient per delay line (or matrix row): the input and output
/// gains of an FDN and a circulant's first row can be negative, so these are
/// not `Amplitude`.
pub type Gains = Inline<f32, MAX_LINES>;

/// Delay lines with feedback: `s(n + m) = A G(s(n)) + B u(n)`,
/// `y(n) = C s(n)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Fdn {
    /// The lines.
    pub lines: Inline<Line, MAX_LINES>,
    /// The feedback matrix `A`.
    pub matrix: Matrix,
    /// The input gains `B`: one per line.
    pub input: Gains,
    /// The output gains `C`.
    pub output: OutputMap,
}

/// The output gains `C`: one row per output channel, one gain per line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputMap {
    /// The rows.
    pub rows: Inline<Gains, MAX_CHANNELS>,
}

/// One delay line of an [`Fdn`] and its absorption `G`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    /// The centre length in fractional frames. A stable line is finite and
    /// positive: a delay-free loop is not a loop a network can have.
    pub length: f32,
    /// How the length is modulated.
    pub modulation: Modulation,
    /// What the line absorbs per trip.
    pub absorption: Absorption,
    /// An all-pass nested in the loop (the plate's diffusers).
    pub diffuser: Option<AllPass>,
}

impl Line {
    /// The length the decay formulas use: the centre length plus the nested
    /// all-pass's group delay at DC.
    #[must_use]
    pub fn effective_length(&self) -> f32 {
        self.length + self.diffuser.map_or(0.0, |d| d.low_frequency_delay())
    }
}

impl Default for Line {
    fn default() -> Self {
        Line {
            length: 0.0,
            modulation: Modulation::NONE,
            absorption: Absorption::Gain(Amplitude::SILENT),
            diffuser: None,
        }
    }
}

/// The feedback matrix `A` of an [`Fdn`]. Structured, not a bag of floats.
// Boxed, `Dense` would allocate; it is a control-side tool and the rest are small.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Matrix {
    /// The identity: every line feeds only itself (combs).
    #[default]
    Diagonal,
    /// `I - (2/N) 1 1^T`, orthogonal, O(N) to apply.
    Householder,
    /// The Hadamard matrix over `sqrt(N)`, orthogonal; `N` a power of two.
    Hadamard,
    /// `I - 2 v v^T` for the vector `v`, one entry a line: a reflection, O(N)
    /// to apply. Orthogonal when `v` is a unit vector (checked, to a
    /// tolerance, not by construction); a `v` shorter than a unit vector is a
    /// contraction (`|1 - 2 |v|^2| <= 1`), never a gain. `v` along one axis
    /// couples no lines, and `v` along `1/sqrt(N)` is [`Householder`](Self::Householder):
    /// the vectors between are how much the lines mix.
    Reflection(Gains),
    /// A circulant matrix from its first row (checked orthogonal by
    /// [`Fdn::is_stable`], not by construction).
    Circulant(Gains),
    /// A dense row-major `N` by `N` matrix, a control-side tool.
    Dense(Inline<f32, MAX_DENSE>),
}

/// What a line absorbs per trip, `G`: the gain of one trip through the loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Absorption {
    /// The same gain at every frequency.
    Gain(Amplitude),
    /// A one-pole low-pass `(1 - damp) / (1 - damp z^-1)` (unity at DC) times
    /// `gain`: Freeverb's comb, the plate's damping. The pole `damp` is a
    /// coefficient (signed, so not an `Amplitude`) and must satisfy
    /// `|damp| < 1`: see [`Absorption::gain`] for what an out-of-range pole
    /// reads as.
    OnePole {
        /// The gain at DC.
        gain: Amplitude,
        /// The pole.
        damp: f32,
    },
    /// A gain per band.
    Bands {
        /// The low band's gain.
        low: Amplitude,
        /// The middle band's gain.
        mid: Amplitude,
        /// The high band's gain.
        high: Amplitude,
        /// Where the bands meet.
        crossover: [Hz; 2],
    },
}

impl Absorption {
    /// The gain per trip in `band`: for [`OnePole`](Self::OnePole) its
    /// magnitude at DC, a quarter of the rate and Nyquist.
    ///
    /// A one-pole whose pole is not finite or has `|damp| >= 1` is not a
    /// filter that settles, and has no response to read: every band returns
    /// infinity, which no stability or decay query accepts. (A pole in
    /// `-1 < damp < 0` is a valid high-pass-ish filter whose Nyquist gain
    /// exceeds `gain`, and reads as such.)
    #[must_use]
    pub fn gain(&self, band: Band) -> Amplitude {
        match *self {
            Absorption::Gain(g) => g,
            Absorption::OnePole { gain, damp } => {
                if !(damp.is_finite() && damp.abs() < 1.0) {
                    return Amplitude(f32::INFINITY);
                }
                let d = f64::from(damp);
                let response = match band {
                    Band::Low => 1.0,
                    Band::Mid => (1.0 - d).abs() / libm::sqrt(1.0 + d * d),
                    Band::High => (1.0 - d).abs() / (1.0 + d).abs(),
                };
                Amplitude((f64::from(gain.get()) * response) as f32)
            }
            Absorption::Bands { low, mid, high, .. } => match band {
                Band::Low => low,
                Band::Mid => mid,
                Band::High => high,
            },
        }
    }

    /// Bands that give a loop of `length` frames the three RT60s
    /// `rt60 = [low, mid, high]` (the design from band decay times:
    /// [`loop_gain`] per band).
    #[must_use]
    pub fn from_rt60(
        rt60: [Seconds; 3],
        crossover: [Hz; 2],
        length: f32,
        rate: SampleRate,
    ) -> Self {
        Absorption::Bands {
            low: loop_gain(length, rt60[0], rate),
            mid: loop_gain(length, rt60[1], rate),
            high: loop_gain(length, rt60[2], rate),
            crossover,
        }
    }
}

/// What the analysis needs of a dense impulse response; the response itself
/// stays with the realization that convolves it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FirSummary {
    /// The response's length.
    pub length: Samples,
    /// Frames before its first non-zero sample.
    pub onset: Samples,
    /// The convolver's processing latency.
    pub latency: Samples,
    /// A Schroeder-integrated RT60 per band, lowest first.
    pub rt60: [Seconds; 3],
    /// Its energy.
    pub energy: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ring;
    use crate::{Lti as _, System as _};

    /// The pre-delay is whole frames, rounded: 1 ms at 48 kHz is 48, more than half a
    /// frame rounds up, and a negative or NaN span is none.
    ///
    /// Mutation (run): `to_samples_floor` for `to_samples` fails the 0.6-frame
    /// row.
    #[test]
    fn the_pre_delay_is_rounded_to_whole_frames() {
        let rate = SampleRate(48_000.0);
        assert_eq!(pre_delay_frames(Seconds(0.001), rate), Samples(48));
        assert_eq!(pre_delay_frames(Seconds(0.6 / 48_000.0), rate), Samples(1));
        assert_eq!(
            pre_delay_frames(Seconds(0.4 / 48_000.0), rate),
            Samples::ZERO
        );
        assert_eq!(pre_delay_frames(Seconds(-1.0), rate), Samples::ZERO);
        assert_eq!(pre_delay_frames(Seconds(f64::NAN), rate), Samples::ZERO);
    }

    /// A `Network` lives on the stack of the audio thread: the whole value stays
    /// under 64 KiB (design 022 guessed "a few kilobytes"; it is about 55 KB: `MAX_STAGES` of the
    /// largest stage).
    ///
    /// Mutation (run): `MAX_LINES` 64 -> 1024 fails it.
    #[test]
    fn a_network_fits_the_audio_thread_stack() {
        let bytes = std::mem::size_of::<Network>();
        println!("size_of::<Network>() = {bytes}");
        assert!(bytes < 64 * 1024, "{bytes}");
    }

    /// An empty network recirculates nothing: no decay, no tail, stable.
    #[test]
    fn an_empty_network_is_silent_and_stable() {
        let n = Network::new(SampleRate(44_100.0));
        assert_eq!(n.rt60(Band::Mid), Seconds(0.0));
        assert_eq!(n.tail(Db(-60.0)), Ring::Finite(Seconds(0.0)));
        assert!(n.is_stable() && !n.is_lossless());
    }

    /// The two forms differ in their direct term: a lattice has `-g` (none at
    /// `g = 0`, a plain delay), Freeverb's always `-1`; only the lattice has a
    /// finite low-frequency group delay for the loop analysis.
    ///
    /// Mutations (run): `self.gain != 0.0` -> `true` fails the lattice-at-zero
    /// row; the Freeverb arm returning `self.gain != 0.0` fails the last row;
    /// dropping the form test from `low_frequency_delay` fails the Freeverb row.
    #[test]
    fn the_forms_differ_in_their_direct_term_and_group_delay() {
        let ap = |form, gain| AllPass {
            delay: 100.0,
            gain,
            modulation: Modulation::NONE,
            form,
        };
        assert!(ap(AllPassForm::Lattice, 0.5).has_direct_term());
        assert!(ap(AllPassForm::Lattice, -0.5).has_direct_term());
        assert!(!ap(AllPassForm::Lattice, 0.0).has_direct_term());
        assert!(ap(AllPassForm::Freeverb, 0.0).has_direct_term());
        assert!(ap(AllPassForm::Freeverb, 0.5).has_direct_term());
        assert_eq!(ap(AllPassForm::Lattice, 0.5).low_frequency_delay(), 300.0);
        assert_eq!(
            ap(AllPassForm::Freeverb, 0.5).low_frequency_delay(),
            f32::INFINITY
        );
    }

    /// The lattice form is unit magnitude at every frequency, from its
    /// recurrence `y[n] = -g x[n] + x[n-D] + g y[n-D]`; with the same `g` the
    /// Freeverb form (`((1+g) z^-D - 1) / (1 - g z^-D)`) is not. Both
    /// impulse responses are run from their difference equations.
    ///
    /// Mutation (run): the sign of `-g * x` flipped in the lattice recurrence
    /// breaks its unit magnitude and fails the first loop.
    #[test]
    fn the_lattice_is_unit_magnitude_and_the_freeverb_form_is_not() {
        let (d, g) = (8usize, 0.5f64);
        let len = 40 * d;
        let mut lattice = vec![0.0f64; len];
        let mut freeverb = vec![0.0f64; len];
        // Freeverb: buf[n] = x[n] + g buf[n-D]; y[n] = buf[n-D] - x[n].
        let mut buf = vec![0.0f64; len];
        for n in 0..len {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let back = |v: &[f64]| if n >= d { v[n - d] } else { 0.0 };
            lattice[n] = -g * x + (if n == d { 1.0 } else { 0.0 }) + g * back(&lattice);
            buf[n] = x + g * back(&buf);
            freeverb[n] = back(&buf) - x;
        }
        let mag = |h: &[f64], w: f64| {
            let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &x)| {
                (
                    re + x * libm::cos(w * n as f64),
                    im - x * libm::sin(w * n as f64),
                )
            });
            f64::hypot(re, im)
        };
        let omegas = [0.0, 0.1, 0.4, std::f64::consts::PI / d as f64, 2.0, 3.0];
        assert!(
            omegas
                .iter()
                .all(|&w| (mag(&lattice, w) - 1.0).abs() < 1e-6)
        );
        assert!(
            omegas
                .iter()
                .any(|&w| (mag(&freeverb, w) - 1.0).abs() > 0.5)
        );
    }
}
