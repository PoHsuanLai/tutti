use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{Drive, Hz, Resonance};

use super::*;
use crate::contract::{Anchor, DesignCase, LtiCase, RealizationCase};
use crate::{Design, Effect, Realization};

const TYPES: [LadderType; 4] = [
    LadderType::LP12,
    LadderType::LP24,
    LadderType::HP12,
    LadderType::HP24,
];

fn controls(ty: LadderType, freq: f32, res: f32, drive: f32) -> LadderControls {
    LadderControls {
        ty,
        freq: Hz(freq),
        res: Resonance(res),
        drive: Drive(drive),
    }
}

fn design() -> LadderDesign<f64> {
    LadderDesign::new(BusTopology::MONO)
}

// The reachable controls stay where the small-signal model is stable: the
// linearisation of the trapezoidal loop has a pole outside the unit circle at a
// high cutoff with a middling resonance (15 kHz at 44.1 kHz, resonance 0.5: radius
// 1.19; 6 kHz at resonance 0.7: 1.009), and past a resonance of about 1 everywhere, where the saturator is what
// holds the real filter (`ladder_model.rs`).
impl DesignCase for LadderDesign<f64> {
    fn reachable(&self) -> Vec<LadderControls> {
        let mut all = Vec::new();
        for ty in TYPES {
            for freq in [30.0, 400.0, 1_500.0, 3_000.0] {
                for res in [0.0, 0.3, 0.5, 0.7] {
                    for drive in [0.1, 1.0, 6.0] {
                        all.push(controls(ty, freq, res, drive));
                    }
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(24);
        all.extend((0..200).map(|_| {
            controls(
                TYPES[rng.gen_range(0..TYPES.len())],
                rng.gen_range(20.0..3_000.0),
                rng.gen_range(0.0..0.7),
                rng.gen_range(0.1..8.0),
            )
        }));
        all
    }

    fn hostile(&self) -> Vec<LadderControls> {
        let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 1e30];
        // The model is held to stability, and a clamp can land on what it calls
        // unstable: a resonance past 1 clamps to 1 (the saturator is what holds
        // the real filter), a cutoff past Nyquist clamps to it, where any
        // resonance is. A cutoff clamped to the 1 Hz floor puts the four poles
        // within 1e-4 of 1, closer than the root finder places them, so it is not
        // tried; the cutoff that clamps to
        // Nyquist is tried with no resonance.
        let mut all: Vec<_> = nasty
            .iter()
            .map(|&v| controls(LadderType::LP24, 1_000.0, 0.5, v))
            .collect();
        all.extend(
            [f32::NAN, f32::INFINITY, f32::NEG_INFINITY]
                .map(|v| controls(LadderType::HP12, v, 0.2, 1.0)),
        );
        all.push(controls(LadderType::HP12, 1e30, 0.0, 1.0));
        all.extend(
            [f32::NAN, f32::NEG_INFINITY, -3.0]
                .iter()
                .map(|&v| controls(LadderType::LP24, 1_000.0, v, 1.0)),
        );
        all
    }
}

/// The closed forms of the linearisation (`ladder_model.rs`): DC is
/// `drive / (1 + 4 res)` for a low-pass and 0 for a high-pass; the stages are
/// `1/sqrt(2)` at the cutoff, so with no resonance a 12 dB type is 1/2 there and
/// a 24 dB type 1/4.
impl LtiCase for LadderDesign<f64> {
    fn anchors(&self) -> Vec<Anchor<LadderControls>> {
        let rate = SampleRate(48_000.0);
        let at = |c: LadderControls, freq: f32, gain: f64| Anchor {
            controls: c,
            rate,
            output: 0,
            freq: Hz(freq),
            gain,
            tolerance: 1e-9,
        };
        let mut all = Vec::new();
        for res in [0.0f32, 0.5, 0.9] {
            let k = 4.0 * f64::from(res);
            all.push(at(
                controls(LadderType::LP24, 1_000.0, res, 1.0),
                0.0,
                1.0 / (1.0 + k),
            ));
            all.push(at(
                controls(LadderType::LP12, 1_000.0, res, 2.0),
                0.0,
                2.0 / (1.0 + k),
            ));
            all.push(at(controls(LadderType::HP12, 1_000.0, res, 1.0), 0.0, 0.0));
        }
        for (ty, want) in [
            (LadderType::LP12, 0.5),
            (LadderType::LP24, 0.25),
            (LadderType::HP12, 0.5),
            (LadderType::HP24, 0.25),
        ] {
            all.push(at(controls(ty, 1_000.0, 0.0, 1.0), 1_000.0, want));
        }
        all
    }
}

impl RealizationCase for LadderDesign<f64> {
    fn probes(&self) -> Vec<LadderControls> {
        vec![
            controls(LadderType::LP24, 1_000.0, 0.3, 1.0),
            controls(LadderType::LP12, 3_000.0, 0.6, 1.0),
            controls(LadderType::HP24, 500.0, 0.2, 1.0),
            controls(LadderType::HP12, 6_000.0, 0.5, 1.0),
        ]
    }

    fn extremes(&self) -> Vec<LadderControls> {
        vec![
            controls(LadderType::LP24, 200.0, 0.9, 1.0),
            controls(LadderType::HP24, 8_000.0, 0.9, 1.0),
        ]
    }

    fn steps(&self) -> Vec<(LadderControls, LadderControls)> {
        vec![
            (
                controls(LadderType::LP24, 800.0, 0.3, 1.0),
                controls(LadderType::LP24, 1_200.0, 0.3, 1.0),
            ),
            (
                controls(LadderType::LP24, 1_000.0, 0.4, 1.0),
                controls(LadderType::LP24, 1_000.0, 0.4, 3.0),
            ),
        ]
    }
}

crate::design_contract!(design(), Lti);

#[test]
fn a_glides_midpoint_is_stable() {
    super::super::channel::tests::assert_midpoints_decay(&design());
}

// No `Lti` group on the realization: it is the nonlinear ladder (the `tanh` in
// the loop), and that group drives it at unit level and asks for linearity
// (additivity, the impulse response against the model's, a decay measured
// there), which the small-signal model does not claim at that level; the
// model's own response is held against `ladder_step` at small signal in
// `ladder_model.rs`. Solved on the stream's 16-frame grid, so the render does not
// depend on how audio is split: `BlockIndependent`.
crate::realization_contract!(design(), BlockIndependent);

/// A blended call is the linear blend of the two sets, coefficients and drive
/// alike, arriving **exactly** on the new set at its last frame: an impulse at
/// frame `i` into a ladder with no state comes out as one `ladder_step` at the
/// set `(i + 1) / 16` of the way from the old to the new, and at frame 15 as the
/// held render of the new set gives it, bit for bit.
#[test]
fn a_blend_is_linear_and_arrives_on_the_new_set() {
    let d = design();
    let rate = SampleRate(48_000.0);
    let model = |hz, drive| d.design(&controls(LadderType::HP12, hz, 0.3, drive), rate);
    let (old, new) = (model(500.0, 1.0), model(5_000.0, 4.0));
    for i in [7usize, 15] {
        let mut blended = d.realization();
        blended.retarget(&old);
        blended.retarget(&new);
        let mut x = [0.0f32; 16];
        x[i] = 0.5;
        let mut y = [0.0f32; 16];
        blended.process(&[&x], &mut [&mut y]);

        let t = (i + 1) as f64 / 16.0;
        let (a, b) = (LadderPoint::<f64>::of(&old), LadderPoint::<f64>::of(&new));
        let at = a.lerp(&b, (i + 1) as u32, 16);
        // The drive is written out: 1 to 4 linearly.
        let drive = 1.0 + (4.0 - 1.0) * t;
        let want = ladder_step(&at.c, LadderType::HP12, &mut [0.0; 4], 0.5, drive);
        assert_eq!(y[i], want as f32, "frame {i}");
    }
    let mut held = d.realization();
    held.retarget(&new);
    let mut x = [0.0f32; 16];
    x[15] = 0.5;
    let (mut y, mut z) = ([0.0f32; 16], [0.0f32; 16]);
    let mut blended = d.realization();
    blended.retarget(&old);
    blended.retarget(&new);
    blended.process(&[&x], &mut [&mut y]);
    held.process(&[&x], &mut [&mut z]);
    assert_eq!(y[15], z[15]);
    assert_ne!(y[15], 0.0);
}

/// A retarget in the middle of a ramp starts from the set in force there, not
/// from the set the ramp was heading for: 8 frames into a ramp to a cutoff, a
/// retarget back leaves the first frame after it where the ramp was, one 16th of
/// the way from there.
#[test]
fn the_cut_ramp_goes_on_from_where_it_was() {
    let d = design();
    let rate = SampleRate(48_000.0);
    let model = |hz| d.design(&controls(LadderType::HP12, hz, 0.3, 1.0), rate);
    let (a, b) = (model(500.0), model(5_000.0));
    let mut r = d.realization();
    r.retarget(&a);
    r.retarget(&b);
    r.process(&[&[0.0; 8]], &mut [&mut [0.0; 8]]);
    r.retarget(&a);
    let mut x = [0.0f32; 1];
    x[0] = 0.5;
    let mut y = [0.0f32; 1];
    r.process(&[&x], &mut [&mut y]);

    let (pa, pb) = (LadderPoint::<f64>::of(&a), LadderPoint::<f64>::of(&b));
    let from = pa.lerp(&pb, 8, 16);
    let at = from.lerp(&pa, 1, 16);
    let want = ladder_step(&at.c, LadderType::HP12, &mut [0.0; 4], 0.5, at.drive);
    assert_eq!(y[0], want as f32);
}
