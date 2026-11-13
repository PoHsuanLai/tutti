use super::*;
use tutti_types::{Beat, Bpm, Samples, Seconds};

/// **A reading is what was written**: the OS time, the frame and the rate of
/// one call, nothing before the first write, and the latest replaces the one
/// before.
#[test]
fn a_reader_sees_the_call_last_written() {
    let mut clock = BlockClock::new();
    let reader = clock.reader();
    assert_eq!(reader.read(), None, "nothing before the first call");

    clock.write(-5, Frame::new(4_800), SampleRate::SR_48K, pose());
    assert_eq!(
        reader.read(),
        Some(BlockReading {
            os_ns: -5,
            frame: Frame::new(4_800),
            rate: SampleRate::SR_48K
        })
    );
    clock.write(10_000_000, Frame::new(5_280), SampleRate::SR_48K, pose());
    assert_eq!(
        reader.clone().read().map(|r| r.frame),
        Some(Frame::new(5_280))
    );
}

fn pose() -> Pose {
    Pose {
        beat: Beat(0.0),
        seconds: Seconds(0.0),
        tempo: Bpm::of(120.0),
        playing: false,
        span: Samples(512),
    }
}

/// **One read is one write**: the beat, frame, seconds and playing flag of a
/// snapshot are those of the one call that wrote them, never a mix of two.
#[test]
fn a_playhead_read_is_the_call_last_written() {
    let mut clock = BlockClock::new();
    let reader = clock.reader();
    assert_eq!(
        reader.playhead(),
        Playhead::default(),
        "resting before a call"
    );

    let rolling = Pose {
        beat: Beat(3.0),
        seconds: Seconds(1.5),
        playing: true,
        ..pose()
    };
    clock.write(1_000, Frame::new(48_000), SampleRate::SR_48K, rolling);
    let a = reader.playhead();
    assert_eq!(
        (a.beat(), a.frame(), a.seconds(), a.playing(), a.at()),
        (
            Beat(3.0),
            Frame::new(48_000),
            Seconds(1.5),
            true,
            Some(1_000)
        )
    );
    let stopped = Pose {
        beat: Beat(7.0),
        seconds: Seconds(3.5),
        playing: false,
        ..pose()
    };
    clock.write(2_000, Frame::new(48_512), SampleRate::SR_48K, stopped);
    let b = reader.playhead();
    assert_eq!(
        (b.beat(), b.frame(), b.seconds(), b.playing(), b.at()),
        (
            Beat(7.0),
            Frame::new(48_512),
            Seconds(3.5),
            false,
            Some(2_000)
        )
    );
}

/// **`beat_now` extrapolates, bounded**: 120 BPM is 2 beats a second, so a
/// call of 480 frames at 48 kHz (10 ms) read half a block later is 0.005 s
/// on, beat + 0.01. At the call's own time it is the snapshot's beat; a stopped
/// transport does not move; a `now` far past the call is held at one block.
#[test]
fn beat_now_interpolates_within_one_block() {
    let mut clock = BlockClock::new();
    let reader = clock.reader();
    let playing = Pose {
        beat: Beat(4.0),
        playing: true,
        span: Samples(480),
        ..pose()
    };
    let at = 1_000_000_000;
    clock.write(at, Frame::new(0), SampleRate::SR_48K, playing);
    let p = reader.playhead();
    assert_eq!(p.beat_now(at), Beat(4.0), "at its own time");
    assert!(
        (p.beat_now(at + 5_000_000).get() - 4.01).abs() < 1e-12,
        "half a block on"
    );
    assert!(
        (p.beat_now(at + 10_000_000).get() - 4.02).abs() < 1e-12,
        "a whole block on"
    );
    assert_eq!(
        p.beat_now(at + 10_000_000),
        p.beat_now(at + 9_000_000_000),
        "clamped to a block"
    );
    assert_eq!(
        p.beat_now(at - 5_000_000),
        Beat(4.0),
        "never back before the call"
    );

    let stopped = Pose {
        playing: false,
        ..playing
    };
    clock.write(at, Frame::new(0), SampleRate::SR_48K, stopped);
    assert_eq!(
        reader.playhead().beat_now(at + 5_000_000),
        Beat(4.0),
        "stopped holds"
    );
}
