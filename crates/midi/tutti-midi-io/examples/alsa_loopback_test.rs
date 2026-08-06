//! Linux loopback over an ALSA virtual-MIDI port.
//!
//! Sends UMP out through one endpoint and reads it back through another,
//! checking the message survived the round trip.
//!
//! **`Midi Through`, not VirMIDI.** A virmidi port does *not* echo: writing to
//! it feeds the raw-MIDI device, not that port's own subscribers, so a
//! send/receive pair on one virmidi port receives nothing. `Midi Through` is the
//! kernel's loopback and does exactly what its name says. Verified against a
//! standalone C probe before this example was written — the same pairing on
//! virmidi sends fine (`rc=28`) and never delivers.
//!
//! ```text
//! sudo modprobe snd-virmidi     # if /proc/asound/seq/clients has none
//! cargo run --example alsa_loopback_test
//! ```

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tutti_midi_io::{HardwareMidiInputs, MidiEvent, MidiSession};
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

/// Pick the loopback endpoint. See the module doc for why this is not VirMIDI.
fn pick(list: &[tutti_midi_io::EndpointInfo]) -> Option<&tutti_midi_io::EndpointInfo> {
    list.iter().find(|e| e.name.contains("Midi Through"))
}

fn main() {
    let pm = Arc::new(HardwareMidiInputs::new(256));
    let io = MidiSession::new(pm.clone());

    let inputs = io.inputs();
    let outputs = io.outputs();
    let (Some(inp), Some(out)) = (pick(&inputs), pick(&outputs)) else {
        eprintln!("ERROR: no `Midi Through` port. `sudo modprobe snd-seq-dummy`.");
        std::process::exit(1);
    };
    println!("in : [{}] {}", inp.id.raw(), inp.name);
    println!("out: [{}] {}", out.id.raw(), out.name);

    io.connect_input(inp.id).expect("connect input");
    io.connect_output(out.id).expect("connect output");
    thread::sleep(Duration::from_millis(200));

    // Drain anything buffered before the test.
    pm.cycle_start_read_all_inputs(512, |_, _| {});

    let mut failures = 0usize;

    let mut roundtrip = |label: &str, sent: MidiEvent, check: &dyn Fn(MidiEvent) -> bool| {
        assert_eq!(io.send(&[sent]), 1, "the session accepted the event");
        thread::sleep(Duration::from_millis(150));
        let mut first = None;
        pm.cycle_start_read_all_inputs(512, |_, ev| {
            if first.is_none() {
                first = Some(ev);
            }
        });
        match first {
            Some(ev) if check(ev) => println!("  PASS [{label}]: {:?}", ev.message()),
            Some(ev) => {
                failures += 1;
                println!("  FAIL [{label}]: unexpected {:?}", ev.message());
            }
            None => {
                failures += 1;
                println!("  FAIL [{label}]: nothing received");
            }
        }
    };

    println!("\n=== Test 1: Note On ===");
    roundtrip(
        "note-on",
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        &|ev| ev.message().is_note_on(),
    );

    println!("\n=== Test 2: Note Off ===");
    roundtrip(
        "note-off",
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000),
        &|ev| ev.message().is_note_off(),
    );

    println!("\n=== Test 3: Burst (10 notes) ===");
    for n in 60..70u8 {
        io.send(&[MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            n,
            0xA000,
        )]);
    }
    thread::sleep(Duration::from_millis(250));
    let mut count = 0usize;
    let mut all_on = true;
    pm.cycle_start_read_all_inputs(512, |_, ev| {
        count += 1;
        all_on &= ev.message().is_note_on();
    });
    if count == 10 && all_on {
        println!("  PASS: received 10/10, all note_on");
    } else {
        failures += 1;
        println!("  FAIL: received {count}/10, all note_on={all_on}");
    }

    io.disconnect_input(inp.id);
    io.disconnect_output();

    if failures == 0 {
        println!("\nAll tests complete!");
    } else {
        println!("\n{failures} test(s) FAILED");
        std::process::exit(1);
    }
}
