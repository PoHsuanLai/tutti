//! The sample-accuracy contract (doc 013 §6) for the synth as a graph
//! node: a note-on at offset `k` of the event input sounds from frame `k`
//! (plus the compiled arrival), exactly, on every path the harness runs —
//! direct, behind PDC, through an event fan-in, across recompiles, in ragged
//! blocks, and delivered by a scheduled `At::Frame` or `At::Beat` command.
//!
//! A square oscillator with an instant attack. Its note's first sample is
//! exactly zero (the voice's lanes ramp from zero), so the response is found
//! one frame after its onset: a constant lead of one frame
//! (`Row::with_lead`), the same on every path.
//!
//! Mutations (run): `gather_events` writing every event at offset 0 (the
//! `frame_offset: e.offset.get()` replaced by `0`) → the note sounds at its
//! block's start → the paths that see a non-zero offset fail; the row without its
//! lead → every path fails one frame late.

use tutti_core::Samples;
use tutti_graph::contract::{Detect, Excite, Row, SAMPLE_RATE};
use tutti_graph::{EventKind, Ump};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_polysynth::{EnvelopeConfig, OscillatorType, PolySynth, SynthConfig};

fn synth_row() -> Row {
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 69, 0xFFFF);
    Row::new(
        "PolySynth",
        || {
            Box::new(
                PolySynth::new(SynthConfig {
                    sample_rate: SAMPLE_RATE,
                    oscillator: OscillatorType::Square { pulse_width: 0.5 },
                    envelope: EnvelopeConfig {
                        attack: tutti_core::Seconds(0.0),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .expect("the synth builds"),
            )
        },
        Excite::Event {
            port: 0,
            kind: EventKind::Midi(Ump(note.data)),
        },
        Detect::Threshold(1e-6),
    )
    .with_lead(Samples(1))
}

tutti_graph::contract_tests!(event polysynth => synth_row());
