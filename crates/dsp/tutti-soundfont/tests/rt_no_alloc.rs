//! Regression gate: the unit's `Node::process` must not allocate, at any
//! block length.
//!
//! A block walks a variable number of render segments (one per distinct
//! event offset), so the likely regressions are shaped like allocation — collecting the segment boundaries into a `Vec`,
//! growing the scratch buffers to `size` on the audio thread, or re-sizing
//! `midi_buffer` when a block carries more events than usual.
//!
//! What the unit owns, and where each is sized:
//! - `left_buffer` / `right_buffer` — sized in `prepare` to the prepared
//!   `MaxBlock` and never resized on the audio thread (no block is longer).
//! - `midi_buffer` — `MIDI_BUFFER_CAPACITY` events, fully initialised in `new`
//!   because the event input is gathered into existing slots.
//!
//! Driven by hand through `support::Hand` (`tutti_graph::contract::Direct`
//! underneath), prepared outside every gate as a graph prepares a node.
//! - the rustysynth `Synthesizer` — its voice blocks, chorus and reverb lines
//!   are all sized from `block_size` at construction.
//!
//! Note the fixture dependency: these run against the committed `TimGM6mb.sf2`,
//! and construction (which does allocate, heavily) is deliberately outside
//! every gate.

use std::path::PathBuf;
use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
mod support;

use support::Hand;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Load the committed test SoundFont, failing loudly if the checkout lacks it.
fn load_test_soundfont() -> Arc<SoundFont> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("assets/soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("test soundfont missing at {}: {e}", path.display()));
    Arc::new(
        SoundFont::new(&mut file)
            .unwrap_or_else(|e| panic!("test soundfont at {} is malformed: {e}", path.display())),
    )
}

/// A unit at 44.1 kHz, prepared (outside any gate) and driven by hand.
fn unit() -> Hand {
    let settings = SynthesizerSettings::new(44_100);
    Hand::new(SoundFontUnit::new(load_test_soundfont(), &settings).expect("create SoundFontUnit"))
}

fn note_on(key: u8, offset: u32) -> MidiEvent {
    MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, key, 100)
        .with_frame_offset(offset)
}

fn note_off(key: u8, offset: u32) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, key, 0).with_frame_offset(offset)
}

#[test]
fn soundfont_process_idle_is_allocation_free() {
    let mut unit = unit();

    for _ in 0..16 {
        unit.block(64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            unit.block(64);
        }
    });
}

#[test]
fn soundfont_process_with_active_voices_is_allocation_free() {
    let mut unit = unit();
    unit.queue_midi(&[note_on(60, 0), note_on(64, 0), note_on(67, 0)]);

    for _ in 0..32 {
        unit.block(64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            unit.block(64);
        }
    });
}

/// Several events at distinct offsets in one block, so `process` splits the
/// render into multiple segments.
///
/// A regression that collected segment
/// boundaries into a `Vec`, or that allocated a scratch slice per segment,
/// lands here and nowhere else — the two tests above drive one segment per
/// block and would stay green.
#[test]
fn soundfont_process_with_events_inside_block_is_allocation_free() {
    let mut unit = unit();

    // Warm up every lazily-sized buffer at full occupancy first.
    unit.queue_midi(&[note_on(48, 0), note_on(60, 16), note_on(72, 48)]);
    for _ in 0..64 {
        unit.block(64);
    }
    unit.queue_midi(&[note_off(48, 0), note_off(60, 0), note_off(72, 0)]);
    for _ in 0..256 {
        unit.block(64);
    }

    // Steady churn with events spread across the block, including frame 0 and
    // the last frame — the two boundary segments of the split loop.
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            if i % 2 == 0 {
                unit.queue_midi(&[note_on(60, 0), note_on(64, 24), note_on(67, 63)]);
            } else {
                unit.queue_midi(&[note_off(60, 0), note_off(64, 24), note_off(67, 63)]);
            }
            unit.block(64);
        }
    });
}

/// Many events in one block — every frame carries one, so the split loop runs
/// its maximum number of segments for a 64-frame block.
///
/// The worst case for the segment walk. `midi_buffer` holds 256 events and this
/// queues 64 per block, so it also exercises the poll + sort without resizing.
#[test]
fn soundfont_process_with_an_event_every_frame_is_allocation_free() {
    let mut unit = unit();

    // One event per frame of the block, alternating on and off across keys so
    // the voice collection churns rather than retriggering one slot.
    let on: Vec<MidiEvent> = (0..64u32)
        .map(|f| note_on(36 + (f % 24) as u8, f))
        .collect();
    let off: Vec<MidiEvent> = (0..64u32)
        .map(|f| note_off(36 + (f % 24) as u8, f))
        .collect();

    for _ in 0..8 {
        unit.queue_midi(&on);
        for _ in 0..16 {
            unit.block(64);
        }
        unit.queue_midi(&off);
        for _ in 0..64 {
            unit.block(64);
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            unit.queue_midi(&on);
            for _ in 0..8 {
                unit.block(64);
            }
            unit.queue_midi(&off);
            for _ in 0..32 {
                unit.block(64);
            }
        }
    });
}

/// One-frame blocks: a graph may
/// hand the node a block of one frame.
#[test]
fn soundfont_one_frame_blocks_are_allocation_free() {
    let mut unit = unit();
    unit.queue_midi(&[note_on(60, 0), note_on(64, 0), note_on(67, 0)]);

    for _ in 0..256 {
        unit.tick();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            unit.tick();
        }
    });
}
