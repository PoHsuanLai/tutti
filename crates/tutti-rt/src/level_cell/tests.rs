use super::*;

/// A fold then a take returns it whole, and the take clears: the next is
/// empty.
///
/// Mutation (run, recorded here): `take` loading the peak instead of swapping
/// it -> the second take still shows the peak and this fails.
#[test]
fn a_take_returns_the_fold_and_clears_it() {
    let (w, mut r) = level_cell();
    w.fold(0.5, 4.0, 16);
    w.fold(0.25, 2.0, 8);
    assert_eq!(
        r.take(),
        LevelSpan {
            peak: 0.5,
            sum_sq: 6.0,
            frames: 24
        }
    );
    assert_eq!(r.take(), LevelSpan::default());
    w.fold(0.125, 1.0, 4);
    assert_eq!(r.take().frames, 4, "the other slot works too");
}

/// An hour of 48 kHz offline render taken once stays exact: an `f32` sum of
/// squares stalls past about 2^24, so the RMS drifts.
///
/// Mutation (run, recorded here): the slot's sum stored as `f32` bits
/// (`(sum as f32 + sum_sq as f32)`) -> the RMS reads low and this fails.
#[test]
fn a_long_unread_span_stays_exact() {
    let (w, mut r) = level_cell();
    // 512-frame blocks of 0.5: 128 each; 337_500 blocks is an hour at 48 kHz.
    let blocks = 337_500u64;
    for _ in 0..blocks {
        w.fold(0.5, 128.0, 512);
    }
    let span = r.take();
    assert_eq!(span.frames, blocks * 512);
    assert_eq!(span.sum_sq, blocks as f64 * 128.0);
    assert_eq!((span.sum_sq / span.frames as f64).sqrt(), 0.5);
}

/// The frame count saturates instead of wrapping.
///
/// Mutation (run, recorded here): `saturating_add` -> `wrapping_add` -> the
/// count wraps to a small number and this fails.
#[test]
fn the_count_saturates() {
    let (w, mut r) = level_cell();
    w.shared.slots[0]
        .frames
        .store(u64::MAX - 1, Ordering::Relaxed);
    w.fold(1.0, 1.0, 5);
    assert_eq!(r.take().frames, u64::MAX);
}
