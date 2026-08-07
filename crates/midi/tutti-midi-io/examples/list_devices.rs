//! Enumerate this machine's MIDI endpoints and what each can carry.
//!
//! ```text
//! cargo run --example list_devices --features midi-hardware
//! ```

use std::sync::Arc;
use tutti_midi_io::{HardwareMidiInputs, MidiSession};

fn main() {
    let session = MidiSession::new(Arc::new(HardwareMidiInputs::new(256)));

    for (label, endpoints) in [
        ("MIDI Input Endpoints", session.inputs()),
        ("MIDI Output Endpoints", session.outputs()),
    ] {
        println!("=== {label} ===");
        if endpoints.is_empty() {
            println!("  (none found)");
        }
        for e in &endpoints {
            // The protocol is what the OS reports for that endpoint, not the one
            // we open ports with — most hardware is still MIDI 1.0, and only a
            // MIDI-2.0 endpoint carries per-note controllers and JR Timestamps.
            println!(
                "  [{:>10}] {:<32} {:?}{}",
                e.id.raw(),
                e.name,
                e.capability.protocol,
                if e.capability.function_blocks.is_empty() {
                    String::new()
                } else {
                    format!("  ({} function blocks)", e.capability.function_blocks.len())
                }
            );
        }
        println!();
    }
}
