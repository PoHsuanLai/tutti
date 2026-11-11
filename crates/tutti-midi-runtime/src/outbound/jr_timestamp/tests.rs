use super::*;
use tutti_fixtures::midi::{jr_timestamp, note_off, note_on};
use tutti_graph::UmpAt;
use tutti_types::{Frame, Samples};
use tutti_ump::{KeyEvent, Message, MidiChannel, MidiGroup, Utility};

/// The sender-clock value if `ump` is a JR Clock (M2-104 section 7.2.2.1).
fn jr_clock_value(ump: &Ump) -> Option<u16> {
    match (*ump).decode() {
        Ok(Message::Utility(Utility::JrClock(t))) => Some(t),
        _ => None,
    }
}

/// The timestamp value if `ump` is a JR Timestamp (section 7.2.2.2).
fn jr_timestamp_value(ump: &Ump) -> Option<u16> {
    match (*ump).decode() {
        Ok(Message::Utility(Utility::JrTimestamp(t))) => Some(t),
        _ => None,
    }
}

/// `ump` at frame `offset`, built against a block long enough to hold it (the
/// stamper reads the offset, not the block).
fn at(offset: u32, ump: Ump) -> UmpAt {
    (Offset::new(offset as usize, Samples(1 << 20)).unwrap(), ump)
}

#[test]
fn clock_maps_samples_to_ticks_monotonically() {
    let clock = JrClock::new(SampleRate::new(48_000.0));
    // 48000 samples = 1 s = 31250 ticks, which wraps: 31250 & 0xFFFF = 31250.
    assert_eq!(clock.ticks_at(Frame(0)), 0);
    assert_eq!(clock.ticks_at(Frame(48_000)), 31_250);
    // Half a second → half the ticks.
    assert_eq!(clock.ticks_at(Frame(24_000)), 15_625);
}

#[test]
fn clock_wraps_at_16_bits() {
    // At exactly the JR reference rate, one sample is one tick, so the wrap
    // boundary is integer-exact (no float rounding at the edge).
    let clock = JrClock::new(SampleRate::new(JR_TICKS_PER_SECOND as f64));
    assert_eq!(clock.ticks_at(Frame(0xFFFF)), 0xFFFF);
    assert_eq!(clock.ticks_at(Frame(0x1_0000)), 0, "wraps back to zero");
    assert_eq!(clock.ticks_at(Frame(0x1_0001)), 1);
}

#[test]
fn the_first_clock_is_due_immediately() {
    // A receiver cannot use any stamp until it has a clock reference, so
    // waiting one interval before the first would leave the opening block's
    // timing unusable.
    let mut e = JrClockEmitter::new(SampleRate::new(48_000.0));
    let first = e.due(Frame(0)).expect("a clock is owed at once");
    assert_eq!(jr_clock_value(&first), Some(0));
}

#[test]
fn clocks_are_emitted_on_the_configured_cadence() {
    let mut e =
        JrClockEmitter::with_interval(SampleRate::new(48_000.0), Duration::from_millis(100));
    let interval = e.interval();
    assert_eq!(interval, Samples(4_800));
    let at = |frames: Samples| Frame(frames.get() as u64);

    assert!(e.due(Frame(0)).is_some(), "first");
    assert!(
        e.due(at(Samples(interval.get() - 1))).is_none(),
        "one sample short"
    );
    assert!(e.due(at(interval)).is_some(), "exactly due");
    assert!(
        e.due(at(Samples(interval.get() + 1))).is_none(),
        "just emitted"
    );
    assert!(
        e.due(at(Samples(interval.get() * 2))).is_some(),
        "next period"
    );
}

#[test]
fn the_cadence_keeps_running_through_silence() {
    // The load-bearing case. §7.2.2.1: JR Clocks are "independent … not
    // related to any other message", and §7.2.2.3 makes a receiver discard
    // every stamp until it sees one. A cadence driven by outgoing traffic
    // would satisfy a test that sends notes and still emit nothing on an
    // idle stream — which is exactly when a receiver most needs the clock.
    let sample_rate = 48_000.0;
    let block = Samples(512);
    let mut stream = JrStream::new(SampleRate::new(sample_rate))
        .with_clock_interval(SampleRate::new(sample_rate), Duration::from_millis(100));

    let mut clocks = 0;
    // 1 second of entirely silent blocks.
    for _ in 0..(sample_rate as usize / block.get()) {
        let mut out = Vec::new();
        stream.stamp_span(&[], block, &mut out);
        clocks += out
            .iter()
            .filter(|e| jr_clock_value(&e.1).is_some())
            .count();
        assert!(
            out.iter().all(|e| jr_clock_value(&e.1).is_some()),
            "a silent block carries clocks and nothing else"
        );
    }
    // 100 ms cadence over ~1 s: the first plus one per interval.
    assert!(
        (10..=11).contains(&clocks),
        "expected ~10 clocks across a silent second, got {clocks}"
    );
}

#[test]
fn a_stamped_block_carries_its_clock_before_the_stamps() {
    let sample_rate = 48_000.0;
    let mut stream =
        JrStream::new(SampleRate::new(sample_rate)).with_clock(SampleRate::new(sample_rate));
    let events = [at(
        0,
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
    )];
    let mut out = Vec::new();
    stream.stamp_span(&events, Samples(512), &mut out);
    // clock, then stamp, then the note.
    assert_eq!(out.len(), 3);
    assert!(jr_clock_value(&out[0].1).is_some(), "clock leads");
    assert!(jr_timestamp_value(&out[1].1).is_some(), "then the stamp");
    assert_eq!(out[2], events[0]);
}

#[test]
fn a_stream_without_a_clock_emits_none() {
    // `new` alone stamps only — the cadence is opt-in, so existing
    // stamp-only callers keep their exact output.
    let mut stream = JrStream::new(SampleRate::new(48_000.0));
    let mut out = Vec::new();
    stream.stamp_span(&[], Samples(512), &mut out);
    assert!(out.is_empty(), "no clock, no events, no output");
}

#[test]
fn stamp_advances_by_events_and_stamp_span_by_the_block() {
    // The distinction the cadence depends on: `stamp` cannot move time on an
    // empty block, `stamp_span` can.
    let mut by_events = JrStream::new(SampleRate::new(48_000.0));
    by_events.stamp(&[], &mut Vec::new());
    assert_eq!(by_events.origin(), Frame(0), "silence stalls the origin");

    let mut by_block = JrStream::new(SampleRate::new(48_000.0));
    by_block.stamp_span(&[], Samples(512), &mut Vec::new());
    assert_eq!(by_block.origin(), Frame(512), "the block advanced it");
}

#[test]
#[should_panic(expected = "exceeds the §7.2.2.1 maximum")]
fn an_interval_over_the_spec_maximum_is_rejected() {
    // §7.2.2.1's 250 ms is a `shall`, so a longer interval is not a tuning
    // choice — it is a non-conformant stream.
    JrClockEmitter::with_interval(SampleRate::new(48_000.0), Duration::from_millis(251));
}

#[test]
fn the_default_interval_is_inside_the_spec_maximum() {
    assert!(JR_CLOCK_INTERVAL < JR_CLOCK_MAX_INTERVAL);
    assert_eq!(JR_CLOCK_MAX_INTERVAL, Duration::from_millis(250));
}

/// The span is one *past* the furthest offset, so the next block's origin
/// does not re-stamp the sample the last event sat on.
#[test]
fn a_blocks_span_is_one_past_its_furthest_offset() {
    let events = [
        at(0, note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)),
        at(511, note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)),
    ];
    assert_eq!(JrStamper::block_span(&events), Samples(512));
    assert_eq!(
        JrStamper::block_span(&[]),
        Samples::ZERO,
        "an empty block spans nothing"
    );
}

/// A stream advances its own origin, so successive blocks keep climbing.
#[test]
fn a_stream_advances_its_origin_across_blocks() {
    let mut stream = JrStream::new(SampleRate::new(48_000.0));
    let events = [
        at(0, note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)),
        at(
            24_000,
            note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ];

    let mut first = Vec::new();
    stream.stamp(&events, &mut first);
    assert_eq!(jr_timestamp_value(&first[0].1), Some(0));
    assert_eq!(stream.origin(), Frame(24_001));

    // The second block's first event is stamped from the new origin, not zero.
    let mut second = Vec::new();
    stream.stamp(&events, &mut second);
    assert_eq!(jr_timestamp_value(&second[0].1), Some(15_625));
    assert_eq!(stream.origin(), Frame(48_002));
}

/// The reason the origin lives on the stream: two producers feeding one wire
/// must share it. Stamping both through one `JrStream` keeps the sequence
/// climbing; a per-producer origin would restart each at zero and hand the
/// receiver stamps that walk backwards.
///
/// The events sit at a *block-sized* offset, not zero: a JR tick is 32 µs —
/// about 1.5 samples at 48 kHz — so a one-sample advance rounds to the same
/// tick and would prove nothing either way.
#[test]
fn two_producers_on_one_stream_keep_stamps_monotonic() {
    let mut wire = JrStream::new(SampleRate::new(48_000.0));

    // One 512-frame block each, the event at the block's end.
    let clock_block = [at(
        511,
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
    )];
    let track_block = [at(
        511,
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000),
    )];

    let mut from_clock = Vec::new();
    wire.stamp(&clock_block, &mut from_clock);
    let mut from_track = Vec::new();
    wire.stamp(&track_block, &mut from_track);

    let first = jr_timestamp_value(&from_clock[0].1).unwrap();
    let second = jr_timestamp_value(&from_track[0].1).unwrap();
    assert!(
        second > first,
        "the second producer must stamp later than the first: {first} then {second}"
    );
    assert_eq!(wire.origin(), Frame(1024), "two 512-frame blocks");
}

#[test]
fn stamp_block_prefixes_each_event() {
    let stamper = JrStamper::new(SampleRate::new(48_000.0));
    let events = [
        at(0, note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)),
        at(
            24_000,
            note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ];
    let mut out = Vec::new();
    stamper.stamp_block(&events, Frame(0), &mut out);
    assert_eq!(out.len(), 4, "one timestamp + one event, twice");
    assert_eq!(jr_timestamp_value(&out[0].1), Some(0));
    assert!(matches!(out[1].1.key_event(), Some(KeyEvent::On(_))));
    assert_eq!(jr_timestamp_value(&out[2].1), Some(15_625));
    assert!(out[3].1.is_note_off());
}

#[test]
fn stamp_then_observe_recovers_spacing() {
    // Two events 0.25 s apart at 48 kHz → the receiver reconstructs ~0.25 s.
    let sr = 48_000.0;
    let stamper = JrStamper::new(SampleRate::new(sr));
    let quarter_second = (sr * 0.25) as u32; // 12000 samples
    let events = [
        at(0, note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)),
        at(
            quarter_second,
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000),
        ),
    ];
    let mut stream = Vec::new();
    stamper.stamp_block(&events, Frame(0), &mut stream);

    let mut rx = JrReceiver::new();
    let mut recovered = Vec::new();
    for ev in &stream {
        if let Some(d) = rx.observe(&ev.1) {
            recovered.push(d);
        }
    }
    // First timestamp: no prior → no delay. Its note: ZERO. Second timestamp:
    // delta ~0.25 s. Its note: ZERO.
    // The meaningful reconstruction is the second timestamp's delta.
    let nonzero: Vec<_> = recovered.iter().filter(|d| **d > Duration::ZERO).collect();
    assert_eq!(nonzero.len(), 1, "exactly one inter-event gap");
    let gap = *nonzero[0];
    let expected = Duration::from_secs_f64(0.25);
    let diff = gap.abs_diff(expected);
    assert!(
        diff < Duration::from_secs_f64(JR_SECONDS_PER_TICK * 2.0),
        "recovered {gap:?} within one tick of {expected:?}"
    );
}

#[test]
fn observe_before_any_timestamp_is_none() {
    let mut rx = JrReceiver::new();
    assert_eq!(
        rx.observe(&note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)),
        None
    );
}

#[test]
fn observe_handles_16bit_wrap() {
    // Two timestamps straddling the wrap: 0xFFF0 → 0x0010 is a delta of 0x20.
    let mut rx = JrReceiver::new();
    assert_eq!(rx.observe(&jr_timestamp(0xFFF0)), None);
    let d = rx
        .observe(&jr_timestamp(0x0010))
        .expect("has a prior stamp");
    assert_eq!(d, ticks_to_duration(0x20));
}
