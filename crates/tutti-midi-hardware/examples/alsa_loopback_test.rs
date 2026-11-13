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

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod common;
use common::Listener;

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_midi_hardware::{DeviceInlets, MidiSession};
use tutti_midi_protocol::channel_voice;
use tutti_ump::{KeyEvent, MidiChannel, Ump};

/// Whether `ev` starts a note, in either protocol.
fn is_note_on(ev: Ump) -> bool {
    matches!(ev.key_event(), Some(KeyEvent::On(_)))
}

/// Pick the loopback endpoint. See the module doc for why this is not VirMIDI.
fn pick(list: &[tutti_midi_hardware::DeviceInfo]) -> Option<&tutti_midi_hardware::DeviceInfo> {
    list.iter().find(|e| e.name.contains("Midi Through"))
}

fn main() {
    // One inlet slot, 256 events, each event placed 20 ms after it arrived.
    let inlets = DeviceInlets::new(
        NonZeroUsize::MIN,
        tutti_graph::EventCapacity::of(256),
        Duration::from_millis(20),
    );
    // The graph's side: the engine's part, played on the wall clock.
    let mut graph = Listener::over(&inlets);
    let mut io = MidiSession::open(Arc::clone(&inlets)).expect("open MIDI");

    let inputs = io.inputs();
    let outputs = io.outputs();
    let (Some(inp), Some(out)) = (pick(&inputs), pick(&outputs)) else {
        eprintln!("ERROR: no `Midi Through` port. `sudo modprobe snd-seq-dummy`.");
        std::process::exit(1);
    };
    println!("in : [{}] {}", inp.id, inp.name);
    println!("out: [{}] {}", out.id, out.name);

    io.connect_input(&inp.id).expect("connect input");
    graph.apply();
    let mut sink = io.connect_output(&out.id).expect("connect output");
    thread::sleep(Duration::from_millis(200));

    // Whatever was buffered before the test.
    graph.listen(Duration::from_millis(50));

    let mut failures = 0usize;

    let mut roundtrip = |label: &str, sent: Ump, check: &dyn Fn(Ump) -> bool| {
        assert_eq!(
            sink.queue(&[(Offset::ZERO, sent)]),
            1,
            "the session accepted the event"
        );
        let first = graph.listen(Duration::from_millis(150)).first().copied();
        match first {
            Some(ev) if check(ev) => println!("  PASS [{label}]: {:?}", channel_voice(ev)),
            Some(ev) => {
                failures += 1;
                println!("  FAIL [{label}]: unexpected {:?}", channel_voice(ev));
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
        midi::note_on(MidiChannel::FIRST, 60, 0x8000),
        &is_note_on,
    );

    println!("\n=== Test 2: Note Off ===");
    roundtrip(
        "note-off",
        midi::note_off(MidiChannel::FIRST, 60, 0x4000),
        &|ev| ev.is_note_off(),
    );

    println!("\n=== Test 3: Burst (10 notes) ===");
    for n in 60..70u8 {
        sink.queue(&[(Offset::ZERO, midi::note_on(MidiChannel::FIRST, n, 0xA000))]);
    }
    let heard = graph.listen(Duration::from_millis(250));
    let count = heard.len();
    let all_on = heard.iter().all(|&ev| is_note_on(ev));
    if count == 10 && all_on {
        println!("  PASS: received 10/10, all note_on");
    } else {
        failures += 1;
        println!("  FAIL: received {count}/10, all note_on={all_on}");
    }

    io.disconnect_input(&inp.id);
    drop(sink);

    if failures == 0 {
        println!("\nAll tests complete!");
    } else {
        println!("\n{failures} test(s) FAILED");
        std::process::exit(1);
    }
}
