//! # 19 - MIDI 2.0
//!
//! Create high-resolution MIDI 2.0 UMP events and convert between MIDI 1.0
//! and 2.0 using the `MidiEvent` (UMP) type and conversion helpers.
//!
//! **Concepts:** MidiEvent (UMP), 16-bit velocity, velocity conversion,
//! CC conversion, per-note pitch bend, MIDI 1.0 ↔ 2.0 interop
//!
//! ```bash
//! cargo run --example 19_midi2 --features midi
//! ```

use tutti::midi::MidiEvent;
use tutti_midi_types::convert::{
    midi1_cc_to_midi2, midi1_velocity_to_midi2, midi2_cc_to_midi1, midi2_velocity_to_midi1,
};

fn main() {
    // --- Note On with 16-bit velocity ---
    let note_on = MidiEvent::note_on(0, 0, 60, midi1_velocity_to_midi2(100));
    println!("MIDI 2.0 Note On (UMP):");
    println!("  Note:         {}", note_on.note().unwrap());
    println!("  Velocity u7:  {}", note_on.velocity_u7().unwrap());

    // --- Per-note pitch bend (MIDI 2.0 only) ---
    let bend = MidiEvent::per_note_pitch_bend(0, 0, 60, 0x8000_0000);
    println!(
        "\nPer-note pitch bend (raw): {:#010X}",
        bend.data_words()[1]
    );

    // --- High-resolution CC ---
    let cc_val_32 = midi1_cc_to_midi2(74);
    let cc = MidiEvent::cc(0, 0, 74, cc_val_32);
    println!("\nControl Change:");
    println!("  CC74 (32-bit):  {:#010X}", cc.data_words()[1]);
    println!("  CC74 back (7-bit): {}", midi2_cc_to_midi1(cc_val_32));

    // --- MIDI 1.0 → MIDI 2.0 velocity conversion ---
    println!("\n--- Velocity conversion ---");
    for v7 in [0u8, 1, 64, 100, 127] {
        let v16 = midi1_velocity_to_midi2(v7);
        let back = midi2_velocity_to_midi1(v16);
        println!("  7-bit {v7:>3} → 16-bit {v16:>5} → 7-bit {back:>3}");
    }

    // --- Round-trip via MIDI 1.0 bytes ---
    println!("\n--- MIDI 1.0 round-trip ---");
    let bytes = [0x90u8, 60, 100]; // Note On ch0, C4, vel 100
    if let Some(ev) = MidiEvent::from_midi1_bytes(0, &bytes) {
        println!("  Parsed note: {}", ev.note().unwrap());
        if let Some((out, _)) = ev.to_midi1_bytes() {
            println!("  Round-trip bytes: {out:02X?}");
        }
    }
}
