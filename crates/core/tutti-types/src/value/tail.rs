//! [`Tail`] — how long a node keeps producing after its input stops.

use crate::value::{SampleRate, Samples, Seconds};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// How long a node keeps producing audio after its input goes silent.
///
/// **A tail excludes latency**: it is how long the node keeps sounding after
/// its latency is compensated. A lookahead that only delays has no tail; a
/// convolver with an `L`-tap response has `L - 1` whatever its block latency.
/// Export trims the latency and extends the render by the tail, and the
/// executor's silence skip waits for latency plus tail.
///
/// A reverb with a 4-second decay rings for 4 seconds past its last input; a
/// gain stage stops immediately. A bounce that stops rendering when the last
/// clip ends truncates the first mid-decay, so the render has to keep pulling
/// for the tail.
///
/// # Ring-out, not response length
///
/// The figure is the frames produced **after** the input stops, which for an
/// impulse response of length `L` is `L - 1`, not `L`. The distinction is not
/// pedantry — it is what makes the quantity composable. Cascading two nodes
/// convolves their responses, so their *lengths* combine as `La + Lb - 1`,
/// while their ring-outs combine as plain `(La - 1) + (Lb - 1)`. Defined this
/// way a chain is a sum and a merge is a max, with no correction term, which is
/// why [`tail`](crate::tail) can add with [`Samples`]' own operator.
///
/// It is also the question a render actually asks — "how much longer must I
/// pull?" — so a node that passes its input through unchanged answers zero
/// rather than one.
///
/// # A sum type rather than a number, because three answers are not one
///
/// Hosted plugins are where the disagreement is sharpest, and each format's is
/// real:
///
/// - **AU** reports `kAudioUnitProperty_TailTime` in *seconds*, and rejects the
///   property outright on units that have no tail concept — every Apple
///   instrument, mixer and generator does (measured, macOS 15.6).
/// - **CLAP** reports `clap_plugin_tail.get` in *samples*, where any value at
///   or above `INT32_MAX` means infinite (`clap/ext/tail.h`).
/// - **VST3** exposes `getTailSamples`, also in samples, where `0` means no
///   tail and only `kInfiniteTail` (`u32::MAX`) means infinite.
/// - **VST2** answers `effGetTailSize` inverted: `0` is "no information" and
///   `1` is "no tail" (decoded by `tutti-vst2-host`).
///
/// The two sample-based formats disagree on the infinite sentinel, so each has
/// its own decode: [`from_clap_samples`](Self::from_clap_samples) and
/// [`from_vst3_samples`](Self::from_vst3_samples). A CLAP plugin answering
/// `INT32_MAX` through the VST3 decode would read as a 12-hour finite tail,
/// and a bounce would try to render it. AU's seconds go through
/// [`from_seconds`](Self::from_seconds). `clap-sys` and the VST3 bindings bind
/// no named constant for either sentinel, so the decodes name them once here
/// rather than each loader spelling the literal.
///
/// All three report latency through a separate query, and none folds it into
/// the tail (VST3's own example: a 2-second reverb answers 2 seconds), so a
/// plugin's figure already excludes latency and is taken as reported.
///
/// [`Unbounded`](Self::Unbounded) exists because a real plugin uses it and a
/// number cannot carry it. TAL Reverb 4 answers `f64::INFINITY` for its tail
/// (measured; no Apple unit exceeds ~21 s), and
/// [`Seconds::to_samples`](crate::Seconds::to_samples) maps every non-finite
/// input to `Samples::ZERO` — deliberately, since that is the right answer for
/// NaN and negatives. The consequence is that an *infinite* tail arrives
/// bit-identical to a *no* tail, and a bounce sizing its render from that number
/// truncates the reverb completely. The two want opposite handling: unbounded
/// wants a caller-chosen fade, none wants nothing.
///
/// [`Unknown`](Self::Unknown) is not [`None`](Self::None). A node that was never
/// asked, or whose format has no tail query, has said nothing about its tail —
/// reporting that as "no tail" would invent an answer it never gave. It is
/// also the default for every
/// graph node that has not been taught to answer, which is why a graph's figure
/// carries an unknown *count* rather than collapsing to one word.
///
/// # Examples
///
/// ```
/// use tutti_types::{Samples, Tail};
///
/// let delay = Tail::Finite(Samples(24_000));
/// let reverb = Tail::Finite(Samples(96_000));
/// assert_eq!(delay.then(reverb), Tail::Finite(Samples(120_000))); // cascade
/// assert_eq!(delay.beside(reverb), reverb); // parallel
/// assert_eq!(Tail::from_vst3_samples(u32::MAX), Tail::Unbounded);
/// assert_eq!(Tail::from_clap_samples(i32::MAX as u32), Tail::Unbounded);
/// assert_eq!(Tail::Unknown.samples(), None);
/// ```
// `Hash` and not `Ord`: hashing needs only that equal values hash equally, which
// the derive gives, whereas an ordering would have to rank `Unknown` against
// `Unbounded` — two answers that are not points on a line. `graph::NodeSpec`
// carries a `Tail` and is `Eq + Hash` so two graphs can be compared; it is
// deliberately not `Ord` for the same reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Tail {
    /// The format has no tail query, or this loader did not ask.
    #[default]
    Unknown,
    /// The node declared it produces nothing after its input stops.
    None,
    /// A bounded tail, in samples at the rate the node was configured with.
    Finite(Samples),
    /// The node declared an unbounded tail — it never decays to silence on its
    /// own. A bounce must choose where to stop; it cannot ask the node.
    Unbounded,
}

impl Tail {
    /// Returns the tail as a sample count a render can add, or `None` when
    /// there is no finite answer.
    ///
    /// [`Unknown`](Self::Unknown) and [`Unbounded`](Self::Unbounded) both yield
    /// `None`, for opposite reasons — one has no information, the other has
    /// information that is not a number. A caller that wants to treat either as
    /// zero says so with `unwrap_or(Samples::ZERO)` and is seen to have decided.
    pub const fn samples(self) -> Option<Samples> {
        match self {
            Self::None => Some(Samples::ZERO),
            Self::Finite(s) => Some(s),
            Self::Unknown | Self::Unbounded => None,
        }
    }

    /// Decodes VST3's `IAudioProcessor::getTailSamples`: `0` is
    /// [`None`](Self::None) and `kInfiniteTail` (`u32::MAX`) is
    /// [`Unbounded`](Self::Unbounded). Every other count is finite, including
    /// those at or above `INT32_MAX`, which only CLAP reads as infinite.
    pub fn from_vst3_samples(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            u32::MAX => Self::Unbounded,
            n => Self::Finite(Samples(n as usize)),
        }
    }

    /// Decodes CLAP's `clap_plugin_tail.get`: `0` is [`None`](Self::None),
    /// and any value at or above `INT32_MAX` is [`Unbounded`](Self::Unbounded)
    /// (`clap/ext/tail.h`: "any value greater or equal to INT32_MAX implies
    /// infinite tail").
    pub fn from_clap_samples(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            n if n >= i32::MAX as u32 => Self::Unbounded,
            n => Self::Finite(Samples(n as usize)),
        }
    }

    /// Decodes a tail given in seconds (AU's `kAudioUnitProperty_TailTime`)
    /// at `sample_rate`, rounding up so a render does not clip its last
    /// partial frame.
    ///
    /// `+∞` is [`Unbounded`](Self::Unbounded) (TAL Reverb 4 answers it); a
    /// span that rounds to zero frames is [`None`](Self::None). NaN, a
    /// negative span and `-∞` are not a tail at all, so they are
    /// [`Unknown`](Self::Unknown): read as "unbounded" (the `!is_finite`
    /// test alone) a bounce would ring out forever, and read as zero it would
    /// truncate.
    pub fn from_seconds(seconds: Seconds, sample_rate: impl Into<SampleRate>) -> Self {
        let s = seconds.get();
        if s.is_nan() || s < 0.0 {
            return Self::Unknown;
        }
        if s.is_infinite() {
            return Self::Unbounded;
        }
        match seconds.to_samples_ceil(sample_rate) {
            n if n.is_zero() => Self::None,
            n => Self::Finite(n),
        }
    }

    /// Returns the tail of `self` feeding into `next`: a cascade, so the two
    /// add.
    ///
    /// Cascading convolves the two responses, and ring-out is defined so that
    /// supports add with no correction term. `Unbounded` wins over everything —
    /// a chain containing something that never decays never decays — and
    /// `Unknown` wins over the rest, since a node that did not report is not
    /// evidence of a short tail.
    pub fn then(self, next: Self) -> Self {
        match (self, next) {
            (Self::Unbounded, _) | (_, Self::Unbounded) => Self::Unbounded,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (a, b) => {
                // Both are `None` or `Finite`, so both have a count.
                let total =
                    a.samples().unwrap_or(Samples::ZERO) + b.samples().unwrap_or(Samples::ZERO);
                if total.is_zero() {
                    Self::None
                } else {
                    Self::Finite(total)
                }
            }
        }
    }

    /// Returns the tail of two nodes running side by side: the longer of the
    /// two.
    ///
    /// Summing two paths leaves the longer one's support untouched, so a merge
    /// takes the max rather than the sum: the question is when the signal last
    /// leaves.
    pub fn beside(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded, _) | (_, Self::Unbounded) => Self::Unbounded,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (a, b) => {
                let longest = a
                    .samples()
                    .unwrap_or(Samples::ZERO)
                    .max(b.samples().unwrap_or(Samples::ZERO));
                if longest.is_zero() {
                    Self::None
                } else {
                    Self::Finite(longest)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unbounded tail must stay distinguishable from no tail at all.
    ///
    /// This is the whole reason [`Tail`] is a sum type. TAL Reverb 4 answers
    /// `f64::INFINITY` for `kAudioUnitProperty_TailTime`, and
    /// `Seconds::to_samples` maps every non-finite input to `Samples::ZERO` —
    /// correct for NaN and negatives, exactly backwards for `+∞`. Carried as a
    /// number, "infinite reverb" and "no tail" arrive bit-identical, and a
    /// bounce that sizes its render from that number truncates the reverb
    /// completely.
    #[test]
    fn unbounded_is_not_none() {
        assert_ne!(Tail::Unbounded, Tail::None);

        // `samples()` refuses to answer for both `Unbounded` and `Unknown`, so
        // a caller cannot accidentally read either as zero.
        assert_eq!(Tail::None.samples(), Some(Samples::ZERO));
        assert_eq!(Tail::Unbounded.samples(), None);
        assert_eq!(Tail::Unknown.samples(), None);
        assert_eq!(Tail::Finite(Samples(512)).samples(), Some(Samples(512)));
    }

    /// VST3's `kInfiniteTail` (`u32::MAX`) decodes to `Unbounded` and zero to
    /// `None`; a count at `INT32_MAX`, CLAP's sentinel, stays finite.
    #[test]
    fn from_vst3_samples_decodes_the_vst3_sentinel() {
        assert_eq!(Tail::from_vst3_samples(0), Tail::None);
        assert_eq!(Tail::from_vst3_samples(u32::MAX), Tail::Unbounded);
        assert_eq!(
            Tail::from_vst3_samples(44_100),
            Tail::Finite(Samples(44_100))
        );
        let int32_max = i32::MAX as u32;
        assert_eq!(
            Tail::from_vst3_samples(int32_max),
            Tail::Finite(Samples(int32_max as usize))
        );
    }

    /// CLAP reads every value at or above `INT32_MAX` as infinite, so
    /// `INT32_MAX` itself and everything up to `u32::MAX` decode to
    /// `Unbounded`, while one below stays finite.
    ///
    /// Mutation (run): `n if n == u32::MAX` as the unbounded arm (the shared
    /// decode this replaced) → `INT32_MAX` reads as a finite 12-hour tail
    /// → fails; `n > i32::MAX as u32` → `INT32_MAX` itself is finite → fails.
    #[test]
    fn from_clap_samples_reads_int32_max_and_above_as_infinite() {
        let int32_max = i32::MAX as u32;
        assert_eq!(Tail::from_clap_samples(0), Tail::None);
        assert_eq!(Tail::from_clap_samples(int32_max), Tail::Unbounded);
        assert_eq!(Tail::from_clap_samples(int32_max + 1), Tail::Unbounded);
        assert_eq!(Tail::from_clap_samples(u32::MAX), Tail::Unbounded);
        assert_eq!(
            Tail::from_clap_samples(int32_max - 1),
            Tail::Finite(Samples(int32_max as usize - 1))
        );
        assert_eq!(
            Tail::from_clap_samples(44_100),
            Tail::Finite(Samples(44_100))
        );
    }

    /// Seconds: `+∞` is unbounded, zero is none, a finite span rounds up, and
    /// NaN, negatives and `-∞` are unknown rather than unbounded or none.
    ///
    /// Mutation (run): the old AU decode (`!is_finite()` → `Unbounded`, then
    /// `to_samples_ceil`) → NaN and `-∞` read as `Unbounded` and a negative
    /// span as `None` → fails.
    #[test]
    fn from_seconds_reads_only_plus_infinity_as_unbounded() {
        let rate = SampleRate(48_000.0);
        assert_eq!(
            Tail::from_seconds(Seconds(f32::INFINITY), rate),
            Tail::Unbounded
        );
        assert_eq!(Tail::from_seconds(Seconds(0.0), rate), Tail::None);
        assert_eq!(
            Tail::from_seconds(Seconds(0.5), rate),
            Tail::Finite(Samples(24_000))
        );
        // A part of a frame still needs that frame.
        assert_eq!(
            Tail::from_seconds(Seconds(1.0 / 96_000.0), rate),
            Tail::Finite(Samples(1))
        );
        for junk in [f32::NAN, f32::NEG_INFINITY, -1.0] {
            assert_eq!(
                Tail::from_seconds(Seconds(junk), rate),
                Tail::Unknown,
                "{junk}"
            );
        }
    }

    /// A node that has said nothing is not a node that said "no tail".
    #[test]
    fn unknown_is_not_none() {
        assert_ne!(Tail::Unknown, Tail::None);
        assert_eq!(Tail::default(), Tail::Unknown);
    }

    /// A cascade adds and a merge takes the max — the two rules that let a
    /// statically-composed graph report without anyone walking it.
    #[test]
    fn cascade_adds_and_merge_takes_the_longer() {
        let a = Tail::Finite(Samples(2000));
        let b = Tail::Finite(Samples(3000));
        assert_eq!(a.then(b), Tail::Finite(Samples(5000)));
        assert_eq!(a.beside(b), Tail::Finite(Samples(3000)));
    }

    /// A merge keeps the longer leg, not the shorter: tail asks when a signal
    /// last leaves, not when it first arrives.
    #[test]
    fn a_merge_is_not_the_shorter_leg() {
        let slow = Tail::Finite(Samples(3000));
        let fast = Tail::Finite(Samples(100));
        assert_eq!(slow.beside(fast), Tail::Finite(Samples(3000)));
        assert_ne!(slow.beside(fast), fast);
    }

    /// Nothing that never decays can be composed away.
    #[test]
    fn unbounded_survives_composition() {
        let n = Tail::Finite(Samples(100));
        assert_eq!(Tail::Unbounded.then(n), Tail::Unbounded);
        assert_eq!(n.then(Tail::Unbounded), Tail::Unbounded);
        assert_eq!(Tail::Unbounded.beside(n), Tail::Unbounded);
        assert_eq!(n.beside(Tail::Unbounded), Tail::Unbounded);
    }

    /// An unreported node poisons a composition rather than counting as zero:
    /// it is not evidence of a short tail.
    #[test]
    fn unknown_survives_composition() {
        let n = Tail::Finite(Samples(100));
        assert_eq!(Tail::Unknown.then(n), Tail::Unknown);
        assert_eq!(n.beside(Tail::Unknown), Tail::Unknown);
        // But `Unbounded` still outranks it — that much IS known.
        assert_eq!(Tail::Unknown.then(Tail::Unbounded), Tail::Unbounded);
    }

    /// Composing silence with silence stays silence, rather than becoming a
    /// zero-length `Finite`.
    #[test]
    fn composing_no_tails_stays_none() {
        assert_eq!(Tail::None.then(Tail::None), Tail::None);
        assert_eq!(Tail::None.beside(Tail::None), Tail::None);
    }
}
