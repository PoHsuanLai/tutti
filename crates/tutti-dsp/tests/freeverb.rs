//! `Freeverb`, from its definition: Freeverb's tunings (comb lengths 1116 and
//! 1188 and on, all-pass 556 and on, the right channel 23 longer, all tuned at
//! 44.1 kHz), a fixed input gain of 0.015, and a comb loop that keeps the room
//! size's fraction of what it carried each trip (0.5 * 0.28 + 0.7 = 0.84).
//! Nothing here is read from another reverb's render.

use tutti_dsp::Freeverb;
use tutti_fixtures::signal::{impulse as impulse_signal, peak};
use tutti_types::{Db, SampleRate, Samples};

const RATE: SampleRate = SampleRate(44_100.0);

/// The wet response to a unit impulse, `frames` long.
fn impulse(reverb: &mut Freeverb, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let input = impulse_signal(Samples(frames));
    let (mut left, mut right) = (vec![0.0; frames], vec![0.0; frames]);
    reverb.process(&input, &mut left, &mut right);
    (left, right)
}

/// Nothing comes out before the shortest comb has gone round once, and then
/// the first echo is the input at the fixed gain: 1116 frames on the left, 23
/// more on the right. The four all-pass stages each negate the first sample
/// they see (nothing is in their line yet), so four of them leave it as it
/// was.
#[test]
fn the_first_echo_is_the_input_at_the_fixed_gain_after_the_shortest_comb() {
    let mut reverb = Freeverb::new(RATE);
    let (left, right) = impulse(&mut reverb, 2_000);
    assert!(left[..1116].iter().all(|&s| s == 0.0), "silent until 1116");
    assert!(right[..1139].iter().all(|&s| s == 0.0), "silent until 1139");
    assert!(
        (left[1116] - Freeverb::FIXED_GAIN).abs() < 1e-6,
        "{}",
        left[1116]
    );
    assert!(
        (right[1139] - Freeverb::FIXED_GAIN).abs() < 1e-6,
        "{}",
        right[1139]
    );
    assert_eq!(Freeverb::FIXED_GAIN, 0.015);
}

/// The tunings scale with the rate: at twice 44.1 kHz the first echo is at
/// twice the frames.
#[test]
fn the_lines_scale_with_the_rate() {
    let mut reverb = Freeverb::new(SampleRate(88_200.0));
    let (left, _) = impulse(&mut reverb, 3_000);
    assert!(left[..2232].iter().all(|&s| s == 0.0));
    assert!(left[2232].abs() > 0.01);
}

/// `mute` empties the lines: after it, the response to an impulse is the same
/// as a fresh reverb's, bit for bit (a property of the state, not a render
/// compared with another implementation's).
#[test]
fn mute_returns_the_reverb_to_a_fresh_state() {
    let mut used = Freeverb::new(RATE);
    impulse(&mut used, 5_000);
    used.mute();
    let mut fresh = Freeverb::new(RATE);
    assert_eq!(impulse(&mut used, 4_000), impulse(&mut fresh, 4_000));
}

/// The ring-out bound covers the response: the last frame of the wet signal at
/// or above a thousandth (-60 dB) of its own peak is within it.
#[test]
fn the_ring_out_bound_covers_the_response() {
    let mut reverb = Freeverb::new(RATE);
    let floor = Db(-60.0);
    let bound = (reverb.ring_out(floor).get() * 44_100.0).ceil() as usize;
    let frames = bound + 20_000;
    let (left, right) = impulse(&mut reverb, frames);
    let peak = peak(&left).max(peak(&right));
    let last = |c: &[f32]| c.iter().rposition(|s| s.abs() >= 1e-3 * peak);
    let rang = last(&left).max(last(&right)).expect("it rings") + 1;
    println!("RINGOUT freeverb rang {rang} frames, bound {bound}");
    assert!(rang <= bound, "rang {rang}, bound {bound}");
    assert!(
        rang > bound / 4,
        "the bound is not absurdly loose: {rang} of {bound}"
    );
}

/// The ring-out bound is the definition's figure: the first echo of a comb comes
/// after one trip at full strength, the n-th has the room's fraction (0.84) to
/// the power n - 1, so falling to -60 dB (0.001) takes ceil(ln 0.001 / ln 0.84)
/// = 40 trips of power, one trip more: 41 round the longest comb (1617 + 23 =
/// 1640 frames at 44.1 kHz), then the four all-pass lines once
/// (556 + 441 + 341 + 225 = 1563).
#[test]
fn the_ring_out_bound_counts_the_trip_of_the_first_echo() {
    let reverb = Freeverb::new(RATE);
    let frames = reverb.ring_out(Db(-60.0)).get() * 44_100.0;
    let want = (41 * 1640 + 1563) as f64;
    assert!((frames - want).abs() < 2.0, "{frames} against {want}");
}
