use super::*;

/// **A reading is what was written**: the OS time, the frame and the rate of
/// one block, and nothing before the first write.
#[test]
fn a_reader_sees_the_block_last_written() {
    let mut clock = BlockClock::new();
    let reader = clock.reader();
    assert_eq!(reader.read(), None, "nothing before the first block");

    let start = Instant::now();
    clock.write(start, Frame(4_800), SampleRate(48_000.0));
    let (os, at) = reader.read().expect("written");
    assert_eq!(os, start);
    assert_eq!(at, Moment::new(Frame(4_800), SampleRate(48_000.0)));

    let later = start + Duration::from_millis(10);
    clock.write(later, Frame(5_280), SampleRate(48_000.0));
    assert_eq!(
        reader.read(),
        Some((later, Moment::new(Frame(5_280), SampleRate(48_000.0)))),
        "the latest write replaces the one before"
    );
}

/// **A reader cloned from a reader sees the same clock.**
#[test]
fn a_cloned_reader_reads_the_same_clock() {
    let mut clock = BlockClock::new();
    let reader = clock.reader().clone();
    let os = Instant::now();
    clock.write(os, Frame(1), SampleRate(44_100.0));
    assert_eq!(reader.read().map(|(o, _)| o), Some(os));
}
