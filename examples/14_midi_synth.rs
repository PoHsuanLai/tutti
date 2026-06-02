//! # 14 - MIDI Synth
//!
//! Basic polyphonic synthesis with multiple oscillators.
//!
//! **Concepts:** Polyphony, chord synthesis, `midi` feature
//!
//! ```bash
//! cargo run --example 14_midi_synth --features midi,synth
//! ```

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    // C major chord: C4, E4, G4
    let c = sine_hz::<f32>(261.63) * 0.2;
    let e = sine_hz::<f32>(329.63) * 0.2;
    let g = sine_hz::<f32>(392.00) * 0.2;
    engine.graph.master(c + e + g);
    engine.graph.commit();

    engine.transport.play();
    println!("Playing C major chord...");

    std::thread::sleep(Duration::from_secs(2));

    Ok(())
}
