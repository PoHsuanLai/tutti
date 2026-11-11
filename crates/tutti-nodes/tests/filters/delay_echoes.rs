//! The delay line's echoes, from its definition: the wet output is the input
//! delayed by the delay time, and what feeds back re-enters the line, so an
//! echo is the last one scaled by the feedback and a period later.
//!
//! The rate is a power of two and the times are whole frames over it, so a
//! delay in frames is exact in `f64`.

use tutti_fixtures::signal::{impulse, sine};
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{DelayLineNode, InterpolationMode};
use tutti_types::{Amplitude, ChannelLayout, Feedback, Hz, SampleRate, Samples, Seconds};

const SR: SampleRate = SampleRate(1_024.0);

/// `frames` frames over `SR`, as a time.
fn frames(frames: f64) -> Seconds {
    Seconds::new(frames / SR.get())
}

/// `node` over `inputs`, in ragged blocks.
fn render(node: DelayLineNode, inputs: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, 64);
    let len = inputs[0].len();
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); inputs.len()];
    let (mut pos, mut k) = (0, 0);
    while pos < len {
        let n = [64, 17, 1, 33][k % 4].min(len - pos);
        k += 1;
        let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[pos..pos + n]).collect();
        for (o, b) in out.iter_mut().zip(drive(&mut node, SR, &ins, &[])) {
            o.extend(b);
        }
        pos += n;
    }
    out
}

/// An impulse leaves a whole-frame delay after exactly that many frames, and
/// with feedback `g` each echo is `g` times the last, one period on: the
/// frames in between are exactly silent.
#[test]
fn an_impulse_echoes_at_the_delay_scaled_by_the_feedback_each_period() {
    let (d, g) = (16usize, 0.5f32);
    let node = DelayLineNode::new(Seconds::new(1.0), frames(d as f64), Feedback::new(g));
    let out = render(node, &[impulse(Samples(d * 8 + 5))]);
    for (i, &y) in out[0].iter().enumerate() {
        let want = if i >= d && i % d == 0 {
            g.powi((i / d - 1) as i32)
        } else {
            0.0
        };
        assert!((y - want).abs() < 1e-6, "frame {i}: got {y}, want {want}");
    }
}

/// A delay of a fraction of a frame reproduces a low sine delayed by the same
/// time, to the interpolator's error: the linear read's is `w^2 / 8` at a half
/// frame (`w` the sine's radians per frame). The cubic Hermite (Catmull-Rom)
/// read reproduces quadratics, so its error is third order: on `x^3` it is
/// `t (1 - t) (1 - 2t)`, at most about 0.096 (at `t ≈ 0.21`), which is
/// `0.016 · |f'''|`, and `|f'''| = w^3` for a unit sine; `w^3 / 8` is that
/// bound with an eightfold margin for the higher-order terms.
#[test]
fn a_fractional_delay_is_a_sine_delayed_by_that_time_within_the_interpolators_error() {
    let delay = 16.5f64;
    let hz = SR.get() / 64.0;
    let w = std::f64::consts::TAU * hz / SR.get();
    let x = sine(Hz::new(hz as f32), Amplitude::new(1.0), SR, Samples(512));
    let mut errors = Vec::new();
    for (mode, tolerance) in [
        (InterpolationMode::Linear, w * w / 8.0 * 1.05),
        (InterpolationMode::CubicHermite, w * w * w / 8.0),
    ] {
        let node = DelayLineNode::new(Seconds::new(1.0), frames(delay), Feedback::new(0.0))
            .with_interpolation(mode);
        let out = render(node, std::slice::from_ref(&x));
        let worst = (20..512)
            .map(|n| (f64::from(out[0][n]) - (w * (n as f64 - delay)).sin()).abs())
            .fold(0.0, f64::max);
        assert!(
            worst <= tolerance,
            "{mode:?}: worst error {worst}, bound {tolerance}"
        );
        errors.push(worst);
    }
    assert!(
        errors[1] < errors[0],
        "the cubic read is the closer one: {errors:?}"
    );
}

/// With a ring routing and no self-feedback, each echo moves to the next
/// channel and is the cross-feedback amount times the last: an impulse into
/// channel 0 is heard on channel `c` at `c + 1` periods, scaled by the amount
/// to the `c`, and nowhere else (up to the lap that would wrap back to 0).
#[test]
fn a_ring_routed_echo_steps_to_the_next_channel_scaled_by_the_cross_feedback() {
    let (n, d, amount) = (6usize, 8usize, 0.5f32);
    let mut ring = vec![0.0f32; n * n];
    for c in 0..n {
        ring[c * n + (c + n - 1) % n] = 1.0; // channel c is fed by channel c - 1
    }
    let node = DelayLineNode::with_channels(
        ChannelLayout::try_from(n).expect("a small width"),
        Seconds::new(1.0),
        frames(d as f64),
        Feedback::new(0.0),
    )
    .with_cross_feedback_matrix(&ring);
    node.set_cross_feedback(Feedback::new(amount));
    // Stops before the echo wraps round to channel 0.
    let mut inputs = vec![vec![0.0; d * (n + 1)]; n];
    inputs[0][0] = 1.0;
    let out = render(node, &inputs);
    for (c, channel) in out.iter().enumerate() {
        for (i, &y) in channel.iter().enumerate() {
            let want = if i == (c + 1) * d {
                amount.powi(c as i32)
            } else {
                0.0
            };
            assert!(
                (y - want).abs() < 1e-6,
                "channel {c}, frame {i}: got {y}, want {want}"
            );
        }
    }
}

/// The stereo delay's default routing crosses left and right: an impulse in
/// the left is heard on the left after one period, on the right after two
/// scaled by the cross-feedback, and on the left again after three scaled by
/// its square.
#[test]
fn the_stereo_cross_feedback_ping_pongs_the_echo() {
    let (d, amount) = (8usize, 0.5f32);
    let node = DelayLineNode::stereo(
        Seconds::new(1.0),
        frames(8.0),
        frames(8.0),
        Feedback::new(0.0),
    );
    node.set_cross_feedback(Feedback::new(amount));
    let mut left = vec![0.0; d * 5];
    left[0] = 1.0;
    let out = render(node, &[left.clone(), vec![0.0; left.len()]]);
    let mut want = vec![vec![0.0f32; left.len()]; 2];
    want[0][d] = 1.0;
    want[1][2 * d] = amount;
    want[0][3 * d] = amount * amount;
    want[1][4 * d] = amount.powi(3);
    for c in 0..2 {
        for i in 0..left.len() {
            assert!(
                (out[c][i] - want[c][i]).abs() < 1e-6,
                "channel {c}, frame {i}: got {}, want {}",
                out[c][i],
                want[c][i]
            );
        }
    }
}
