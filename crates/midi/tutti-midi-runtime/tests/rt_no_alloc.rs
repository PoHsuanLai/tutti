//! Regression gate for RT-safety: the MIDI border nodes process a block
//! without allocating, events flowing — a wire's through the input node's
//! translation onto its ports, a control thread's through the queue into the
//! out node's ring, and the clock's ticks.
//!
//! Mutation (run): a `Vec::new()` + push per block in `MidiInputNode::process`
//! → the gate panics.

use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Prepare, Transport};
use tutti_midi_runtime::{ClockNode, MidiInputNode, MidiMailbox, MidiOutNode, MidiQueueNode};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup, MidiIn};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(ch: u8, note: u8) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(ch), note, 0x8000)
}

/// A wire with a fresh note on four channels every poll.
struct Busy;

impl MidiIn for Busy {
    fn poll_block(&self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        for (i, slot) in buffer.iter_mut().take(4).enumerate() {
            *slot = note_on(i as u8 * 3, 60).with_frame_offset(40 - 10 * i as u32);
        }
        4
    }
}

#[test]
fn the_border_nodes_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(NodeKey(1), "in", MidiInputNode::new(Some(Arc::new(Busy))));
    let keys = ed.insert(NodeKey(2), "keys", MidiQueueNode::new());
    let clock = ed.insert(NodeKey(3), "clock", ClockNode::new());
    let out = ed.insert(NodeKey(4), "out", MidiOutNode::new());
    clock.set_enabled(true);
    let into_out = |ed: &mut Editor, node: u64, port: u16| {
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(4),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(node),
                port,
            }),
        );
    };
    into_out(&mut ed, 1, 0);
    into_out(&mut ed, 1, 3);
    into_out(&mut ed, 2, 0);
    into_out(&mut ed, 3, 0);
    ed.commit().unwrap();
    let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
    let mut drain = [MidiEvent::noop(); 256];
    // Warm up outside the gate.
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let (tx, rx) = MidiMailbox::pair();
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..200u32 {
            keys.note_on(MidiChannel::FIRST, 60, 100);
            let at = Beat(f64::from(b) * 512.0 / 24_000.0);
            exec.process(
                512,
                &Transport::new(true, Bpm(120.0), at, None),
                &[],
                &mut [],
            );
            let n = out.poll_into(&mut drain);
            // Both halves of a mailbox, too.
            tx.queue(&drain[..n.min(4)]);
            let _ = rx.poll_into(&mut drain);
        }
    });
    assert_eq!(out.dropped(), 0);
}
