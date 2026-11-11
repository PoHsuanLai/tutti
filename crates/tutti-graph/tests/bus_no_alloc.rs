//! The audio thread's side of the buses allocates nothing (doc 013, "Buses and
//! coercion between declared layouts"): a bus edge's weighted lanes, a bypass
//! that is the fold of the main buses, and a node at `Mix::DRY` that reads the
//! same fold through the blend, each on the serial executor and on a pool whose
//! every participant runs inside `assert_no_alloc`.
//!
//! The renders are not compared: the figures are `bus_edges.rs`'s and
//! `mode.rs`'s. This pins the cost.

mod common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_graph::{
    Bus, BusRef, BusRole, BusSource, Buses, Cx, Edge, Editor, Executor, InPort, Io, Node, NodeKey,
    NodeMode, OutPort, Prepare, Shape, Source, Status, Transport, Unforkable,
};
use tutti_types::{BusTopology, ChannelLayout, ChannelTopology, Latency, Mix, Samples};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const BLOCK: usize = 64;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn smpte(n: u16) -> Bus {
    let topology = ChannelTopology::smpte(ChannelLayout::from_count(n)).expect("an SMPTE width");
    Bus::new(
        BusTopology::try_from(&topology).expect("fits a bus"),
        BusRole::Main,
    )
}

fn bused(inner: Kind, input: Bus, output: Bus) -> Kind {
    Kind::Bused {
        inner: Box::new(inner),
        buses_in: Box::new(Buses::new(&[input]).expect("one bus")),
        buses_out: Box::new(Buses::new(&[output]).expect("one bus")),
    }
}

/// A `6 -> 2` node, declared 5.1 to stereo, with `latency` frames: it writes
/// its first input to both outputs. (Not `TestNode`: its `reset` allocates, and
/// leaving Bypassed calls `reset` on the audio thread.)
struct Downmixer {
    latency: usize,
}

impl Node for Downmixer {
    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::new(&[smpte(6)]).expect("one bus"),
            Buses::new(&[smpte(2)]).expect("one bus"),
        )
        .with_latency(Latency::new(Samples(self.latency)))
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let (inputs, mut outputs) = io.split();
        for out in outputs.iter_mut() {
            out[..n].copy_from_slice(&inputs.get(0)[..n]);
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// Nodes: 1 a 5.1 source and 2 a stereo sink joined by a bus edge; 3 a latent
/// downmixer, bypassed; 4 one at `Mix::DRY`. Every node's inputs are the six
/// global ones; the outputs are the two channels of nodes 2, 3 and 4.
fn rig(workers: usize) -> (Editor, Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    if workers > 1 {
        exec.set_pool(Some(common::TestPool::no_alloc(workers)));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(6);
    let kinds = [
        (
            1,
            bused(
                Kind::Gain {
                    gain: 1.0,
                    width: 6,
                },
                smpte(6),
                smpte(6),
            ),
        ),
        (2, bused(Kind::Thru { width: 2 }, smpte(2), smpte(2))),
    ];
    for (n, kind) in kinds {
        ed.insert(key(n), "test", Unforkable(TestNode::new(kind)));
    }
    ed.insert(key(3), "down", Unforkable(Downmixer { latency: 40 }));
    ed.insert(key(4), "down", Unforkable(Downmixer { latency: 0 }));
    for n in [1, 3, 4] {
        for c in 0..6 {
            ed.spec_mut().topology.connect(
                InPort {
                    node: key(n),
                    port: c,
                },
                Edge::Direct(Source::Global(c)),
            );
        }
    }
    ed.connect_bus(
        BusSource::Node(BusRef::new(key(1), 0)),
        BusRef::new(key(2), 0),
    )
    .expect("5.1 folds into stereo");
    ed.set_mode(key(3), NodeMode::Bypassed).expect("folds");
    ed.set_mix(key(4), Mix::DRY).expect("a mix");
    let out = |n: u64, port: u16| Source::Node(OutPort { node: key(n), port });
    ed.spec_mut().topology.set_outputs(vec![
        out(2, 0),
        out(2, 1),
        out(3, 0),
        out(3, 1),
        out(4, 0),
        out(4, 1),
    ]);
    ed.commit().expect("compiles");
    exec.apply_pending();
    (ed, exec)
}

fn gate(workers: usize) {
    let (mut ed, mut exec) = rig(workers);
    let t = Transport::default();
    let input: Vec<Vec<f32>> = (0..6)
        .map(|c| (0..BLOCK).map(|i| ((i + c) as f32 * 0.07).sin()).collect())
        .collect();
    let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    let mut chans = vec![vec![0.0f32; BLOCK]; 6];
    let mut block = |ed: &mut Editor, exec: &mut Executor| {
        let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
        assert_no_alloc::assert_no_alloc(|| exec.process(BLOCK, &t, &ins, &mut outs));
        ed.collect();
    };
    // Warm up, so first-touch faults are not counted against the audio thread.
    for _ in 0..4 {
        block(&mut ed, &mut exec);
    }
    for round in 0..200 {
        // Toggling is a cell store; the fade, the un-bypass and the folds that
        // follow are the audio thread's.
        let mode = if round % 20 < 10 {
            NodeMode::Active
        } else {
            NodeMode::Bypassed
        };
        ed.set_mode(key(3), mode).expect("folds");
        block(&mut ed, &mut exec);
    }
    assert!(
        chans[2].iter().any(|s| s.abs() > 1e-6),
        "the fold rendered silence; the gate would walk no DSP"
    );
}

#[test]
fn bus_edges_and_bypass_folds_are_allocation_free_on_the_serial_executor() {
    gate(1);
}

#[test]
fn bus_edges_and_bypass_folds_are_allocation_free_on_every_participant() {
    gate(3);
}
