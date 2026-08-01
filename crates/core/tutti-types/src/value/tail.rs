//! [`Tail`] — how long a node keeps producing after its input stops.

use crate::value::Samples;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// How long a node keeps producing audio after its input goes silent.
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
/// - **CLAP** reports `clap_plugin_tail.get` in *samples*.
/// - **VST3** exposes `getTailSamples`, also in samples, where `0` means no
///   tail.
///
/// Both sample-based formats saturate at `u32::MAX`, which their specs read as
/// an effectively unbounded tail; `clap-sys` does not bind a named constant for
/// it, so [`from_samples`](Self::from_samples) names the sentinel once here
/// rather than each loader spelling the literal.
/// - **VST2** has no tail concept the vendored bindings surface.
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
/// reporting that as "no tail" is the same class of invention the `probed` mask
/// exists to prevent for plugin capabilities. It is also the default for every
/// graph node that has not been taught to answer, which is why a graph's figure
/// carries an unknown *count* rather than collapsing to one word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    /// The tail as a sample count a render can add, or `None` when there is no
    /// finite answer.
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

    /// Build from a format's raw sample count, mapping the `u32::MAX` sentinel
    /// CLAP and VST3 both use for "unbounded".
    ///
    /// The sentinel is the formats' own, so decoding it belongs here rather
    /// than being repeated at each loader.
    pub fn from_samples(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            u32::MAX => Self::Unbounded,
            n => Self::Finite(Samples(n as usize)),
        }
    }

    /// The tail of `self` feeding into `next`: a cascade, so the two add.
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

    /// The tail of two nodes running side by side: the longer of the two.
    ///
    /// Summing two paths leaves the longer one's support untouched, so a merge
    /// takes the max rather than the sum. This is where tail and latency
    /// genuinely differ — latency takes the *minimum* across a merge, because it
    /// asks when a signal first arrives where this asks when it last leaves.
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

    /// The `u32::MAX` sentinel CLAP and VST3 share decodes to `Unbounded`, and
    /// zero to `None` — the two ends a raw count cannot tell apart.
    #[test]
    fn from_samples_decodes_the_format_sentinel() {
        assert_eq!(Tail::from_samples(0), Tail::None);
        assert_eq!(Tail::from_samples(u32::MAX), Tail::Unbounded);
        assert_eq!(Tail::from_samples(44_100), Tail::Finite(Samples(44_100)));
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

    /// The merge rule is where tail and latency part company.
    ///
    /// fundsp's latency takes the *minimum* across a merge, because it asks when
    /// a signal first arrives. Tail asks when it last leaves, so it must take
    /// the maximum — which is why tail cannot ride the `Signal` carrier latency
    /// already uses.
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
