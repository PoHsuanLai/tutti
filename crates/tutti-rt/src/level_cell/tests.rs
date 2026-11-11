use super::*;

/// A fold then a take returns it whole, and the take clears: the next is
/// empty.
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
    // The next takes read the other slot, then this one again, cleared.
    assert_eq!(r.take(), LevelSpan::default());
    assert_eq!(r.take(), LevelSpan::default());
    w.fold(0.125, 1.0, 4);
    assert_eq!(r.take().frames, 4, "the other slot works too");
}

/// An hour of 48 kHz offline render taken once stays exact: an `f32` sum of
/// squares stalls past about 2^24, so the RMS drifts.
#[test]
fn a_long_unread_span_stays_exact() {
    let (w, mut r) = level_cell();
    // 512-frame blocks each worth 127 (not a power of two, so an `f32` sum
    // rounds once it passes 2^24); 337_500 blocks is an hour at 48 kHz.
    let blocks = 337_500u64;
    for _ in 0..blocks {
        w.fold(0.5, 127.0, 512);
    }
    let span = r.take();
    assert_eq!(span.frames, blocks * 512);
    assert_eq!(span.sum_sq, blocks as f64 * 127.0);
}

/// The frame count saturates instead of wrapping.
#[test]
fn the_count_saturates() {
    let (w, mut r) = level_cell();
    w.shared.slots[0]
        .frames
        .store(u64::MAX - 1, Ordering::Relaxed);
    w.fold(1.0, 1.0, 5);
    assert_eq!(r.take().frames, u64::MAX);
}

/// A writer thread and a taking thread conserve the total, and every take is
/// whole (its energy and frames agree): the std-thread twin of the loom model.
#[test]
fn concurrent_takes_lose_nothing_and_stay_whole() {
    const BLOCKS: u64 = 50_000;
    let (w, mut r) = level_cell();
    let writer = std::thread::spawn(move || {
        for _ in 0..BLOCKS {
            w.fold(0.5, 4.0, 16);
        }
    });
    let (mut frames, mut sum) = (0u64, 0.0f64);
    while !writer.is_finished() {
        let span = r.take();
        assert_eq!(span.sum_sq, span.frames as f64 / 4.0, "{span:?}");
        assert_eq!(span.peak > 0.0, span.frames > 0, "{span:?}");
        frames += span.frames;
        sum += span.sum_sq;
    }
    writer.join().expect("the writer finished");
    let span = r.take();
    frames += span.frames;
    sum += span.sum_sq;
    assert_eq!(frames, BLOCKS * 16);
    assert_eq!(sum, BLOCKS as f64 * 4.0);
}
