//! Regression gate: `MidiClockDecoder::tick` must not allocate.
//!
//! The decoder runs on whichever thread receives MIDI input — for hardware
//! clock messages that arrives on a high-priority I/O thread, and the
//! decoded position then flows into the audio callback. Either path is
//! RT-sensitive: an allocation here either drops MIDI sync ticks or
//! adds jitter to the derived tempo.
//!
//! Tick body is pure scalar state arithmetic + a fixed-size ring buffer
//! update; the gate is a regression net against a future change that
//! moves to a `Vec`-backed interval history or similar.

use assert_no_alloc::AllocDisabler;
use tutti_midi_types::sync::clock::MidiClockDecoder;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const PPQN: u32 = 24;
fn us_per_tick(bpm: f64) -> u64 {
    (60_000_000.0 / (bpm * PPQN as f64)) as u64
}

#[test]
fn midi_clock_decoder_tick_is_allocation_free() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();

    // Warm up the interval ring buffer so subsequent calls take the
    // tempo-update branch every time.
    let interval = us_per_tick(120.0);
    let mut ts = 0u64;
    for _ in 0..48 {
        clock.tick(ts);
        ts += interval;
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            clock.tick(ts);
            ts += interval;
        }
    });
}

#[test]
fn midi_clock_decoder_transport_msgs_are_allocation_free() {
    let mut clock = MidiClockDecoder::new();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            clock.start_msg();
            clock.stop_msg();
            clock.continue_msg();
            clock.reset();
        }
    });
}

#[test]
fn midi_clock_decoder_queries_are_allocation_free() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();
    let interval = us_per_tick(140.0);
    let mut ts = 0u64;
    for _ in 0..48 {
        clock.tick(ts);
        ts += interval;
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            let _ = clock.beat_position();
            let _ = clock.tempo_bpm();
            let _ = clock.transport_state();
        }
    });
}
