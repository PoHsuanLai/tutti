use super::*;

/// A line for 0.1 s at 48 kHz holds 4800 frames: `0.1 * 48000` is 4800 in
/// `f64`, so its `_ceil` is 4800, where the `f32` nearest 0.1 times 48000
/// is 4800.00007 and rounds up to 4801.
#[test]
fn a_line_is_sized_from_the_exact_seconds() {
    let from_secs = DelayLine::from_seconds(Seconds(0.1), SampleRate(48_000.0));
    assert_eq!(from_secs.buffer.len(), DelayLine::new(4800).buffer.len());
}

#[test]
fn test_delay_line_basic() {
    let mut dl = DelayLine::new(10);
    dl.push_sample(1.0);
    dl.push_sample(0.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(2.0, InterpolationMode::None);
    assert!((val - 1.0).abs() < 0.001, "Expected 1.0, got {val}");
}

/// An integer read is a delay of exactly that many frames, at every
/// delay the line holds and every cursor position: an impulse pushed at
/// frame `k` reads back at `d` only on frame `k + d`.
#[test]
fn an_integer_read_delays_by_exactly_that_many_frames() {
    const MAX: usize = 6;
    for at in 0..3 * (MAX + 1) {
        let mut dl = DelayLine::new(MAX);
        for frame in 0..at + MAX + 2 {
            dl.push_sample(if frame == at { 1.0 } else { 0.0 });
            for d in 0..=MAX {
                let want = if frame == at + d { 1.0 } else { 0.0 };
                let got = dl.read_sample(d as f32, InterpolationMode::None);
                assert_eq!(got, want, "impulse at {at}, frame {frame}, delay {d}");
            }
        }
    }
}

#[test]
fn test_delay_line_linear_interpolation() {
    let mut dl = DelayLine::new(10);
    dl.push_sample(0.0);
    dl.push_sample(1.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(1.5, InterpolationMode::Linear);
    assert!((val - 0.5).abs() < 0.001, "Expected 0.5, got {val}");
}

#[test]
fn test_delay_line_cubic_interpolation() {
    let mut dl = DelayLine::new(10);
    for i in 0..5 {
        dl.push_sample(i as f32);
    }
    let val = dl.read_sample(1.5, InterpolationMode::CubicHermite);
    // Between samples 3 and 2 (delay 1 = sample 3, delay 2 = sample 2)
    assert!(
        val > 2.0 && val < 4.0,
        "Cubic value {val} should be between recent samples"
    );
}

#[test]
fn test_delay_line_reset() {
    let mut dl = DelayLine::new(10);
    dl.push_sample(1.0);
    dl.push_sample(1.0);
    dl.reset();
    let val = dl.read_sample(1.0, InterpolationMode::None);
    assert!((val).abs() < 0.001, "After reset, should read 0.0");
}
