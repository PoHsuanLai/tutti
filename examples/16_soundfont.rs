//! # 16 - SoundFont
//!
//! Load and play SoundFont (.sf2) instruments with MIDI.
//!
//! **Concepts:** `tutti::sf2(&engine.soundfont, ...)`, `MidiSender::note_on/off`, `Note`
//!
//! ```bash
//! cargo run --example 16_soundfont --features soundfont
//! ```

use std::time::Duration;
use tutti::prelude::*;
use tutti_midi_types::Note;

fn main() -> tutti::Result<()> {
    let soundfont_path = std::env::var("SOUNDFONT_PATH")
        .unwrap_or_else(|_| "assets/soundfonts/TimGM6mb.sf2".to_string());

    if !std::path::Path::new(&soundfont_path).exists() {
        println!("SoundFont not found: {}", soundfont_path);
        println!("Set SOUNDFONT_PATH or run: cd assets/soundfonts && ./download-timgm6mb.sh");
        return Ok(());
    }

    let mut engine = TuttiEngine::builder().outputs(2).build()?;

    let piano = tutti::sf2(&engine.soundfont, &soundfont_path)
        .preset(0)
        .build()?;

    // Grab the sender before we move the unit into the graph.
    let midi = piano.midi_sender();

    engine.graph.master(piano);
    engine.graph.commit();

    engine.transport.play();
    println!("Playing melody...");

    let melody = [
        (Note::C4, 500),
        (Note::D4, 500),
        (Note::E4, 500),
        (Note::F4, 500),
        (Note::G4, 500),
        (Note::A4, 500),
        (Note::G4, 500),
        (Note::E4, 500),
        (Note::C4, 1000),
    ];

    for (note, duration_ms) in melody {
        midi.note_on(0, note.into(), 100);
        std::thread::sleep(Duration::from_millis(duration_ms - 50));
        midi.note_off(0, note.into());
        std::thread::sleep(Duration::from_millis(50));
    }

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
