//! The chase (`Node::chase`, doc 013, "Nodes communicate only through
//! ports"): a replaced node writes its state for its successor, and the
//! executor delivers it once, at offset 0 of the successor's first block,
//! ahead of every other event there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tutti_graph::testing as common;

use common::{WORKERS, bits, prepare, test_pool};
use tutti_fixtures::midi::m1_cc;
use tutti_graph::{
    Buses, Chase, CrossfadeCurve, Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut,
    Executor, Fade, Io, Node, Prepare, Shape, Status, Transport, Unforkable,
};
use tutti_graph::{NodeKey, OutPort, Source};
use tutti_types::{ChannelLayout, Samples};
use tutti_ump::{CCNumber, MidiChannel, U7, Ump};

const BLOCK: usize = 64;

/// What each keeper received: `(call, offset, first word)`, in delivery order.
type Log = Arc<Mutex<Vec<(usize, u32, u32)>>>;

/// A mono instrument stand-in. Its state is the last message it received,
/// heard as a constant output (the message's data byte over 127); with
/// `chases` it writes that state for its successor.
struct Keeper {
    state: Option<Ump>,
    chases: bool,
    capacity: u32,
    log: Log,
    calls: usize,
}

impl Keeper {
    fn new(state: Option<Ump>, chases: bool, capacity: u32, log: &Log) -> Self {
        Self {
            state,
            chases,
            capacity,
            log: Arc::clone(log),
            calls: 0,
        }
    }
}

fn data(u: &Ump) -> f32 {
    (u.words()[0] & 0x7f) as f32 / 127.0
}

impl Node for Keeper {
    fn shape(&self) -> Shape {
        let s = Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::MONO)).with_events(1, 0);
        if self.chases {
            s.with_chase(self.capacity)
        } else {
            s
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for e in io.events(0).iter() {
            if let EventKind::Midi(u) = e.kind {
                self.log
                    .lock()
                    .unwrap()
                    .push((self.calls, e.offset.get(), u.words()[0]));
                self.state = Some(u);
            }
        }
        self.calls += 1;
        let v = self.state.as_ref().map_or(0.0, data);
        io.output(0).fill(v);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn chase(&self, out: &mut Chase<'_>) {
        if let Some(u) = self.state {
            out.push(u);
        }
    }
}

/// A mono instrument stand-in with a whole bank of state, for overflow.
struct Wide(u8);

impl Node for Wide {
    fn shape(&self) -> Shape {
        Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::MONO))
            .with_events(1, 0)
            .with_chase(3)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn chase(&self, out: &mut Chase<'_>) {
        for i in 0..self.0 {
            out.push(cc(i, 1));
        }
    }
}

/// The keeper's audio shape and no event input at all.
struct Deaf;

impl Node for Deaf {
    fn shape(&self) -> Shape {
        Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::MONO))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn cc(controller: u8, value: u8) -> Ump {
    m1_cc(
        MidiChannel::FIRST,
        CCNumber::new(controller).unwrap(),
        U7::of(value),
    )
}

/// Writes one message at offset 0 of every block.
struct Edge0(Ump, Arc<AtomicBool>);

impl Node for Edge0 {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if !self.1.load(Ordering::Relaxed) {
            return Status::Silent;
        }
        let at = cx.env.offsets().next().expect("a block has a frame");
        io.event_out(0).push(Event::midi(at, self.0)).unwrap();
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// One keeper at key 1 into the output; its event input is fed by nothing.
fn rig(old: Keeper) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(prepare(BLOCK));
    ed.insert(NodeKey(1), "keeper", Unforkable(old));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    (ed, exec)
}

fn block(exec: &mut Executor) -> Vec<f32> {
    blocks(exec, 1).remove(0)
}

fn blocks(exec: &mut Executor, outputs: usize) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0; BLOCK]; outputs];
    let mut slices: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(BLOCK, &Transport::default(), &[], &mut slices);
    out
}

fn cut() -> Fade {
    Fade::new(Samples(0), CrossfadeCurve::EqualPower)
}

/// A CC and its value set on the outgoing node sound from the incoming
/// node's first block, as if it had received them: the spec of a chase.
#[test]
fn the_chased_state_is_heard_on_the_incoming_nodes_first_block() {
    let log = Log::default();
    let set = cc(7, 100);
    let (mut ed, mut exec) = rig(Keeper::new(Some(set), true, 8, &log));
    assert_eq!(block(&mut exec)[0], data(&set), "the old node's state");
    ed.replace(
        NodeKey(1),
        Unforkable(Keeper::new(None, true, 8, &log)),
        cut(),
    )
    .expect("fits");
    ed.commit().expect("commits");
    let first = block(&mut exec);
    assert!(
        first.iter().all(|&s| s == data(&set)),
        "first block: {first:?}"
    );
    assert_eq!(log.lock().unwrap().as_slice(), &[(0, 0, set.words()[0])]);
}

/// Without `chase` nothing travels: the incoming node starts empty.
#[test]
fn a_node_that_does_not_chase_hands_nothing_over() {
    let log = Log::default();
    let (mut ed, mut exec) = rig(Keeper::new(Some(cc(7, 100)), false, 8, &log));
    block(&mut exec);
    ed.replace(
        NodeKey(1),
        Unforkable(Keeper::new(None, true, 8, &log)),
        cut(),
    )
    .expect("fits");
    ed.commit().expect("commits");
    assert!(block(&mut exec).iter().all(|&s| s == 0.0));
    assert!(log.lock().unwrap().is_empty());
}

/// During a crossfade the incoming node is chased and the outgoing one is
/// not sent anything.
#[test]
fn a_crossfade_chases_the_incoming_node_alone() {
    let (old_log, new_log) = (Log::default(), Log::default());
    let set = cc(7, 90);
    let (mut ed, mut exec) = rig(Keeper::new(Some(set), true, 8, &old_log));
    block(&mut exec);
    let fade = Fade::new(Samples(4 * BLOCK), CrossfadeCurve::EqualPower);
    ed.replace(
        NodeKey(1),
        Unforkable(Keeper::new(None, true, 8, &new_log)),
        fade,
    )
    .expect("fits");
    ed.commit().expect("commits");
    for _ in 0..3 {
        block(&mut exec);
    }
    assert_eq!(
        new_log.lock().unwrap().as_slice(),
        &[(0, 0, set.words()[0])]
    );
    assert!(
        old_log.lock().unwrap().is_empty(),
        "the outgoing node hears nothing"
    );
}

/// The chase lands ahead of an edge event at the same offset.
#[test]
fn the_chase_comes_ahead_of_edge_events_at_offset_zero() {
    let log = Log::default();
    let (old, edge) = (cc(7, 100), cc(10, 20));
    let on = Arc::new(AtomicBool::new(false));
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.insert(
        NodeKey(1),
        "keeper",
        Unforkable(Keeper::new(Some(old), true, 8, &log)),
    );
    ed.insert(NodeKey(2), "edge", Unforkable(Edge0(edge, Arc::clone(&on))));
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(1),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(2),
            port: 0,
        }),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    block(&mut exec);
    on.store(true, Ordering::Relaxed);
    ed.replace(
        NodeKey(1),
        Unforkable(Keeper::new(None, true, 8, &log)),
        cut(),
    )
    .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec);
    let seen: Vec<_> = log.lock().unwrap().iter().map(|e| (e.1, e.2)).collect();
    assert_eq!(seen, [(0, old.words()[0]), (0, edge.words()[0])]);
}

/// A node with no event input cannot take the chase: each message is dropped
/// and counted.
#[test]
fn a_successor_with_no_event_input_counts_the_drop() {
    let log = Log::default();
    let (mut ed, mut exec) = rig(Keeper::new(Some(cc(7, 100)), true, 8, &log));
    block(&mut exec);
    assert_eq!(exec.dropped_event_count(), 0);
    ed.insert(NodeKey(1), "deaf", Unforkable(Deaf));
    ed.commit().expect("commits");
    block(&mut exec);
    assert_eq!(exec.dropped_event_count(), 1);
}

/// Past the declared capacity a message is refused and counted, and what
/// fits is delivered in order.
#[test]
fn a_chase_past_its_declared_capacity_is_counted() {
    let log = Log::default();
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.insert(NodeKey(1), "wide", Unforkable(Wide(5)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    block(&mut exec);
    ed.replace(
        NodeKey(1),
        Unforkable(Keeper::new(None, true, 8, &log)),
        cut(),
    )
    .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec);
    assert_eq!(
        exec.dropped_event_count(),
        2,
        "five written, three declared"
    );
    let seen: Vec<_> = log.lock().unwrap().iter().map(|e| e.2).collect();
    let want: Vec<_> = (0..3).map(|i| cc(i, 1).words()[0]).collect();
    assert_eq!(seen, want);
}

/// A render across swaps (cut and crossfade, on several keys) is bit for bit
/// the serial one on every worker count.
#[test]
fn a_chase_renders_bit_identically_in_parallel() {
    fn render(workers: usize) -> Vec<Vec<u32>> {
        let log = Log::default();
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        if workers > 1 {
            exec.set_pool(Some(test_pool(workers)));
        }
        let keys = 1..=6u64;
        for k in keys.clone() {
            ed.insert(
                NodeKey(k),
                "keeper",
                Unforkable(Keeper::new(Some(cc(k as u8, 10 * k as u8)), true, 8, &log)),
            );
        }
        ed.spec_mut().topology.set_outputs(keys.clone().map(|k| {
            Source::Node(OutPort {
                node: NodeKey(k),
                port: 0,
            })
        }));
        ed.commit().expect("commits");
        let mut all = Vec::new();
        let mut run = |exec: &mut Executor| all.extend(blocks(exec, 6));
        run(&mut exec);
        for k in keys.clone() {
            let fade = if k % 2 == 0 {
                cut()
            } else {
                Fade::new(Samples(3 * BLOCK), CrossfadeCurve::EqualPower)
            };
            ed.replace(
                NodeKey(k),
                Unforkable(Keeper::new(None, true, 8, &log)),
                fade,
            )
            .expect("fits");
        }
        ed.commit().expect("commits");
        for _ in 0..6 {
            run(&mut exec);
        }
        assert_eq!(exec.is_parallel(), workers > 1);
        bits(&all)
    }
    let serial = render(1);
    for w in WORKERS {
        assert_eq!(render(w), serial, "{w} participants");
    }
}
