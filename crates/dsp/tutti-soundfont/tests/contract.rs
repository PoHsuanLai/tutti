//! The sample-accuracy contract (doc 013 §6) for the SoundFont player as a
//! native node: a note-on at offset `k` of the event input sounds where the
//! contract puts it, to the 8-frame resolution the node declares
//! (`Resolution::Frames(8)`, rustysynth's chunk), on every path the harness
//! runs.
//!
//! The note's first sample is not at its onset: the fixture's attack ramps
//! from exactly zero, so the response is found `LEAD` frames on — a constant
//! of the preset's DSP (`Row::with_lead`), the same on every path (41
//! frames: what `tests/soundfont_unit_audio.rs` measures at 44.1 kHz too).
//!
//! Mutations (run): the row without its lead → every path fails ("puts it at
//! 512", the response at 553); `gather_events` writing every event at offset 0
//! → the paths that see a non-zero offset (behind PDC, the recompiles) fail
//! past the 7-frame tolerance.

use std::sync::Arc;

use tutti_core::Samples;
use tutti_graph::contract::{Detect, Excite, Row, SAMPLE_RATE};
use tutti_graph::{EventKind, Ump};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

/// Frames from a note's onset to its first non-zero sample on the fixture's
/// preset 0 at 48 kHz.
const LEAD: usize = 41;

fn font() -> Arc<SoundFont> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../assets/soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
        panic!(
            "committed test soundfont missing at {}: {e}",
            path.display()
        )
    });
    Arc::new(SoundFont::new(&mut file).expect("the test soundfont parses"))
}

fn soundfont_row() -> Row {
    let font = font();
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
    Row::new(
        "SoundFontUnit",
        move || {
            let mut settings = SynthesizerSettings::new(SAMPLE_RATE.get() as i32);
            settings.enable_reverb_and_chorus = false;
            Box::new(SoundFontUnit::new(Arc::clone(&font), &settings).expect("the unit builds"))
        },
        Excite::Event {
            port: 0,
            kind: EventKind::Midi(Ump(note.data)),
        },
        Detect::Threshold(0.0),
    )
    .with_lead(Samples(LEAD))
}

tutti_graph::contract_tests!(event soundfont => soundfont_row());
