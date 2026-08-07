//! An inbound hardware event is timed against the device's real sample rate.
//!
//! The port manager turns a wall-clock delta into a `frame_offset`
//! (`manager.rs`'s `read_inputs`), and that conversion needs the rate the
//! device is actually running at. `HardwareMidiInputs` is constructed before a
//! device exists, so it starts at a placeholder 44100 and documents
//! `set_sample_rate` as "call before starting the audio stream" — which nothing
//! did. At 48 kHz every inbound event landed ~8.8% early.
//!
//! # What these cover, and what they do not
//!
//! They prove `set_sample_rate` *works* — it was previously untested, being
//! called from nowhere. They do **not** prove `build_into` calls it: that
//! function opens a real CPAL device, so it cannot run headless, and the
//! timestamps here come from the wall clock rather than anything injectable
//! through the engine. The call site is covered by inspection.

#![cfg(feature = "midi-hardware")]

use std::time::{Duration, Instant};

use tutti_midi_hardware::HardwareMidiInputs;
use tutti_midi_runtime::tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_midi_types::ump::MidiEvent;

/// A block long enough that a 1 ms offset is a large, unambiguous fraction of
/// it, and short enough to stay a plausible audio block.
const NFRAMES: usize = 1024;

/// How old the event is when the block starts.
const AGE: Duration = Duration::from_millis(1);

/// Push one event aged `AGE` into a fresh manager and read back its
/// `frame_offset`.
///
/// `push` takes the timestamp explicitly (unlike `push_input_event`, which
/// stamps `Instant::now()`), which is what makes the age injectable at all.
fn offset_at(sample_rate: Option<f64>) -> u32 {
    let inputs = HardwareMidiInputs::new(256);
    if let Some(rate) = sample_rate {
        inputs.set_sample_rate(rate);
    }
    let port = inputs.create_input_port("test in");
    let handle = inputs
        .get_input_producer_handle(port)
        .expect("the port we just created has a producer");

    assert!(
        handle.push(
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
            Instant::now() - AGE
        ),
        "the fifo has room for one event"
    );

    // The drain hands each event to a visitor rather than returning a slice, so
    // nothing borrows into the audio-thread-only scratch.
    let mut seen: Vec<MidiEvent> = Vec::new();
    inputs.cycle_start_read_all_inputs(NFRAMES, |_port, event| seen.push(event));
    assert_eq!(seen.len(), 1, "exactly the event we pushed");
    seen[0].frame_offset
}

/// The rate the device reports is the rate the offset is computed against.
///
/// At 48 kHz, 1 ms is 48 samples, so the event belongs 48 frames before the end
/// of the block: 1024 - 48 = 976. The assertion band deliberately **excludes
/// 980**, the answer the stale 44100 default gives — a test that accepted both
/// would pass with the bug in place.
#[test]
fn an_inbound_event_is_timed_against_the_device_sample_rate() {
    let offset = offset_at(Some(48_000.0));
    assert!(
        (970..=978).contains(&offset),
        "1 ms before a 1024-frame block at 48 kHz is ~976, got {offset} \
         (980 would mean the 44100 default is still in force)"
    );
}

/// The placeholder rate mistimes the same event, and this pins the exact wrong
/// answer.
///
/// Without this, someone changing `CycleScratch::new`'s default would silently
/// widen the band the test above tolerates rather than failing anything. Here
/// the wrong value is named, so the default cannot drift unnoticed.
#[test]
fn the_default_rate_mistimes_an_event() {
    let offset = offset_at(None);
    assert!(
        (976..=984).contains(&offset),
        "1 ms at the 44100 placeholder is ~980, got {offset}"
    );
}
