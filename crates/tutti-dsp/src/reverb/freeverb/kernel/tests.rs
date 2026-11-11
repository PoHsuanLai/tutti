use super::*;

/// Freeverb's all-pass is `H(z) = ((1 + g) z^-D - 1) / (1 - g z^-D)`, the
/// network's `AllPassForm::Freeverb`, not the unit-magnitude lattice. Its
/// impulse response, from the transfer function, is -1 at frame 0 and
/// `g^(k-1)` at frame `k D` for `k >= 1`, and zero elsewhere; its gain is 1
/// at DC for `g = 0.5` and `(2 + g) / (1 + g)` where `D w = pi`.
#[test]
fn the_all_pass_is_the_freeverb_form_not_the_lattice() {
    let (d, g) = (8usize, 0.5f32);
    let mut ap = AllPassFilter::new(d);
    ap.feedback = g;
    let mut h = tutti_fixtures::signal::impulse(tutti_types::Samples(18 * d + 1));
    ap.process(&mut h);
    assert!((h[0] + 1.0).abs() < 1e-6, "h[0] = {}", h[0]);
    for k in 1..=17usize {
        assert!(
            (h[k * d] - g.powi(k as i32 - 1)).abs() < 1e-5,
            "h[{}] = {}",
            k * d,
            h[k * d]
        );
    }
    assert!((0..h.len()).filter(|n| n % d != 0).all(|n| h[n] == 0.0));
    let gain_at = |omega: f64| {
        let (re, im) = h
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(re, im), (n, &x)| {
                let a = omega * n as f64;
                (
                    re + f64::from(x) * libm::cos(a),
                    im - f64::from(x) * libm::sin(a),
                )
            });
        re.hypot(im)
    };
    assert!((gain_at(0.0) - 1.0).abs() < 1e-3, "DC {}", gain_at(0.0));
    let peak = gain_at(std::f64::consts::PI / d as f64);
    assert!(
        (peak - (2.0 + 0.5) / (1.0 + 0.5)).abs() < 1e-3,
        "peak {peak}"
    );
}

/// A room size and a damping that move mid-stream move the combs' feedback and
/// damping over the 20 ms glide, a chunk at a time, and land on the new figures
/// exactly: each chunk's change is the distance over the 30 steps (to within
/// rounding), the first is far from the target, and the coefficient is the
/// target at the last step and not before. (What jumped before: a size step was
/// a step in the comb's loop gain, a click.)
#[test]
fn a_moving_room_glides_its_combs() {
    use crate::{Ceiling, Design, Effect, FreeverbControls, FreeverbDesign, Realization};
    use tutti_types::{MaxBlock, Mix};
    let rate = SampleRate(48_000.0);
    let d = FreeverbDesign;
    let mut r = d.realization();
    r.prepare(
        &d.bounds(&Ceiling::at(rate).unwrap()),
        &Setup {
            rate,
            max_block: MaxBlock::of(tutti_types::Samples(CHUNK)),
        },
    );
    let at = |room: f32, damp: f32| {
        d.design(
            &FreeverbControls {
                room: Mix(room),
                damp: Mix(damp),
            },
            rate,
        )
    };
    let (x, mut l, mut rr) = (
        vec![0.25f32; CHUNK],
        vec![0.0f32; CHUNK],
        vec![0.0f32; CHUNK],
    );
    let mut chunk = |r: &mut crate::Realized<FreeverbKernel>| {
        r.process(&[&x], &mut [&mut l, &mut rr]);
        let (_, c, _) = r.parts_mut();
        (c.feedback.now(), c.damp.now())
    };
    r.retarget(&at(0.2, 0.1));
    let (from, from_damp) = chunk(&mut r);
    r.retarget(&at(0.9, 0.9));
    let (to, to_damp) = {
        let (_, c, _) = r.parts_mut();
        (c.feedback.target(), c.damp.target())
    };
    let steps = 30; // 20 ms at 48 kHz in chunks of 32 frames.
    let mut last = from;
    for k in 1..=steps {
        let (f, dmp) = chunk(&mut r);
        assert!(
            ((f - last) - (to - from) / steps as f32).abs() < 1e-5,
            "step {k}: {last} -> {f}"
        );
        if k == 1 {
            assert!(
                (to - f).abs() > 0.9 * (to - from).abs(),
                "the first chunk is far from the target: {f} of {from} -> {to}"
            );
        }
        if k < steps {
            assert!(f != to && dmp != to_damp, "landed early at {k}");
        }
        last = f;
    }
    assert_eq!(chunk(&mut r), (to, to_damp));
    assert!(from != to && from_damp != to_damp);
}
