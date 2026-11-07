use super::*;
use tutti_metering::AudioTap;

/// An opened tap plus its consumer, fed through the real
/// `AudioTap::push` — the same call the audio callback makes.
///
/// Built this way rather than from a hand-split `HeapRb` so the tests
/// exercise the production path: `push` takes flat interleaved samples and
/// pairs them into the ring, which is the conversion the first test below
/// is actually about.
fn tap_with(frames: &[(f32, f32)]) -> TapIn {
    let tap = AudioTap::new();
    let cons = tap.open().expect("a fresh tap opens");
    let flat: Vec<f32> = frames.iter().flat_map(|&(l, r)| [l, r]).collect();
    tap.push(&flat, frames.len());
    TapIn::new(cons)
}

/// Frames the callback pushed come back out in order, interleaved L then R.
///
/// The tap stores `(f32, f32)` and `AudioIn` speaks a flat interleaved
/// slice; this is the conversion, so a transposition here would silently
/// swap every recording's channels.
#[test]
fn pushed_frames_come_back_in_order_and_channel_side() {
    let frames: Vec<(f32, f32)> = (0..4).map(|i| (i as f32, -(i as f32))).collect();
    let mut tap = tap_with(&frames);
    let mut out = [0.0f32; 8 * 2];
    assert_eq!(
        tap.poll_into(&mut out),
        Samples(4),
        "the return is FRAMES, not samples"
    );

    for i in 0..4 {
        assert_eq!(out[i * 2], i as f32, "left channel, frame {i}");
        assert_eq!(out[i * 2 + 1], -(i as f32), "right channel, frame {i}");
    }
}

/// A poll is bounded by the output slice, not by what the ring holds.
///
/// The pump hands a fixed scratch buffer every pass; writing past it would
/// be the kind of overrun no test above would notice.
#[test]
fn a_poll_never_writes_past_the_output_slice() {
    let mut tap = tap_with(&[(1.0, 1.0); 32]);
    let mut out = [0.0f32; 4 * 2];
    assert_eq!(
        tap.poll_into(&mut out),
        Samples(4),
        "must fill exactly the slice"
    );

    // The rest is still queued, not dropped.
    let mut rest = [0.0f32; 32 * 2];
    assert_eq!(tap.poll_into(&mut rest), Samples(28));
}

/// An empty tap yields nothing and says so — without claiming the end.
///
/// The `ON_EMPTY` assertion is the load-bearing half: a `Starved` source
/// makes a pump park and retry, and the recording survives the gap between
/// audio callbacks. `EndOfStream` here would end every take immediately.
#[test]
fn an_empty_tap_starves_rather_than_ending() {
    let mut tap = tap_with(&[]);

    let mut out = [0.0f32; 4 * 2];
    assert_eq!(tap.poll_into(&mut out), Samples::ZERO);
    assert_eq!(
        TapIn::ON_EMPTY,
        OnEmpty::Starved,
        "the graph does not end; an empty tap is a gap, not a finish"
    );
}

/// The join this type exists for: a tap consumer satisfies the bound
/// `Recorder::start` and `pump` require.
///
/// Asserted as a compile-time bound rather than by running a recorder — the
/// defect it guards against is `TapIn` failing to *be* an `AudioIn`, which
/// is a type error, not a behaviour.
#[test]
fn a_tap_is_accepted_where_a_live_source_is_required() {
    fn takes_a_live_source<I: AudioIn + Send + 'static>(_: I) {}

    takes_a_live_source(tap_with(&[]));
}
