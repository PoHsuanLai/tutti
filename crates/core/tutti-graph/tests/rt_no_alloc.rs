//! Regression gate: `Executor::process` never allocates once a plan is
//! applied — through PDC rings, event delays, event fan-in merges, feedback of
//! both kinds, in-place aliasing, the silence skip, the `Legacy` adapter, and
//! block lengths that change every call.
//!
//! What this cannot cover, stated rather than implied: `apply` allocates by
//! design in phase 1 (see `exec.rs`), so it runs outside the gate; and a node
//! that allocates is the node's bug, which this test catches only for the
//! nodes it runs.

mod common;

use assert_no_alloc::AllocDisabler;
use common::{prepare, Kind, TestNode};
use fundsp::prelude32::lowpass_hz;
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Executor, Legacy, Transport};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, NodeKey};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn at(node: u64, port: u16) -> InPort {
    InPort {
        node: NodeKey(node),
        port,
    }
}

fn from(node: u64, port: u16) -> Edge {
    Edge::Direct(Source::Node(OutPort {
        node: NodeKey(node),
        port,
    }))
}

/// Mutation: in `Executor::apply`, create the event slots with `Vec::new()`
/// instead of `Vec::with_capacity(cap)` → the first event a writer accepts
/// inside the gate grows its slot → aborts.
#[test]
fn process_is_allocation_free_in_steady_state() {
    let mut ed = Editor::new(prepare(256));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        NodeKey(1),
        "emit",
        TestNode::new(Kind::Emitter {
            period: 5,
            phase: 0,
        }),
    );
    ed.insert(
        NodeKey(2),
        "emit",
        TestNode::new(Kind::Emitter {
            period: 7,
            phase: 3,
        }),
    );
    ed.insert(
        NodeKey(3),
        "evlag",
        TestNode::new(Kind::EventLag { latency: 11 }),
    );
    ed.insert(
        NodeKey(4),
        "consume",
        TestNode::new(Kind::Consumer { inputs: 2 }),
    );
    ed.insert(NodeKey(5), "lag", TestNode::new(Kind::Lag { latency: 40 }));
    ed.insert(NodeKey(6), "smooth", TestNode::new(Kind::Smooth));
    ed.insert(NodeKey(7), "sum", TestNode::new(Kind::Sum { inputs: 3 }));
    ed.insert(NodeKey(8), "lowpass", Legacy::new(lowpass_hz(800.0, 0.7)));
    ed.insert(
        NodeKey(9),
        "gain",
        TestNode::new(Kind::Gain {
            gain: 0.5,
            width: 1,
        }),
    );

    let spec = ed.spec_mut();
    let ev = |node, port| EventOut {
        node: NodeKey(node),
        port,
    };
    spec.connect_events(
        EventIn {
            node: NodeKey(3),
            port: 0,
        },
        EventEdge::Direct(ev(1, 0)),
    );
    // Fan-in, a PDC-delayed source (3 is 11 frames late), and feedback.
    spec.connect_events(
        EventIn {
            node: NodeKey(4),
            port: 0,
        },
        EventEdge::Direct(ev(3, 0)),
    );
    spec.connect_events(
        EventIn {
            node: NodeKey(4),
            port: 0,
        },
        EventEdge::Direct(ev(2, 0)),
    );
    spec.connect_events(
        EventIn {
            node: NodeKey(4),
            port: 1,
        },
        EventEdge::Feedback(ev(3, 0)),
    );
    let t = &mut spec.topology;
    t.edges.insert(at(5, 0), Edge::Direct(Source::Global(0)));
    t.edges.insert(at(6, 0), from(4, 0));
    t.edges.insert(at(7, 0), from(5, 0));
    t.edges.insert(at(7, 1), from(6, 0));
    t.edges.insert(
        at(7, 2),
        Edge::Feedback(FeedbackFrom {
            from: OutPort {
                node: NodeKey(9),
                port: 0,
            },
        }),
    );
    t.edges.insert(at(8, 0), from(7, 0));
    t.edges.insert(at(9, 0), from(8, 0));
    t.outputs = vec![
        Source::Node(OutPort {
            node: NodeKey(9),
            port: 0,
        }),
        Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        }),
    ];

    let mut exec = Executor::new(prepare(256));
    let done = exec.apply(ed.commit().expect("commits"));
    ed.reclaim(done);
    let plan = exec.plan().expect("applied");
    assert!(
        plan.delays().len() >= 2,
        "the gate must run through PDC rings"
    );

    let input = vec![0.25f32; 256];
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    let transport = Transport::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    // Warm up: every size once, so any lazily-sized state is sized.
    for &n in &sizes {
        exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            let n = sizes[i % sizes.len()];
            exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
        }
    });
    assert_eq!(exec.dropped_events(), 0);
    assert!(l.iter().any(|&x| x != 0.0), "the gate rendered signal");
}
