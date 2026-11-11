//! Events a port cannot hold: a note-off is taken before a note-on is, every
//! loss is counted exactly and charged to the output port that had it, and
//! the parallel executor reports the same losses as the serial one.
//!
//! All figures are counted by hand from the nodes below, never from another
//! render.

mod common;

use std::sync::{Arc, Mutex};

use common::test_pool;
use tutti_fixtures::midi::{m1_note_off, m1_note_on};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventLoss, EventOut, Executor, Io, Node,
    Prepare, Shape, Status, Transport, note_off_room,
};
use tutti_types::{SampleRate, Samples};
use tutti_ump::{MidiChannel, NoteNumber, U7, Ump};

const BLOCK: usize = 64;

/// MIDI 1.0, group 0, channel 0, note-on, velocity 100.
fn note_on(key: u32) -> [u32; 4] {
    m1_note_on(MidiChannel::FIRST, NoteNumber::of(key as u8), U7::of(100)).0
}

fn note_off(key: u32) -> [u32; 4] {
    m1_note_off(MidiChannel::FIRST, NoteNumber::of(key as u8), U7::of(0)).0
}

/// One port's script for a block: `ons` note-ons then `offs` note-offs, each
/// on its own frame in order (frame = index, so offsets never decrease).
#[derive(Clone, Copy)]
struct Script {
    ons: u32,
    offs: u32,
}

/// Writes one script per output port every block, declaring `cap`.
struct Writer {
    cap: u32,
    scripts: Vec<Script>,
}

impl Node for Writer {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, self.scripts.len() as u16)
            .with_event_capacity(self.cap)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (p, s) in self.scripts.iter().enumerate() {
            let out = io.event_out(p);
            for i in 0..s.ons + s.offs {
                let words = if i < s.ons {
                    note_on(i % 100)
                } else {
                    note_off(i % 100)
                };
                let at = cx.env.offset(i as usize).expect("inside the block");
                let _ = out.push(Event::midi(at, Ump(words)));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Counts what arrives on each input port: `(note-ons, note-offs)`.
struct Counter {
    ports: u16,
    seen: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl Node for Counter {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for p in 0..io.event_input_count() {
            for e in io.events(p) {
                if e.is_note_off() {
                    seen[p].1 += 1;
                } else if matches!(e.kind, EventKind::Midi(Ump(_))) {
                    seen[p].0 += 1;
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// `writers` (each a key and its scripts) all feeding one counter, port `i`
/// of the counter taking writer `i`'s port 0 ... in the order listed.
fn rig(
    workers: usize,
    cap: u32,
    writers: &[Vec<Script>],
) -> (Editor, Executor, Arc<Mutex<Vec<(u32, u32)>>>) {
    let prep = Prepare::new(SampleRate(48_000.0), Samples(BLOCK));
    let (mut ed, mut exec) = Editor::new(prep);
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    let ports: u16 = writers.iter().map(|w| w.len() as u16).sum();
    let seen = Arc::new(Mutex::new(vec![(0, 0); ports as usize]));
    ed.insert(
        NodeKey(100),
        "counter",
        tutti_graph::Unforkable(Counter {
            ports,
            seen: Arc::clone(&seen),
        }),
    );
    let mut next = 0;
    for (i, scripts) in writers.iter().enumerate() {
        let key = NodeKey(1 + i as u64);
        ed.insert(
            key,
            "writer",
            tutti_graph::Unforkable(Writer {
                cap,
                scripts: scripts.clone(),
            }),
        );
        for p in 0..scripts.len() {
            ed.spec_mut().connect_events(
                EventIn {
                    node: NodeKey(100),
                    port: next,
                },
                EventEdge::Direct(EventOut {
                    node: key,
                    port: p as u16,
                }),
            );
            next += 1;
        }
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, seen)
}

fn run(ed: &mut Editor, exec: &mut Executor, blocks: usize) -> Vec<EventLoss> {
    for _ in 0..blocks {
        exec.process(BLOCK, &Transport::default(), &[], &mut []);
    }
    ed.take_event_losses().collect()
}

/// A port at its capacity takes note-offs, not note-ons, up to the room past
/// it. Capacity 16 is room for 16 + 2 + 8 = 26 events. A script of 20
/// note-ons and 20 note-offs writes the 16 first note-ons (4 refused), and
/// then 10 of its note-offs (10 refused): 16 + 10 = 26 land.
#[test]
fn a_full_port_takes_note_offs_before_note_ons() {
    assert_eq!(note_off_room(16), 26);
    let (mut ed, mut exec, seen) = rig(1, 16, &[vec![Script { ons: 20, offs: 20 }]]);
    let losses = run(&mut ed, &mut exec, 1);
    assert_eq!(*seen.lock().unwrap(), vec![(16, 10)]);
    assert_eq!(exec.dropped_event_count(), 14);
    assert_eq!(
        losses,
        vec![EventLoss {
            node: NodeKey(1),
            port: Some(0),
            count: 14,
            recovered: 0
        }]
    );
}

/// A node that keeps to its declaration loses nothing, note-offs included,
/// although the declaration was all note-ons-worth: 16 note-ons and 10
/// note-offs fit capacity 16 plus its note-off room.
#[test]
fn a_node_within_its_room_loses_nothing() {
    let (mut ed, mut exec, seen) = rig(1, 16, &[vec![Script { ons: 16, offs: 10 }]]);
    let losses = run(&mut ed, &mut exec, 3);
    assert_eq!(*seen.lock().unwrap(), vec![(48, 30)]);
    assert_eq!(exec.dropped_event_count(), 0);
    assert!(losses.is_empty());
}

/// The loss is charged to the port that had it, in the block that had it,
/// and a block with none reports none. Port 1 of the first writer is 5 over
/// (21 note-ons against 16) each of two blocks; its port 0 and the second
/// writer lose nothing.
#[test]
fn a_loss_is_charged_to_its_port() {
    let ok = Script { ons: 4, offs: 4 };
    let over = Script { ons: 21, offs: 0 };
    let (mut ed, mut exec, _) = rig(1, 16, &[vec![ok, over], vec![ok]]);
    let losses = run(&mut ed, &mut exec, 2);
    let each = EventLoss {
        node: NodeKey(1),
        port: Some(1),
        count: 5,
        recovered: 0,
    };
    assert_eq!(losses, vec![each, each]);
    assert_eq!(exec.dropped_event_count(), 10);
}

/// The parallel executor reports the same totals, and here the same set of
/// losses, as the serial one, whatever the worker count: each worker's log is
/// added to the block's. Four writers over capacity on different ports, so
/// the tasks spread. The entries are compared **sorted**: in a parallel block
/// they come in the order the workers finished, and a block that lost at more
/// places than a log holds (not this one) would leave out different ones. The
/// totals and the render are exact and identical; the order and truncation of
/// entries are not promised to match.
#[test]
fn the_parallel_losses_are_the_serial_ones() {
    let scripts = |n: u32| {
        vec![
            Script {
                ons: 20 + n,
                offs: n,
            },
            Script { ons: 3, offs: 3 },
        ]
    };
    let writers: Vec<Vec<Script>> = (0..4).map(scripts).collect();
    let (mut ed, mut exec, seen) = rig(1, 16, &writers);
    let serial = run(&mut ed, &mut exec, 5);
    let (serial_total, serial_seen) = (exec.dropped_event_count(), seen.lock().unwrap().clone());
    assert!(!serial.is_empty());
    let sorted = |mut v: Vec<EventLoss>| {
        v.sort_by_key(|l| (l.node, l.port, l.count));
        v
    };
    for workers in common::WORKERS {
        let (mut ed, mut exec, seen) = rig(workers, 16, &writers);
        assert!(exec.is_parallel(), "{workers} workers");
        let parallel = run(&mut ed, &mut exec, 5);
        assert_eq!(exec.dropped_event_count(), serial_total, "{workers}");
        assert_eq!(*seen.lock().unwrap(), serial_seen, "{workers}");
        assert_eq!(sorted(parallel), sorted(serial.clone()), "{workers}");
    }
}
