//! [`SvfDesign`]: the state-variable filter as a [`Design`](crate::Design) (design 024 step 2b).
//!
//! The controls are [`SvfControls`], the model [`SvfModel`] and the kernel [`Svf`]:
//! Simper's trapezoidal form, whose coefficients are read from the model's own
//! (`SvfModel::coeffs`) and narrowed once to the state precision `F`. The filter is
//! designed on a 16-frame grid counted on the stream clock from the last reset
//! ([`Design::CADENCE`](crate::Design::CADENCE)): a node retargets it at each grid point it solves, and the
//! realization interpolates the coefficients linearly over the 16 frames that follow,
//! arriving exactly on the solved set at the segment's last frame. A control is
//! therefore in the render at most 16 frames after it was sampled, and the render does
//! not depend on how audio is split into blocks.

use core::marker::PhantomData;

use tutti_types::{ChannelLayout, SampleRate};

use super::real::Real;
use super::svf::{SvfCoefficients, svf_step};
use super::{ChannelKernel, Filter, SvfControls, SvfModel};

/// The [`Design`](crate::Design) of the state-variable filter, `channels` wide, with
/// state precision `F`.
///
/// `F` defaults to `f64` in the nodes for accuracy at low cutoffs, where an `f32`
/// integrator loses resolution in the feedback path. The coefficients are shared
/// across the channels and only the integrator pair is per channel.
pub type SvfDesign<F> = Filter<Svf<F>>;

/// The state-variable filter as a [`ChannelKernel`]: `[ic1eq, ic2eq]` per channel, in
/// state precision `F`.
#[derive(Clone, Copy, Debug)]
pub struct Svf<F>(PhantomData<fn() -> F>);

impl<F> SvfDesign<F> {
    /// A design `channels` wide.
    #[must_use]
    pub fn new(channels: ChannelLayout) -> Self {
        Self::over(Svf(PhantomData), channels)
    }
}

impl<F: Real> ChannelKernel for Svf<F> {
    type Controls = SvfControls;
    type Model = SvfModel;
    type Coeffs = SvfCoefficients<F>;
    type State = [F; 2];
    type Frame = [f32; 1];

    const STEPS: u32 = 16;

    fn blank(&self, rate: SampleRate) -> SvfModel {
        SvfModel::new(SvfControls::DEFAULT, rate)
    }

    fn design_into(&self, controls: &SvfControls, rate: SampleRate, out: &mut SvfModel) {
        *out = SvfModel::new(*controls, rate);
    }

    fn coeffs(&self, model: &SvfModel) -> SvfCoefficients<F> {
        SvfCoefficients::from(model.coeffs())
    }

    #[inline(always)]
    fn tick(c: &SvfCoefficients<F>, s: &mut [F; 2], x: f32) -> [f32; 1] {
        [svf_step(c, s, F::from_f32(x)).to_f32()]
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tutti_types::{Db, Hz, Q};

    use super::*;
    use crate::SvfType;
    use crate::contract::{Anchor, DesignCase, LtiCase, RealizationCase};
    use crate::{Design, Effect, Realization};

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

    fn controls(ty: SvfType, freq: f32, q: f32, gain: f32) -> SvfControls {
        SvfControls {
            ty,
            freq: Hz(freq),
            q: Q(q),
            gain: Db(gain),
        }
    }

    fn design() -> SvfDesign<f64> {
        SvfDesign::new(ChannelLayout::MONO)
    }

    impl DesignCase for SvfDesign<f64> {
        fn reachable(&self) -> Vec<SvfControls> {
            let mut all = Vec::new();
            for ty in TYPES {
                for freq in [30.0, 400.0, 3_000.0, 15_000.0] {
                    for q in [0.1, 0.707, 4.0, 60.0] {
                        for gain in [-18.0, 0.0, 18.0] {
                            all.push(controls(ty, freq, q, gain));
                        }
                    }
                }
            }
            let mut rng = StdRng::seed_from_u64(24);
            all.extend((0..200).map(|_| {
                controls(
                    TYPES[rng.gen_range(0..TYPES.len())],
                    rng.gen_range(20.0..18_000.0),
                    rng.gen_range(0.1..30.0),
                    rng.gen_range(-24.0..24.0),
                )
            }));
            all
        }

        fn hostile(&self) -> Vec<SvfControls> {
            let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 1e30];
            nasty
                .iter()
                .flat_map(|&v| {
                    [
                        controls(SvfType::Bell, v, 1.0, 6.0),
                        controls(SvfType::Bell, 1_000.0, v, 6.0),
                        controls(SvfType::Bell, 1_000.0, 1.0, v),
                        controls(SvfType::LowShelf, v, v, v),
                    ]
                })
                .collect()
        }
    }

    /// The closed forms of the response (`1 / (s^2 + k s + 1)` under the bilinear
    /// map, which puts `s = j` at the cutoff exactly): a low-pass and a high-pass
    /// are `Q` there, an all-pass is unity, a bell is its gain, a shelf is its
    /// gain at the far end, and a low-pass at DC is unity.
    impl LtiCase for SvfDesign<f64> {
        fn anchors(&self) -> Vec<Anchor<SvfControls>> {
            let rate = SampleRate(48_000.0);
            let at = |c: SvfControls, freq: f32, gain: f64| Anchor {
                controls: c,
                rate,
                output: 0,
                freq: Hz(freq),
                gain,
                tolerance: 1e-6,
            };
            let a = |db: f32| 10f64.powf(f64::from(db) / 20.0);
            let mut all = Vec::new();
            for q in [0.5f32, 0.707, 3.0] {
                all.push(at(
                    controls(SvfType::LowPass, 1_000.0, q, 0.0),
                    1_000.0,
                    f64::from(q),
                ));
                all.push(at(
                    controls(SvfType::HighPass, 2_500.0, q, 0.0),
                    2_500.0,
                    f64::from(q),
                ));
                all.push(at(controls(SvfType::Allpass, 800.0, q, 0.0), 5_000.0, 1.0));
                all.push(at(controls(SvfType::LowPass, 1_000.0, q, 0.0), 0.0, 1.0));
            }
            for db in [-12.0f32, 6.0, 15.0] {
                all.push(at(
                    controls(SvfType::Bell, 1_000.0, 1.5, db),
                    1_000.0,
                    a(db),
                ));
                all.push(at(
                    controls(SvfType::LowShelf, 1_000.0, 0.707, db),
                    0.0,
                    a(db),
                ));
                all.push(at(
                    controls(SvfType::HighShelf, 1_000.0, 0.707, db),
                    0.0,
                    1.0,
                ));
            }
            all
        }
    }

    impl RealizationCase for SvfDesign<f64> {
        fn probes(&self) -> Vec<SvfControls> {
            vec![
                controls(SvfType::LowPass, 1_000.0, 0.707, 0.0),
                controls(SvfType::BandPass, 3_000.0, 4.0, 0.0),
                // A long ring, for the decay case's windows of 4096 frames.
                controls(SvfType::BandPass, 300.0, 200.0, 0.0),
                controls(SvfType::Bell, 500.0, 1.5, 12.0),
                controls(SvfType::HighShelf, 6_000.0, 0.8, -9.0),
            ]
        }

        fn extremes(&self) -> Vec<SvfControls> {
            vec![
                controls(SvfType::BandPass, 200.0, 40.0, 0.0),
                controls(SvfType::Bell, 8_000.0, 30.0, 24.0),
            ]
        }

        fn steps(&self) -> Vec<(SvfControls, SvfControls)> {
            vec![
                (
                    controls(SvfType::LowPass, 800.0, 0.707, 0.0),
                    controls(SvfType::LowPass, 1_200.0, 0.707, 0.0),
                ),
                (
                    controls(SvfType::Bell, 1_000.0, 2.0, 6.0),
                    controls(SvfType::Bell, 1_000.0, 2.0, -6.0),
                ),
            ]
        }
    }

    crate::design_contract!(design(), Lti);

    #[test]
    fn a_glides_midpoint_is_stable() {
        super::super::channel::tests::assert_midpoints_decay(&design());
    }

    // Solved on the stream's 16-frame grid, so the render does not depend on how
    // audio is split: the contract names `BlockIndependent` (the `cadence` case
    // fails if either side is wrong).
    crate::realization_contract!(design(), Lti, BlockIndependent);

    /// The dead-band: a control moves the design when it passes its epsilon or the
    /// response changes, and not below it.
    #[test]
    fn a_control_moves_the_design_only_past_its_dead_band() {
        let base = controls(SvfType::Bell, 1_000.0, 1.0, 3.0);
        assert!(!controls(SvfType::Bell, 1_000.005, 1.0, 3.0).moved_from(&base));
        assert!(!controls(SvfType::Bell, 1_000.0, 1.00005, 3.0).moved_from(&base));
        assert!(!controls(SvfType::Bell, 1_000.0, 1.0, 3.005).moved_from(&base));
        assert!(controls(SvfType::Bell, 1_000.02, 1.0, 3.0).moved_from(&base));
        assert!(controls(SvfType::Bell, 1_000.0, 1.0002, 3.0).moved_from(&base));
        assert!(controls(SvfType::Bell, 1_000.0, 1.0, 3.02).moved_from(&base));
        assert!(controls(SvfType::Notch, 1_000.0, 1.0, 3.0).moved_from(&base));
    }

    /// A blended call lands exactly on the new set at its last frame: an impulse on
    /// that frame, through a filter with no state, comes out as the held render of
    /// the new set gives it, bit for bit (a low-pass's first output is `a3`).
    #[test]
    fn a_blend_arrives_on_the_new_set_at_its_last_frame() {
        let d = design();
        let rate = SampleRate(48_000.0);
        let model = |hz| d.design(&controls(SvfType::LowPass, hz, 0.707, 0.0), rate);
        let mut blended = d.realization();
        blended.retarget(&model(500.0));
        blended.retarget(&model(5_000.0));
        let mut held = d.realization();
        held.retarget(&model(5_000.0));

        let mut x = [0.0f32; 16];
        x[15] = 1.0;
        let (mut y, mut z) = ([0.0f32; 16], [0.0f32; 16]);
        blended.process(&[&x], &mut [&mut y]);
        held.process(&[&x], &mut [&mut z]);
        assert_eq!(y[15], z[15]);
        assert_ne!(y[15], 0.0);
    }
}
