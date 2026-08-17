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
//! voices, and mix into the output. A regression that grows `midi_buffer`
//! at runtime, or that frees the finished-indices buffer on the audio
//! thread, would land here.
//!
//! Note that most tests below run at `max_voices: 8` — half the inline
//! capacity — so they exercise the steady state but *not* the collection's
//! worst case. `polysynth_all_voices_finishing_together_is_allocation_free`
//! is the one that fills it; `max_voices` past the inline capacity is
//! refused by the constructor and covered separately.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, Hz, SampleRate, Q};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::new(channel),
        note,
        midi1_velocity_to_midi2(vel),
    )
}

fn note_off(channel: u8, note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(channel), note, 0)
}

#[test]
fn polysynth_process_idle_is_allocation_free() {
    // No active voices — process should still tick through allocator
    // bookkeeping and the MIDI inbox poll.
    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
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
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 8,
        oscillator: OscillatorType::Saw,
        filter: FilterType::Svf {
            cutoff: Hz(2_000.0),
            q: Q(0.707),
            mode: tutti_polysynth::SvfMode::Lowpass,
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
        sample_rate: tutti_core::SampleRate::from(48_000.0),
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
        sample_rate: tutti_core::SampleRate::from(48_000.0),
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
    sender.queue(&[
        note_on(0, 48, 100),
        note_on(0, 60, 100),
        note_on(0, 72, 100),
    ]);
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

/// The case the tests above never reached: **every** voice finishing in the
/// same block, at the maximum `max_voices` the constructor accepts.
///
/// `finished_indices` collects one entry per voice that finished, so this
/// fills it exactly to its inline capacity. The other tests in this file all
/// run at `max_voices: 8`, half of it.
///
/// Note what this test can and cannot prove. Because the constructor caps
/// `max_voices` at the inline capacity, the collection cannot spill, so this
/// passes whether the drain indexes or uses `mem::take` — at 16-of-16 a take
/// swaps two inline buffers and never touches the heap. It guards the
/// *steady state* at full occupancy. The allocation hazard `mem::take`
/// carries is only reachable if the cap is raised without also making the
/// buffer bigger, which
/// `polysynth_rejects_max_voices_past_inline_capacity` is what forecloses.
/// (Verified by construction: removing the cap and running this at 24 voices
/// against a `mem::take` drain aborts inside the no-alloc gate.)
#[test]
fn polysynth_all_voices_finishing_together_is_allocation_free() {
    const MAX_VOICES: usize = 16;

    let mut synth = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: MAX_VOICES,
        oscillator: OscillatorType::Saw,
        ..Default::default()
    })
    .unwrap();
    synth.set_sample_rate(SampleRate(48_000.0));
    let sender = synth.midi_sender();

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    // Distinct pitches so the allocator assigns a separate voice to each
    // rather than retriggering one slot.
    let all_on: Vec<MidiEvent> = (0..MAX_VOICES)
        .map(|i| note_on(0, 36 + i as u8, 100))
        .collect();
    let all_off: Vec<MidiEvent> = (0..MAX_VOICES).map(|i| note_off(0, 36 + i as u8)).collect();

    // Warm up: run the full on/off cycle once outside the gate so every
    // lazily-sized buffer reaches its steady-state capacity first.
    for _ in 0..4 {
        sender.queue(&all_on);
        for _ in 0..32 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
        sender.queue(&all_off);
        for _ in 0..256 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            synth.process(64, &input, &mut output);
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..32 {
            sender.queue(&all_on);
            for _ in 0..32 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                synth.process(64, &input, &mut output);
            }
            // All 16 releases land together, so a single block collects
            // `MAX_VOICES` finished indices.
            sender.queue(&all_off);
            for _ in 0..256 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                synth.process(64, &input, &mut output);
            }
        }
    });
}

/// `max_voices` above the inline capacity is refused at construction rather
/// than silently spilling `finished_indices` onto the heap per block.
#[test]
fn polysynth_rejects_max_voices_past_inline_capacity() {
    let result = PolySynth::new(SynthConfig {
        sample_rate: tutti_core::SampleRate::from(48_000.0),
        max_voices: 17,
        ..Default::default()
    });
    assert!(
        result.is_err(),
        "max_voices past FINISHED_NOTES_CAPACITY must be rejected, not spilled"
    );
}
