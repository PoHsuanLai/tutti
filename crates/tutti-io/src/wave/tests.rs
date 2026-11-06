use super::*;

/// `push_frame` appends to every channel and advances the length once.
///
/// Mutation: advancing `len` per channel inside the loop makes `len()` 2
/// here and fails.
#[test]
fn push_frame_appends_one_frame_across_channels() {
    let mut w = Wave::new(2, 48_000.0);
    w.push_frame(&[0.25, -0.5]);
    assert_eq!(w.len(), 1);
    assert_eq!((w.at(0, 0), w.at(1, 0)), (0.25, -0.5));
}

/// Mutation: dropping the width assert lets a mono frame into a stereo
/// wave, leaving channel 1 one sample short, and this stops panicking.
#[test]
#[should_panic(expected = "frame width")]
fn push_frame_rejects_a_frame_of_the_wrong_width() {
    Wave::new(2, 48_000.0).push_frame(&[0.5]);
}

/// Mutation: `round()` → `floor()` in `zero` makes 0.5 frames 0 here.
#[test]
fn zero_rounds_the_duration_to_the_nearest_frame() {
    let w = Wave::zero(1, 2.0, 0.75);
    assert_eq!(w.len(), 2);
    assert!(w.channel(0).iter().all(|&s| s == 0.0));
}
