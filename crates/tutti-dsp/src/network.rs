//! The reverb model (design 022): a reverb as a plain value with analytic
//! queries.
//!
//! A [`Network`] describes what a reverb *is*, in the state-space form of a
//! feedback delay network: delay lines whose outputs are mixed by a matrix,
//! filtered by per-line absorption and fed back. It is a description for design
//! and analysis, never an instruction stream: no code interprets every network,
//! so an algorithm's kernel stays free to run its own shape. Everything here is
//! a pure function of the value (no kernel, no signal, no clock), fixed in
//! capacity and allocation-free to build, so a design can run on the audio
//! thread.
//!
//! The values are in frames (fractional, as `f32`: [`Samples`] is whole) and in
//! loop coefficients (bare `f32`: a gain per trip is not a unit the vocabulary
//! has). The queries ([`Network::rt60`], [`tail`](Network::tail),
//! [`is_stable`](Network::is_stable), the densities) read **centre** lengths:
//! modulation moves a line by a few frames around its centre and the queries do
//! not model that.

mod design;
mod gain;
mod inline;
mod queries;

#[cfg(test)]
pub(crate) mod contract;

pub use design::{Bounds, Ceiling, Design};
pub use gain::{loop_gain, rt60_from_gain};
pub use inline::{Full, Inline};
pub use queries::Ring;

use tutti_types::{Db, Hz, SampleRate, Samples, Seconds, StereoWidth};

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
    /// The pre-delay in whole frames: `round(seconds * rate)`, held as `f32`
    /// to share the stages' arithmetic.
    pub pre_delay: f32,
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
            pre_delay: 0.0,
            stages: Inline::new(),
            width: StereoWidth::NATURAL,
            wet: Db(0.0),
        }
    }
}

/// The pre-delay in whole frames, for [`Network::pre_delay`].
#[must_use]
pub fn pre_delay_frames(pre_delay: Seconds, rate: SampleRate) -> f32 {
    pre_delay.to_samples(rate).get() as f32
}

/// One stage of a [`Network`].
// Boxing the large variant would allocate on the audio thread: the size is the
// price of a network built without the heap.
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
    /// The gain, a coefficient.
    pub gain: f32,
    /// The output channel it feeds.
    pub channel: u8,
}

/// A Schroeder all-pass, `(z^-D - g) / (1 - g z^-D)`: the delay `D` in frames
/// and the coefficient `g`, `|g| < 1`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AllPass {
    /// The delay in frames.
    pub delay: f32,
    /// The coefficient.
    pub gain: f32,
    /// How its delay is modulated.
    pub modulation: Modulation,
}

impl AllPass {
    /// The group delay at DC, `D (1 + g) / (1 - g)`, in frames: what the
    /// all-pass adds to a loop's length at low frequency. An all-pass with
    /// `|g| >= 1` has no finite figure: infinity.
    #[must_use]
    pub fn low_frequency_delay(&self) -> f32 {
        if self.gain.abs() < 1.0 {
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

/// One gain per delay line (or per tap).
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
    /// The centre length in frames.
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
            absorption: Absorption::Gain(0.0),
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
    Gain(f32),
    /// A one-pole low-pass `(1 - damp) / (1 - damp z^-1)` (unity at DC) times
    /// `gain`: Freeverb's comb, the plate's damping.
    OnePole {
        /// The gain at DC.
        gain: f32,
        /// The pole.
        damp: f32,
    },
    /// A gain per band.
    Bands {
        /// The low band's gain.
        low: f32,
        /// The middle band's gain.
        mid: f32,
        /// The high band's gain.
        high: f32,
        /// Where the bands meet.
        crossover: [Hz; 2],
    },
}

impl Absorption {
    /// The gain per trip in `band`.
    #[must_use]
    pub fn gain(&self, band: Band) -> f32 {
        match *self {
            Absorption::Gain(g) => g,
            Absorption::OnePole { gain, damp } => {
                let d = f64::from(damp);
                let response = match band {
                    Band::Low => 1.0,
                    Band::Mid => (1.0 - d).abs() / libm::sqrt(1.0 + d * d),
                    Band::High => (1.0 - d).abs() / (1.0 + d).abs(),
                };
                (f64::from(gain) * response) as f32
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
/// stays with the kernel that convolves it.
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

    /// The pre-delay is whole frames, rounded: 1 ms at 48 kHz is 48, more than half a
    /// frame rounds up, and a negative or NaN span is none.
    ///
    /// Mutation (run): `to_samples_floor` for `to_samples` fails the 0.6-frame
    /// row.
    #[test]
    fn the_pre_delay_is_rounded_to_whole_frames() {
        let rate = SampleRate(48_000.0);
        assert_eq!(pre_delay_frames(Seconds(0.001), rate), 48.0);
        assert_eq!(pre_delay_frames(Seconds(0.6 / 48_000.0), rate), 1.0);
        assert_eq!(pre_delay_frames(Seconds(0.4 / 48_000.0), rate), 0.0);
        assert_eq!(pre_delay_frames(Seconds(-1.0), rate), 0.0);
        assert_eq!(pre_delay_frames(Seconds(f32::NAN), rate), 0.0);
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
}
