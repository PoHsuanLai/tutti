//! The shape of a node swap on the `Net` path: how `Net::crossfade` blends
//! the outgoing unit into the incoming one.
//!
//! The curve itself is `tutti-graph`'s [`CrossfadeCurve`], re-exported at
//! this crate's root: the native graph's `Editor::replace` follows it too, so
//! it lives with the graph (doc 013, Phase 3 PR 3). `Net::crossfade` still
//! takes the fork's `sequencer::Fade`, and the orphan rule forbids a `From`
//! impl between two foreign types here, so the conversion is [`net_fade`].
//! It goes with `Net` (doc 013, Phase 5).

use tutti_graph::CrossfadeCurve;

/// The fork's fade for `curve`, for `Net::crossfade` — the only thing that
/// still takes it. A caller writes `net_fade(curve)` without naming the fork.
///
/// The fork's two laws are the graph's two: equal power (a sine pair) and
/// equal amplitude (fundsp's `smooth5`, the polynomial
/// [`CrossfadeCurve::gains`] uses).
pub fn net_fade(curve: CrossfadeCurve) -> fundsp::sequencer::Fade {
    match curve {
        CrossfadeCurve::EqualPower => fundsp::sequencer::Fade::Power,
        CrossfadeCurve::EqualAmplitude => fundsp::sequencer::Fade::Smooth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each curve lands on the fork variant with the matching law.
    ///
    /// Read through `Fade::at`, since the fork's enum has no `PartialEq`: the
    /// complementary halves sum to one in amplitude for
    /// `EqualAmplitude` and in power for `EqualPower` — which is the property
    /// each name promises. (The fork's equal-power law is Bhaskara's sine
    /// approximation, so its power sum is one to within ~0.2%, hence the looser
    /// tolerance there; a swapped law misses by 20% or more.)
    ///
    /// Mutation: swapping the two arms of `net_fade` fails both sums.
    #[test]
    fn each_curve_maps_to_the_law_its_name_promises() {
        let rise = |c: CrossfadeCurve, x: f32| net_fade(c).at(x);
        for x in [0.1f32, 0.25, 0.4] {
            let (a, b) = (
                rise(CrossfadeCurve::EqualAmplitude, x),
                rise(CrossfadeCurve::EqualAmplitude, 1.0 - x),
            );
            assert!(
                (a + b - 1.0).abs() < 1e-5,
                "amplitude sum at {x}: {}",
                a + b
            );
            let (p, q) = (
                rise(CrossfadeCurve::EqualPower, x),
                rise(CrossfadeCurve::EqualPower, 1.0 - x),
            );
            assert!(
                (p * p + q * q - 1.0).abs() < 5e-3,
                "power sum at {x}: {}",
                p * p + q * q
            );
        }
        assert_eq!(CrossfadeCurve::default(), CrossfadeCurve::EqualAmplitude);
    }

    /// The graph's equal-amplitude law is the fork's `smooth5`, so a swap
    /// moved from `Net::crossfade` to `Editor::replace` sounds the same.
    /// Frame `k` of an `n`-frame graph fade sits at `x = (k + 1) / (n + 1)`.
    ///
    /// Mutation: change the polynomial in `CrossfadeCurve::gains` → fails.
    #[test]
    fn the_graph_and_the_fork_share_the_equal_amplitude_law() {
        let n = 9;
        for k in 0..n {
            let x = (k + 1) as f32 / (n + 1) as f32;
            let (g_in, _) = CrossfadeCurve::EqualAmplitude.gains(k, n);
            let fork = net_fade(CrossfadeCurve::EqualAmplitude).at(x);
            assert!((g_in - fork).abs() < 1e-6, "frame {k}: {g_in} vs {fork}");
        }
    }
}
