//! [`SvfDesign`]: the state-variable filter as a [`Design`] (design 024 step 2b).
//!
//! The controls are [`SvfControls`], the model [`SvfModel`] and the realization
//! [`SvfRealization`]: Simper's trapezoidal form, whose coefficients are read from
//! the model's own (`SvfModel::coeffs`) and narrowed once to the state precision
//! `F`. The filter is designed on the 16-frame grid ([`COEFF_CADENCE`]): a node
//! retargets it at each step it solves, and the realization interpolates the
//! coefficients linearly across the next call, arriving exactly on the solved set
//! at its last frame.

use core::marker::PhantomData;

use tutti_types::{ChannelLayout, Db, SampleRate};

use super::real::Real;
use super::svf::{SvfCoefficients, svf_step};
use super::{SvfControls, SvfModel};
use crate::{
    COEFF_CADENCE, Cadence, Ceiling, Channels, Design, Effect, Effort, Realization, Ring, Setup,
    Sizes, lane_group,
};

/// A filter allocates nothing from its controls: its width is the design's.
#[derive(Clone, Copy, Debug)]
pub struct SvfSizes;

impl Sizes for SvfSizes {
    type Source = SvfModel;

    fn measure(_: &SvfModel, _: Db) -> Self {
        SvfSizes
    }

    /// A resonant filter rings for as long as its Q says; the node declares
    /// [`Tail::Unknown`](tutti_types::Tail::Unknown) and never parks.
    fn tail(&self) -> Ring {
        Ring::Unbounded
    }

    fn dominates(&self, _: &Self) -> bool {
        true
    }
}

/// The [`Design`] of the state-variable filter, `channels` wide, with state
/// precision `F`.
///
/// `F` defaults to `f64` in the nodes for accuracy at low cutoffs, where an `f32`
/// integrator loses resolution in the feedback path. The coefficients are shared
/// across the channels and only the integrator pair is per channel.
#[derive(Clone, Copy, Debug)]
pub struct SvfDesign<F> {
    channels: ChannelLayout,
    state: PhantomData<fn() -> F>,
}

impl<F> SvfDesign<F> {
    /// A design `channels` wide.
    #[must_use]
    pub fn new(channels: ChannelLayout) -> Self {
        Self {
            channels,
            state: PhantomData,
        }
    }
}

impl<F: Real> Design for SvfDesign<F> {
    type Controls = SvfControls;
    type Model = SvfModel;
    type Bounds = SvfSizes;
    type Range = ();
    type Realization = SvfRealization<F>;

    /// Solved every 16 frames from the start of each block, so a render depends on
    /// how audio is split.
    const CADENCE: Cadence = COEFF_CADENCE;

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

    fn bounds(&self, _: &Ceiling<()>) -> SvfSizes {
        SvfSizes
    }

    fn realization(&self) -> SvfRealization<F> {
        let rest = SvfModel::new(SvfControls::DEFAULT, SampleRate::DEFAULT);
        let c = SvfCoefficients::from(rest.coeffs());
        SvfRealization {
            prev: c,
            next: c,
            state: vec![[F::default(); 2]; usize::from(self.channels.count()).max(1)],
            landed: false,
        }
    }
}

/// The realization of an [`SvfModel`]: the integrator pair of each channel and the
/// coefficients it runs, interpolated from where the last call ended to the model
/// it was last retargeted to.
#[derive(Clone, Debug)]
pub struct SvfRealization<F: Real> {
    /// The set the last call ended at.
    prev: SvfCoefficients<F>,
    /// The set this call ends at.
    next: SvfCoefficients<F>,
    /// `[ic1eq, ic2eq]`, one pair per channel; `len()` is the audio width. Built
    /// with the design and never resized (no allocation on the audio thread).
    state: Vec<[F; 2]>,
    /// Whether a model has been retargeted since [`prepare`](Realization::prepare):
    /// the first lands whole, with nothing to interpolate from.
    landed: bool,
}

impl<F: Real> SvfRealization<F> {
    /// The channels `first..first + N` through the held set, sample-inner, the
    /// group's integrators in registers. A recursive filter is latency-bound on one
    /// channel (each sample waits for the last), so running a few channels' independent
    /// chains side by side fills that latency while each still reads and writes its
    /// own planar slice. Measured on the f64 SVF at width 6, one block: all six as
    /// one group 0.49 us, as 3 + 3 0.60 us, one channel at a time 1.33 us.
    #[inline]
    fn held_group<const N: usize>(
        &mut self,
        first: usize,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
    ) {
        let c = self.next;
        let xs: [&[f32]; N] = core::array::from_fn(|k| input[first + k]);
        let mut group = output.iter_mut().skip(first);
        let ys: [&mut [f32]; N] =
            core::array::from_fn(|_| &mut **group.next().expect("the group is inside the width"));
        let mut st: [[F; 2]; N] = core::array::from_fn(|k| self.state[first + k]);
        for i in 0..xs[0].len() {
            for k in 0..N {
                ys[k][i] = svf_step(&c, &mut st[k], F::from_f32(xs[k][i])).to_f32();
            }
        }
        self.state[first..first + N].copy_from_slice(&st);
    }
}

impl<F: Real> Realization<SvfModel> for SvfRealization<F> {
    type Bounds = SvfSizes;

    fn prepare(&mut self, _: &SvfSizes, _: &Setup) {
        self.landed = false;
    }

    fn retarget(&mut self, model: &SvfModel) {
        self.next = SvfCoefficients::from(model.coeffs());
        if !self.landed {
            self.prev = self.next;
            self.landed = true;
        }
    }

    fn reset(&mut self) {
        self.state.fill([F::default(); 2]);
        self.prev = self.next;
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }

    fn fresh(&self) -> Self {
        let mut fresh = self.clone();
        fresh.reset();
        fresh
    }
}

impl<F: Real> Effect<SvfModel> for SvfRealization<F> {
    /// Renders with the set held when nothing moved, else with the set blended
    /// linearly from where the last call ended, arriving **exactly** on the new one
    /// at the call's last frame (a call of one frame is the solve it replaces).
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let width = self.state.len();
        if self.prev == self.next {
            let mut first = 0;
            while first < width {
                let n = lane_group(width - first);
                match n {
                    8 => self.held_group::<8>(first, input, output),
                    7 => self.held_group::<7>(first, input, output),
                    6 => self.held_group::<6>(first, input, output),
                    5 => self.held_group::<5>(first, input, output),
                    4 => self.held_group::<4>(first, input, output),
                    3 => self.held_group::<3>(first, input, output),
                    2 => self.held_group::<2>(first, input, output),
                    _ => self.held_group::<1>(first, input, output),
                }
                first += n;
            }
        } else {
            let len = input[0].len();
            let n = F::from_f64(len as f64);
            for i in 0..len {
                let k = if i + 1 == len {
                    self.next
                } else {
                    self.prev.lerp(&self.next, F::from_f64((i + 1) as f64) / n)
                };
                for (ch, s) in self.state.iter_mut().enumerate() {
                    output[ch][i] = svf_step(&k, s, F::from_f32(input[ch][i])).to_f32();
                }
            }
        }
        self.prev = self.next;
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tutti_types::{Hz, Q};

    use super::*;
    use crate::SvfType;
    use crate::contract::{Anchor, DesignCase, LtiCase, RealizationCase};

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

    // Solved from the start of each block, so the render depends on how audio is
    // split and the contract does not name `BlockIndependent` (the `cadence` case
    // fails if either side is wrong).
    crate::realization_contract!(design(), Lti);

    /// The dead-band: a control moves the design when it passes its epsilon or the
    /// response changes, and not below it.
    ///
    /// Mutation (run): an epsilon of zero fails the sub-band rows; dropping the `ty`
    /// comparison fails the last row.
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
    ///
    /// Mutation (run): the blend at `i / n` for every frame, the last included, fails
    /// (`i + 1 == len` alone is not caught: `lerp` at 1 is the set to the bit).
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
