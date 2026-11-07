//! List this machine's MIDI devices and what each can carry, then watch for
//! devices coming and going.
//!
//! Prints, per device, its id, its name and the protocol the OS says it speaks.
//! Expect `Midi1` for most of it: most hardware in the world is still MIDI 1.0,
//! and only a `Midi2` device carries per-note controllers and JR Timestamps to
//! the wire. Then it waits ten seconds and prints each change.
//!
//! No flag is needed: this crate has no cargo features.
//!
//! ```text
//! cargo run --example list_devices
//! ```

use std::time::{Duration, Instant};

use tutti_midi_hardware::{Devices, Error};

fn main() -> Result<(), Error> {
    let devices = Devices::open()?;

    println!("=== MIDI devices ===");
    let list = devices.list();
    if list.is_empty() {
        println!("  (none found)");
    }
    for d in &list {
        let protocol = devices.capability(&d.id).map(|c| c.protocol);
        println!(
            "  {:<48} {:<32} {:?} {protocol:?}",
            d.id, d.name, d.direction
        );
    }

    println!("\nwatching for 10 s; plug or unplug a device");
    let until = Instant::now() + Duration::from_secs(10);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        for event in devices.wait(left) {
            println!("  {event:?}");
        }
    }
    Ok(())
}
