//! # 17 - MIDI Hardware
//!
//! Enumerate MIDI input devices and connect to hardware for receiving events.
//!
//! **Concepts:** Device enumeration, `MidiIo::connect_input_by_name`, `midi-hardware` feature
//!
//! ```bash
//! cargo run --example 17_midi_hardware --features midi,midi-hardware
//! ```

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().midi().build()?;
    let midi_io = engine
        .midi_io
        .as_ref()
        .expect("midi() was called on the builder");

    let devices = midi_io.list_input_devices();
    println!("MIDI input devices:");
    if devices.is_empty() {
        println!("  (none found — connect a MIDI controller and try again)");
        return Ok(());
    }
    for dev in &devices {
        println!("  [{}] {}", dev.index, dev.name);
    }

    let device_name = devices[0].name.clone();
    println!("\nConnecting to: {device_name}");
    midi_io.connect_input_by_name(&device_name)?;

    // Create a simple sine synth to hear alongside hardware MIDI activity.
    let osc = sine_hz::<f32>(440.0) * 0.3;
    engine.graph.master(osc);
    engine.graph.commit();

    engine.transport.play();
    println!("Listening for MIDI input for 10 seconds...");
    println!("(Play notes on your MIDI controller)");
    std::thread::sleep(Duration::from_secs(10));

    midi_io.disconnect_input(&device_name);
    println!("Disconnected.");

    Ok(())
}
