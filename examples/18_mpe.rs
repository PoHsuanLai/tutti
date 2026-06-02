//! # 18 - MPE (MIDI Polyphonic Expression)
//!
//! Configure MPE zones and read per-note expression data (pitch bend, pressure, slide).
//!
//! **Concepts:** `MpeMode`, `MpeZoneConfig`, per-note expression, channel allocation
//!
//! ```bash
//! cargo run --example 18_mpe --features midi,mpe
//! ```
//!
//! TODO: This example needs rework — MPE processor integration moved from
//! tutti-midi-io to the engine layer. The MpeProcessor is in tutti-core;
//! wiring it to the engine's `midi_io` field is a follow-up task.

use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let engine = TuttiEngine::builder().midi().build()?;
    let midi_io = engine
        .midi_io
        .as_ref()
        .expect("midi() was called on the builder");

    println!("Input devices:  {:?}", midi_io.list_input_devices());
    println!("Output devices: {:?}", midi_io.list_output_devices());

    println!("\nMPE example temporarily simplified during midi-io redesign.");

    Ok(())
}
