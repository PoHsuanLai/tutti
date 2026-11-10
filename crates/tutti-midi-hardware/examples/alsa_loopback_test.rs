//! Linux loopback over an ALSA virtual-MIDI port.
//!
//! Sends UMP out through one endpoint and reads it back through another,
//! checking the message survived the round trip.
//!
//! **`Midi Through`, not VirMIDI.** A virmidi port does *not* echo: writing to
//! it feeds the raw-MIDI device, not that port's own subscribers, so a
//! send/receive pair on one virmidi port receives nothing. `Midi Through` is the
//! kernel's loopback and does exactly what its name says. Verified against a
//! standalone C probe: on virmidi the same pairing sends fine (`rc=28`) and
//! never delivers, so a failure there is not this crate's.
//!
//! ```text
//! sudo modprobe snd-virmidi     # if /proc/asound/seq/clients has none
//! cargo run --example alsa_loopback_test
//! ```

use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tutti_types::SampleRate;

use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::Offset;
use tutti_midi_hardware::{HardwareMidiDrain, HardwareMidiInputs, MidiMessage, MidiSession};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

/// Pick the loopback endpoint. See the module doc for why this is not VirMIDI.
fn pick(list: &[tutti_midi_hardware::DeviceInfo]) -> Option<&tutti_midi_hardware::DeviceInfo> {
    list.iter().find(|e| e.name.contains("Midi Through"))
}

fn main() {
    let pm = Arc::new(HardwareMidiInputs::new(256, SampleRate::new(44_100.0)));
    // The audio side's drain: what a `MidiInputNode` would own.
    let mut drain = HardwareMidiDrain::new(Arc::clone(&pm));
    let io = MidiSession::open(pm.clone()).expect("open MIDI");

    let inputs = io.inputs();
    let outputs = io.outputs();
    let (Some(inp), Some(out)) = (pick(&inputs), pick(&outputs)) else {
        eprintln!("ERROR: no `Midi Through` port. `sudo modprobe snd-seq-dummy`.");
        std::process::exit(1);
    };
    println!("in : [{}] {}", inp.id, inp.name);
    println!("out: [{}] {}", out.id, out.name);

    io.connect_input(&inp.id).expect("connect input");
    io.connect_output(&out.id).expect("connect output");
    thread::sleep(Duration::from_millis(200));

    // Drain anything buffered before the test.
    drain.cycle_start_read_all_inputs(512, |_, _| {});

    let mut failures = 0usize;

    let mut roundtrip = |label: &str, sent: Ump, check: &dyn Fn(Ump) -> bool| {
        assert_eq!(
            io.send(&[(Offset::ZERO, sent)]),
            1,
            "the session accepted the event"
        );
        thread::sleep(Duration::from_millis(150));
        let mut first = None;
        drain.cycle_start_read_all_inputs(512, |_, (_, ev)| {
            if first.is_none() {
                first = Some(ev);
            }
        });
        match first {
            Some(ev) if check(ev) => println!("  PASS [{label}]: {:?}", MidiMessage::from_ump(ev)),
            Some(ev) => {
                failures += 1;
                println!(
                    "  FAIL [{label}]: unexpected {:?}",
                    MidiMessage::from_ump(ev)
                );
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
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        &|ev| MidiMessage::from_ump(ev).is_note_on(),
    );

    println!("\n=== Test 2: Note Off ===");
    roundtrip(
        "note-off",
        note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000),
        &|ev| MidiMessage::from_ump(ev).is_note_off(),
    );

    println!("\n=== Test 3: Burst (10 notes) ===");
    for n in 60..70u8 {
        io.send(&[(
            Offset::ZERO,
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xA000),
        )]);
    }
    thread::sleep(Duration::from_millis(250));
    let mut count = 0usize;
    let mut all_on = true;
    drain.cycle_start_read_all_inputs(512, |_, (_, ev)| {
        count += 1;
        all_on &= MidiMessage::from_ump(ev).is_note_on();
    });
    if count == 10 && all_on {
        println!("  PASS: received 10/10, all note_on");
    } else {
        failures += 1;
        println!("  FAIL: received {count}/10, all note_on={all_on}");
    }

    io.disconnect_input(&inp.id);
    io.disconnect_output();

    if failures == 0 {
        println!("\nAll tests complete!");
    } else {
        println!("\n{failures} test(s) FAILED");
        std::process::exit(1);
    }
}
