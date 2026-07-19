//! Regression gate: `PolySynth::process` must not allocate.
//!
//! PolySynth is the umbrella audio-callback unit for the in-tree
//! polyphonic synth. It owns:
//! - a `Vec<SynthVoice>` (allocated once on build),
//! - a `SmallVec<[usize; 16]>` of finished-voice indices (inline),
//! - a per-process `midi_buffer` for sorted events,
//! - and a `mix_buffer` scalar pair.
//!
//! `tick` and `process` both pull MIDI off the inbox, iterate active
//! voices, and mix into the output. A regression that pushes a voice
//! beyond the SmallVec inline capacity or grows `midi_buffer` at
//! runtime would land here.

#![cfg(feature = "midi")]

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, SampleRate};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_synth::{FilterType, OscillatorType, PolySynth, SynthConfig};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_on(0, channel, note, midi1_velocity_to_midi2(vel))
}

fn note_off(channel: u8, note: u8) -> MidiEvent {
    MidiEvent::note_off(0, channel, note, 0)
}

#[test]
fn polysynth_process_idle_is_allocation_free() {
    // No active voices — process should still tick through allocator
    // bookkeeping and the MIDI inbox poll.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: 48_000.0,
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    });
}

#[test]
fn polysynth_process_with_active_voices_is_allocation_free() {
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: 48_000.0,
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Svf {
            cutoff: 2_000.0,
            q: 0.707,
            mode: tutti_synth::SvfMode::Lowpass,
        },
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));

    // Trigger 4 sustained voices before entering the gate.
    let sender = synth.midi_sender();
    sender.queue(&[
        note_on(0, 60, 100),
        note_on(0, 64, 100),
        note_on(0, 67, 100),
        note_on(0, 72, 100),
    ]);

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..32 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    });
}

#[test]
fn polysynth_tick_with_active_voices_is_allocation_free() {
    // `tick` is the per-sample path — drives `Voice::tick_stereo`
    // directly without MIDI sub-buffer splitting.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: 48_000.0,
        max_voices: 8,
        oscillator: OscillatorType::Triangle,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));

    synth
        .midi_sender()
        .queue(&[note_on(0, 60, 90), note_on(0, 64, 90), note_on(0, 67, 90)]);

    let mut output = [0.0f32; 2];
    for _ in 0..256 {
        synth.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            synth.tick(&[], &mut output);
        }
    });
}

#[test]
fn polysynth_process_with_midi_events_inside_block_is_allocation_free() {
    // Sub-buffer split path: queue events with non-zero frame offsets so
    // `process` walks the event-driven block boundaries.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: 48_000.0,
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));
    let sender = synth.midi_sender();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Warm up the voice pool + finished-indices SmallVec at full size.
    sender.queue(&[note_on(0, 48, 100), note_on(0, 60, 100), note_on(0, 72, 100)]);
    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }
    sender.queue(&[note_off(0, 48), note_off(0, 60), note_off(0, 72)]);
    for _ in 0..256 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        synth.process(64, &input, &mut output);
    }

    // Steady note-on/note-off churn inside the gate. Each iteration
    // routes events through the same code path that handles real DAW
    // playback: poll inbox → sort by frame offset → drive voices.
    let on = note_on(0, 60, 100);
    let off = note_off(0, 60);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            if i % 2 == 0 {
                sender.queue(&[on]);
            } else {
                sender.queue(&[off]);
            }
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    });
}
