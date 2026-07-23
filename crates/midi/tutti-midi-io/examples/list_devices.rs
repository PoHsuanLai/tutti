use std::sync::Arc;
use tutti_midi_io::{MidiIo, HardwareMidiInputs};

fn main() {
    let io = MidiIo::new(Arc::new(HardwareMidiInputs::new(256)));

    println!("=== MIDI Input Devices ===");
    let inputs = io.list_input_devices();
    if inputs.is_empty() {
        println!("  (none found)");
    }
    for dev in &inputs {
        println!("  [{}] {}", dev.index, dev.name);
    }

    println!("\n=== MIDI Output Devices ===");
    let outputs = io.list_output_devices();
    if outputs.is_empty() {
        println!("  (none found)");
    }
    for dev in &outputs {
        println!("  [{}] {}", dev.index, dev.name);
    }
}
