//! The shape of a node swap: how `Net::crossfade` blends the outgoing unit
//! into the incoming one.

/// The gain curve a graph crossfade follows.
///
/// This is the engine's name for fundsp's `sequencer::Fade`, which used to be
/// re-exported at the crate root and through both umbrella preludes — where it
/// sat beside `tutti-sampler`'s private `Fade` struct (the butler's loop and
/// seek crossfader), two unrelated things under one name. `Net::crossfade`
/// still takes the fork's type, so the conversion is the `From` impl below and
/// a caller writes `curve.into()` without ever naming the fork.
///
/// Which to pick is a property of the two signals, not of taste:
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossfadeCurve {
    /// Equal **power**: a sine/cosine pair, so the summed power is constant
    /// across the fade. Right for signals with independent phase — two
    /// different sources — which add in power.
    EqualPower,
    /// Equal **amplitude**: a smooth (fifth-order) polynomial whose two halves
    /// sum to one. Right for phase-coherent signals — the same source before
    /// and after a parameter rebuild — which add in amplitude, so an
    /// equal-power fade would bump the level by up to 3 dB mid-swap.
    #[default]
    EqualAmplitude,
}

impl From<CrossfadeCurve> for fundsp::sequencer::Fade {
    fn from(curve: CrossfadeCurve) -> Self {
        match curve {
            CrossfadeCurve::EqualPower => Self::Power,
            CrossfadeCurve::EqualAmplitude => Self::Smooth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fundsp::sequencer::Fade;

    /// Each curve lands on the fork variant with the matching law.
    ///
    /// Read through `Fade::at`, since the fork's enum has no `PartialEq`: the
    /// complementary halves sum to one in amplitude for
    /// `EqualAmplitude` and in power for `EqualPower` — which is the property
    /// each name promises. (The fork's equal-power law is Bhaskara's sine
    /// approximation, so its power sum is one to within ~0.2%, hence the looser
    /// tolerance there; a swapped law misses by 20% or more.)
    ///
    /// Mutation: swapping the two arms of the `From` impl fails both sums.
    #[test]
    fn each_curve_maps_to_the_law_its_name_promises() {
        let rise = |c: CrossfadeCurve, x: f32| Fade::from(c).at(x);
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
}
