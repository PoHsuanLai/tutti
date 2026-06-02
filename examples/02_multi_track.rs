//! # 02 - Multi-Track Mixing
//!
//! Mix multiple audio sources with independent volume control.
//!
//! **Concepts:** Multiple nodes, volume control, summing into a mix bus
//!
//! ```bash
//! cargo run --example 02_multi_track
//! ```

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    // Sum the sources directly via fundsp's `+` operator and master the
    // resulting unit.
    let bass = sine_hz::<f64>(110.0) * 0.3;
    let melody = sine_hz::<f64>(440.0) * 0.2;
    let harmony = sine_hz::<f64>(550.0) * 0.15;
    let perc = pink::<f64>() * 0.1;

    engine.graph.master(bass + melody + harmony + perc);
    engine.graph.commit();

    engine.transport.play();
    println!("Playing multi-track mix...");
    std::thread::sleep(Duration::from_secs(5));

    Ok(())
}
