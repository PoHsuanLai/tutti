use super::*;

/// **A reading is what was written**: the OS time, the frame and the rate of
/// one call, nothing before the first write, and the latest replaces the one
/// before.
#[test]
fn a_reader_sees_the_call_last_written() {
    let mut clock = BlockClock::new();
    let reader = clock.reader();
    assert_eq!(reader.read(), None, "nothing before the first call");

    clock.write(-5, Frame(4_800), SampleRate(48_000.0));
    assert_eq!(
        reader.read(),
        Some(BlockReading {
            os_ns: -5,
            frame: Frame(4_800),
            rate: SampleRate(48_000.0)
        })
    );
    clock.write(10_000_000, Frame(5_280), SampleRate(48_000.0));
    assert_eq!(reader.clone().read().map(|r| r.frame), Some(Frame(5_280)));
}
