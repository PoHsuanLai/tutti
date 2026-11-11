//! The phaser's wet path is a chain of first-order all-passes, from the
//! textbook transfer function `H(z) = (c + z^-1) / (1 + c z^-1)`.
//!
//! At depth 0 the sweep is parked at the band's lower edge, so the coefficient
//! `c = (t - 1) / (t + 1)`, `t = tan(pi f0 / sr)`, is constant. Each stage then
//! has unit gain at every frequency and a phase that is `H` on the unit
//! circle, so a sine through `N` stages comes out the same amplitude, its
//! phase moved by `N * arg H`. At the centre `f0` a stage's phase is a quarter
//! turn, so two stages invert a sine: half the dry and half the wet cancel
//! (the phaser's notch), which a graph's 50% mix would render as silence.

use tutti_fixtures::signal::sine;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::PhaserNode;
use tutti_types::{Amplitude, ChannelLayout, Depth, Feedback, Hz, SampleRate, Samples};

const SR: SampleRate = SampleRate(48_000.0);
const F0: f32 = 1_000.0;
const LEN: usize = 6_000;

/// The wet output of a depth-0 phaser of `stages` stages parked at `F0`, over a
/// sine of `hz`, in ragged blocks.
fn wet(stages: usize, hz: f32) -> Vec<f32> {
    let mut node = PhaserNode::with_channels(ChannelLayout::MONO, stages);
    node.set_frequency_range(Hz::new(F0), Hz::new(2_000.0));
    node.set_depth(Depth::new(0.0));
    node.set_feedback(Feedback::new(0.0));
    let mut node = prepared(node, SR, 64);
    let x = sine(Hz::new(hz), Amplitude::new(1.0), SR, Samples(LEN));
    let mut out = Vec::new();
    let (mut pos, mut k) = (0, 0);
    while pos < LEN {
        let n = [64, 17, 1, 33][k % 4].min(LEN - pos);
        k += 1;
        out.extend(&drive(&mut node, SR, &[&x[pos..pos + n]], &[])[0]);
        pos += n;
    }
    out
}

/// The phase of one stage at `hz`, from the textbook `H` on the unit circle.
fn stage_phase(hz: f64) -> f64 {
    let t = (std::f64::consts::PI * f64::from(F0) / SR.get()).tan();
    let c = (t - 1.0) / (t + 1.0);
    let w = std::f64::consts::TAU * hz / SR.get();
    let (zr, zi) = (w.cos(), -w.sin()); // z^-1 on the unit circle
    let (nr, ni) = (c + zr, zi);
    let (dr, di) = (1.0 + c * zr, c * zi);
    // arg(n / d) = arg(n * conj d)
    (ni * dr - nr * di).atan2(nr * dr + ni * di)
}

/// A depth-0 phaser's wet output is the sine at unit amplitude with the phase
/// `stages * arg H`, at, below and above the stage's centre.
#[test]
fn a_parked_phaser_is_unit_gain_with_the_textbook_all_pass_phase() {
    for stages in [2, 4, 6] {
        for hz in [250.0f32, 1_000.0, 3_000.0] {
            let out = wet(stages, hz);
            let w = std::f64::consts::TAU * f64::from(hz) / SR.get();
            let phase = stages as f64 * stage_phase(f64::from(hz));
            let worst = (LEN - 1_000..LEN)
                .map(|n| (f64::from(out[n]) - (w * n as f64 + phase).sin()).abs())
                .fold(0.0, f64::max);
            assert!(worst < 2e-3, "{stages} stages at {hz} Hz: error {worst}");
        }
    }
}

/// At the centre two stages are half a turn apart from the dry signal: the
/// wet output is the input inverted, the condition for the notch.
#[test]
fn two_stages_invert_a_sine_at_their_centre() {
    let out = wet(2, F0);
    let x = sine(Hz::new(F0), Amplitude::new(1.0), SR, Samples(LEN));
    let worst = (LEN - 1_000..LEN)
        .map(|n| (out[n] + x[n]).abs())
        .fold(0.0, f32::max);
    assert!(worst < 2e-3, "wet is not the inverted input: {worst}");
}
