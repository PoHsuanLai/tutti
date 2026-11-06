//! Enumerate this machine's MIDI endpoints and what each can carry.
//!
//! Prints, per endpoint, the [`EndpointId`](tutti_midi_hardware::EndpointId)
//! raw value to open it by, the OS-reported name, and the protocol the OS says
//! it speaks. Expect `Midi1` for most of it: most hardware in the world is still
//! MIDI 1.0, and only a `Midi2` endpoint carries per-note controllers and JR
//! Timestamps to the wire.
//!
//! No flag is needed — this crate has no cargo features.
//!
//! ```text
//! cargo run --example list_devices
//! ```

use std::sync::Arc;
use tutti_midi_hardware::{HardwareMidiInputs, MidiSession};

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
            // ports are opened with — most hardware is still MIDI 1.0, and only
            // a MIDI-2.0 endpoint carries per-note controllers and JR
            // Timestamps.
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
