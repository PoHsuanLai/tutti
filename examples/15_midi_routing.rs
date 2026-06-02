//! # 15 - MIDI Routing
//!
//! Create a polyphonic synthesizer and push MIDI events to it via the
//! synth's `MidiSender` handle.
//!
//! **Concepts:** `SynthHandle`, `MidiSender::note_on/note_off`, `Note` enum, polyphony
//!
//! ```bash
//! cargo run --example 15_midi_routing --features "synth,midi"
//! ```

use std::time::Duration;
use tutti::prelude::*;
use tutti_midi_types::Note;
use tutti_synth::SynthHandle;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().midi().build()?;

    // Create a polyphonic saw synth with Moog filter.
    let synth = SynthHandle::new(engine.sample_rate)
        .saw()
        .poly(8)
        .filter_moog(2000.0, 0.5)
        .adsr(0.01, 0.1, 0.7, 0.3)
        .build()?;

    // Grab the sender before we move the synth into the graph.
    let midi = synth.midi_sender();

    engine.graph.master(synth);
    engine.graph.commit();

    engine.transport.play();
    println!("Playing arpeggio...");

    let arpeggio = [Note::C4, Note::E4, Note::G4, Note::C5, Note::G4, Note::E4];
    for note in arpeggio {
        midi.note_on(0, note.into(), 100);
        std::thread::sleep(Duration::from_millis(200));
        midi.note_off(0, note.into());
        std::thread::sleep(Duration::from_millis(50));
    }

    println!("Playing chord...");
    let chord = [Note::C4, Note::E4, Note::G4];
    for note in chord {
        midi.note_on(0, note.into(), 100);
    }
    std::thread::sleep(Duration::from_secs(2));
    for note in chord {
        midi.note_off(0, note.into());
    }
    std::thread::sleep(Duration::from_millis(500));

    Ok(())
}
