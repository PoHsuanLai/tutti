//! Regression gate: `SoundFontUnit::process` and `tick` must not allocate.
//!
//! This crate had no such gate before the frame-offset fix, which is what made
//! the fix worth gating: `process` now walks a variable number of render
//! segments per block rather than one fixed refill, so the obvious regressions
//! are shaped like allocation — collecting the segment boundaries into a `Vec`,
//! growing the scratch buffers to `size` on the audio thread, or re-sizing
//! `midi_buffer` when a block carries more events than usual.
//!
//! What the unit owns, and where each is sized:
//! - `left_buffer` / `right_buffer` — `MAX_BUFFER_SIZE` frames each, allocated
//!   in `new` and never resized. `process` clamps `size` into them rather than
//!   growing.
//! - `midi_buffer` — `MIDI_BUFFER_CAPACITY` events, fully initialised in `new`
//!   because `poll_into` iterates existing slots.
//! - the rustysynth `Synthesizer` — its voice blocks, chorus and reverb lines
//!   are all sized from `block_size` at construction.
//!
//! Note the fixture dependency: these run against the committed `TimGM6mb.sf2`,
//! and construction (which does allocate, heavily) is deliberately outside
//! every gate.

use std::path::PathBuf;
use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec};
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

fn unit() -> SoundFontUnit {
    let settings = SynthesizerSettings::new(44_100);
    SoundFontUnit::new(load_test_soundfont(), &settings).expect("create SoundFontUnit")
}

fn note_on(key: u8, offset: u32) -> MidiEvent {
    MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, key, 100)
        .with_frame_offset(offset)
}

fn note_off(key: u8, offset: u32) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, key, 0)
        .with_frame_offset(offset)
}

#[test]
fn soundfont_process_idle_is_allocation_free() {
    let mut unit = unit();
    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            unit.process(64, &input, &mut output);
        }
    });
}

#[test]
fn soundfont_process_with_active_voices_is_allocation_free() {
    let mut unit = unit();
    unit.midi_sender()
        .queue(&[note_on(60, 0), note_on(64, 0), note_on(67, 0)]);

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..32 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            unit.process(64, &input, &mut output);
        }
    });
}

/// The path the frame-offset fix added: several events at distinct offsets in
/// one block, so `process` splits the render into multiple segments.
///
/// This is the test the fix is gated on. A regression that collected segment
/// boundaries into a `Vec`, or that allocated a scratch slice per segment,
/// lands here and nowhere else — the two tests above drive one segment per
/// block and would stay green.
#[test]
fn soundfont_process_with_events_inside_block_is_allocation_free() {
    let mut unit = unit();
    let sender = unit.midi_sender();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm up every lazily-sized buffer at full occupancy first.
    sender.queue(&[note_on(48, 0), note_on(60, 16), note_on(72, 48)]);
    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);
    }
    sender.queue(&[note_off(48, 0), note_off(60, 0), note_off(72, 0)]);
    for _ in 0..256 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);
    }

    // Steady churn with events spread across the block, including frame 0 and
    // the last frame — the two boundary segments of the split loop.
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            if i % 2 == 0 {
                sender.queue(&[note_on(60, 0), note_on(64, 24), note_on(67, 63)]);
            } else {
                sender.queue(&[note_off(60, 0), note_off(64, 24), note_off(67, 63)]);
            }
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            unit.process(64, &input, &mut output);
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
    let sender = unit.midi_sender();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // One event per frame of the block, alternating on and off across keys so
    // the voice collection churns rather than retriggering one slot.
    let on: Vec<MidiEvent> = (0..64u32)
        .map(|f| note_on(36 + (f % 24) as u8, f))
        .collect();
    let off: Vec<MidiEvent> = (0..64u32)
        .map(|f| note_off(36 + (f % 24) as u8, f))
        .collect();

    for _ in 0..8 {
        sender.queue(&on);
        for _ in 0..16 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            unit.process(64, &input, &mut output);
        }
        sender.queue(&off);
        for _ in 0..64 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            unit.process(64, &input, &mut output);
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            sender.queue(&on);
            for _ in 0..8 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                unit.process(64, &input, &mut output);
            }
            sender.queue(&off);
            for _ in 0..32 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                unit.process(64, &input, &mut output);
            }
        }
    });
}

/// `tick` is the per-sample path — one frame per call, polled at block size 1.
#[test]
fn soundfont_tick_is_allocation_free() {
    let mut unit = unit();
    unit.midi_sender()
        .queue(&[note_on(60, 0), note_on(64, 0), note_on(67, 0)]);

    let mut output = [0.0f32; 2];
    for _ in 0..256 {
        unit.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            unit.tick(&[], &mut output);
        }
    });
}
