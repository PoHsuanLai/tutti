//! The hardware input path from the driver callback to the drain: what a
//! device's thread pays to push an event, and what the graph pays to take the
//! block's events out of the inlet, translate them and send them out of their
//! channels' ports (the receive path is about 0.3% of a 64-frame block at 48 kHz
//! for 40 events a block).
//!
//! - `push`: the callback's side alone, 40 events pushed into a device's inlet
//!   (`DeviceInput::push`, ring write and the stamp).
//! - `drain`: the graph's side alone, one 512-frame block of an ingest node
//!   over 40 events already in its inlet.
//! - `callback_to_drain`: both, one thread, one block.
//!
//! The wall time between a callback and the block that plays the event is the
//! inlet's lead (one block, `tutti-session`'s `input_lead`), by design, and
//! is not a cost to bench. The executor and inlets are built outside the timed
//! region.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use tutti_fixtures::midi;
use tutti_graph::{
    Editor, EventCapacity, EventEdge, EventIn, EventOut, Executor, NodeKey, Prepare,
    TransportPosition,
};
use tutti_midi_runtime::{DeviceInlets, DeviceInput, InletChange, MidiIngestNode};
use tutti_types::MaxBlock;
use tutti_types::{SampleRate, Samples};
use tutti_ump::{CCNumber, MidiChannel, U7, Ump};

const BLOCK: usize = 512;
const EVENTS: usize = 40;

/// Forty MIDI 1.0 messages: notes on and off over a sweep of controllers.
fn wire() -> Vec<Ump> {
    (0..EVENTS as u8)
        .map(|i| {
            let ch = MidiChannel::of(i % 4);
            match i % 4 {
                0 => midi::m1_note_on(ch, 40 + i, U7::of(100)),
                1 => midi::m1_note_off(ch, 40 + i - 1, U7::of(0)),
                _ => midi::m1_cc(ch, CCNumber::of(1), U7::of(i)),
            }
        })
        .collect()
}

/// An executor over an ingest node that follows a one-slot set of inlets, and
/// the device's end of its inlet.
fn rig() -> (Executor, DeviceInput) {
    let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(4_096), Duration::ZERO);
    let device = inlets.open().expect("a free slot");
    let (mut editor, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples(BLOCK)),
    ));
    editor.insert(
        NodeKey(1),
        "ingest",
        MidiIngestNode::new(NonZeroUsize::MIN)
            .following(&inlets)
            .expect("one slot"),
    );
    for change in inlets.take_changes() {
        if let InletChange::Attach { node, .. } = change {
            editor.insert(NodeKey(2), "inlet", node);
            editor.spec_mut().connect_events(
                EventIn {
                    node: NodeKey(1),
                    port: 0,
                },
                EventEdge::Direct(EventOut {
                    node: NodeKey(2),
                    port: 0,
                }),
            );
        }
    }
    editor.commit().expect("commits");
    (exec, device)
}

fn push(device: &mut DeviceInput, wire: &[Ump]) {
    let now = Instant::now();
    for &ump in wire {
        let _ = device.push(ump, now);
    }
}

fn block(exec: &mut Executor) {
    exec.process(BLOCK, &TransportPosition::default(), &[], &mut []);
}

fn ingest(c: &mut Criterion) {
    let wire = wire();
    let mut group = c.benchmark_group("ingest");
    group.bench_function("push", |b| {
        b.iter_batched(
            rig,
            |(exec, mut device)| {
                push(&mut device, &wire);
                (exec, device)
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("drain", |b| {
        b.iter_batched(
            || {
                let (exec, mut device) = rig();
                push(&mut device, &wire);
                (exec, device)
            },
            |(mut exec, device)| {
                block(&mut exec);
                (exec, device)
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("callback_to_drain", |b| {
        b.iter_batched(
            rig,
            |(mut exec, mut device)| {
                push(&mut device, &wire);
                block(&mut exec);
                (exec, device)
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, ingest);
criterion_main!(benches);
