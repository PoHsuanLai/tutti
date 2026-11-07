use super::*;
use tutti_ump::MidiChannel;
use tutti_ump::MidiGroup;

fn us_per_tick(bpm: f64) -> u64 {
    // 1 beat = PPQN ticks; 1 beat at bpm = 60/bpm seconds = 60_000_000/bpm us
    (60_000_000.0 / (bpm * PPQN as f64)) as u64
}

/// 24 ticks is one beat, and the count keeps accumulating across beats.
#[test]
fn test_basic_beat_counting() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();
    assert_eq!(clock.transport_state(), ClockTransportState::Playing);
    assert!((clock.beat_position() - 0.0).abs() < 0.001);

    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..24 {
        clock.tick(ts);
        ts += interval;
    }
    assert!((clock.beat_position() - 1.0).abs() < 0.001);

    // Three more beats' worth: the position accumulates rather than
    // wrapping or resetting at the beat boundary.
    for _ in 0..72 {
        clock.tick(ts);
        ts += interval;
    }
    assert!(
        (clock.beat_position() - 4.0).abs() < 0.001,
        "96 ticks / 24 PPQN = 4 beats, got {}",
        clock.beat_position()
    );
}

// Mutation: ignore 0xF2 (the old behaviour) and the position stays 0;
// swap the two halves and 0x0123 reads as 0x2301.
#[test]
fn song_position_then_continue_resumes_at_spp_times_six_ticks() {
    let mut clock = MidiClockDecoder::new();
    assert!(clock.feed(&MidiEvent::song_position(MidiGroup::FIRST, 0x0123), 0));
    // 0x0123 sixteenths = 291 × 6 ticks.
    assert!((clock.beat_position() - 291.0 * 6.0 / 24.0).abs() < 1e-9);
    assert!(clock.feed(&MidiEvent::continue_msg(MidiGroup::FIRST), 0));
    let mut ts = 0;
    for _ in 0..24 {
        clock.feed(&MidiEvent::timing_clock(MidiGroup::FIRST), ts);
        ts += us_per_tick(120.0);
    }
    assert!((clock.beat_position() - (291.0 * 6.0 / 24.0 + 1.0)).abs() < 1e-9);
    // A Start begins at zero whatever the pointer said.
    clock.feed(&MidiEvent::start(MidiGroup::FIRST), ts);
    assert_eq!(clock.beat_position(), 0.0);
}

#[test]
fn feed_dispatches_transport_events_like_the_direct_methods() {
    let mut clock = MidiClockDecoder::new();
    // Start via a real MidiEvent, then advance a beat of clock ticks.
    assert!(clock.feed(&MidiEvent::start(MidiGroup::FIRST), 0));
    assert_eq!(clock.transport_state(), ClockTransportState::Playing);

    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..24 {
        assert!(clock.feed(&MidiEvent::timing_clock(MidiGroup::FIRST), ts));
        ts += interval;
    }
    assert!((clock.beat_position() - 1.0).abs() < 0.001);

    assert!(clock.feed(&MidiEvent::stop(MidiGroup::FIRST), ts));
    assert_eq!(clock.transport_state(), ClockTransportState::Stopped);

    // A non-transport event is ignored and reported as unrecognised.
    assert!(!clock.feed(
        &MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ts
    ));
}

/// The decoder recovers the sender's tempo from the tick spacing alone.
///
/// A table over several rates rather than one test per rate: a decoder that
/// returned a hardcoded 120 would pass a single-rate test, so the rates
/// have to vary within one property for the assertion to mean anything.
#[test]
fn tempo_is_derived_from_the_tick_interval() {
    for bpm in [60.0f64, 120.0, 140.0, 200.0] {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(bpm);
        let mut ts = 0u64;
        // Two beats' worth — enough for the estimate to stabilize.
        for _ in 0..48 {
            clock.tick(ts);
            ts += interval;
        }

        let tempo = clock
            .tempo_bpm()
            .expect("48 ticks is enough to derive a tempo");
        assert!(
            !tempo.differs_from(Bpm(bpm), 1.0),
            "expected ~{bpm} BPM, got {tempo:?}"
        );
    }
}

#[test]
fn test_start_resets_position() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();

    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..48 {
        clock.tick(ts);
        ts += interval;
    }
    assert!(clock.beat_position() > 1.0);

    clock.start_msg();
    assert!((clock.beat_position() - 0.0).abs() < 0.001);
}

#[test]
fn test_stop_pauses_counting() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();

    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..24 {
        clock.tick(ts);
        ts += interval;
    }
    let pos_before_stop = clock.beat_position();

    clock.stop_msg();
    for _ in 0..24 {
        clock.tick(ts);
        ts += interval;
    }
    // Position should not advance while stopped
    assert!(
        (clock.beat_position() - pos_before_stop).abs() < 0.001,
        "Position should not advance while stopped"
    );
}

#[test]
fn test_continue_resumes() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();

    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..24 {
        clock.tick(ts);
        ts += interval;
    }
    let pos_at_stop = clock.beat_position();

    clock.stop_msg();
    // Tick while stopped
    for _ in 0..24 {
        clock.tick(ts);
        ts += interval;
    }

    clock.continue_msg();
    for _ in 0..24 {
        clock.tick(ts);
        ts += interval;
    }
    // Should have advanced by 1 beat from the stop position
    assert!(
        (clock.beat_position() - (pos_at_stop + 1.0)).abs() < 0.001,
        "Continue should resume from stop position"
    );
}

#[test]
fn test_no_tempo_before_enough_ticks() {
    let clock = MidiClockDecoder::new();
    assert!(clock.tempo_bpm().is_none());
}

#[test]
fn test_reset() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();

    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..48 {
        clock.tick(ts);
        ts += interval;
    }

    clock.reset();
    assert!(clock.tempo_bpm().is_none());
    assert!((clock.beat_position() - 0.0).abs() < 0.001);
    assert_eq!(clock.transport_state(), ClockTransportState::Stopped);
}
