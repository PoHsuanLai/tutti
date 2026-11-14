//! Linux loopback over the kernel's `Midi Through` port.
//!
//! Sends UMP out through one endpoint and reads it back through another,
//! checking the message survives the round trip.
//!
//! **`Midi Through`, not VirMIDI.** A virmidi port does not echo: writing to
//! it feeds the raw-MIDI device, not that port's own subscribers, so a
//! send/receive pair on one virmidi port receives nothing. `Midi Through` is
//! the kernel's loopback.
//!
//! The suite needs the `snd-seq` module and a `Midi Through` client
//! (`sudo modprobe snd-seq-dummy`) and fails without them.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use super::listener::Listener;

use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_hardware::{DeviceInfo, DeviceInlets, MidiSession, OpenOutput};
use tutti_ump::{KeyEvent, MidiChannel, Ump};

/// Whether `ev` starts a note, in either protocol.
fn is_note_on(ev: Ump) -> bool {
    matches!(ev.key_event(), Some(KeyEvent::On(_)))
}

/// The loopback endpoint.
fn pick(list: &[DeviceInfo]) -> Option<&DeviceInfo> {
    list.iter().find(|e| e.name.contains("Midi Through"))
}

/// A session with `Midi Through` connected both ways, and the graph that
/// listens to it. The fields drop in order: the output, the graph, then the
/// session.
struct Loopback {
    sink: OpenOutput,
    graph: Listener,
    _io: MidiSession,
}

fn open() -> Loopback {
    // One inlet slot, 256 events, each placed 20 ms after it arrived.
    let inlets = DeviceInlets::new(
        NonZeroUsize::MIN,
        EventCapacity::of(256),
        Duration::from_millis(20),
    );
    let mut graph = Listener::over(&inlets);
    let mut io = MidiSession::open(Arc::clone(&inlets)).expect("open MIDI");
    let inputs = io.inputs();
    let outputs = io.outputs();
    let (Some(inp), Some(out)) = (pick(&inputs), pick(&outputs)) else {
        panic!("no `Midi Through` port: `sudo modprobe snd-seq-dummy`");
    };
    io.connect_input(&inp.id).expect("connect input");
    graph.apply();
    let sink = io.connect_output(&out.id).expect("connect output");
    std::thread::sleep(Duration::from_millis(200));
    // Whatever was buffered before the test.
    graph.listen(Duration::from_millis(50));
    Loopback {
        sink,
        graph,
        _io: io,
    }
}

/// One test, not three: `Midi Through` is a kernel-wide loopback, so tests
/// running in parallel processes would hear each other's notes.
#[test]
fn midi_through_returns_a_note_on_a_note_off_and_a_burst_of_ten_note_ons() {
    let mut lb = open();

    let on = note_on(MidiChannel::FIRST, 60, 0x8000);
    assert_eq!(lb.sink.queue(&[(Offset::ZERO, on)]), 1);
    let heard = lb.graph.listen(Duration::from_millis(150));
    assert_eq!(heard.len(), 1, "one event came back for the note on");
    assert!(is_note_on(heard[0]), "the event is a note on");

    let off = note_off(MidiChannel::FIRST, 60, 0x4000);
    assert_eq!(lb.sink.queue(&[(Offset::ZERO, off)]), 1);
    let heard = lb.graph.listen(Duration::from_millis(150));
    assert_eq!(heard.len(), 1, "one event came back for the note off");
    assert!(heard[0].is_note_off(), "the event is a note off");

    for n in 60..70u8 {
        lb.sink
            .queue(&[(Offset::ZERO, note_on(MidiChannel::FIRST, n, 0xA000))]);
    }
    let heard = lb.graph.listen(Duration::from_millis(250));
    assert_eq!(heard.len(), 10, "all ten of the burst came back");
    assert!(
        heard.iter().all(|&ev| is_note_on(ev)),
        "all ten are note ons"
    );
}
