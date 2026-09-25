//! Regression gate: `Executor::process` never allocates once a plan is
//! applied — through PDC rings, event delays, event fan-in merges, feedback of
//! both kinds, in-place aliasing, the silence skip, the `Legacy` adapter,
//! scheduled commands landing (on time, late, and still waiting), and block
//! lengths that change every call.
//!
//! What this cannot cover, stated rather than implied: `apply` allocates by
//! design in phase 1 (see `exec.rs`), so it runs outside the gate; and a node
//! that allocates is the node's bug, which this test catches only for the
//! nodes it runs.

mod common;

use assert_no_alloc::AllocDisabler;
use common::{prepare, Kind, TestNode};
use fundsp::prelude32::lowpass_hz;
use tutti_graph::{
    CrossfadeCurve, Editor, EventEdge, EventIn, EventKind, EventOut, Fade, Legacy, ParamRamp,
    Transport, Ump,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{At, Beat, ChannelLayout, Frame, NodeKey, Samples};

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
/// inside the gate grows its slot → aborts. Mutation: size the command
/// overlay with `Vec::new()` in `rebuild` → the first scheduled command to
/// land inside the gate grows it → aborts. Mutation: build the command
/// channel's pending list with `Vec::new()` → aborts.
#[test]
fn process_is_allocation_free_in_steady_state() {
    let (mut ed, mut exec) = Editor::new(prepare(256));
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
        EventEdge::feedback(ev(3, 0), Samples(256)),
    );
    let t = &mut spec.topology;
    t.edges.insert(at(5, 0), Edge::Direct(Source::Global(0)));
    t.edges.insert(at(6, 0), from(4, 0));
    t.edges.insert(at(7, 0), from(5, 0));
    t.edges.insert(at(7, 1), from(6, 0));
    t.edges.insert(
        at(7, 2),
        Edge::Feedback(FeedbackFrom::one_block(
            OutPort {
                node: NodeKey(9),
                port: 0,
            },
            Samples(256),
        )),
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
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let plan = exec.plan().expect("applied");
    assert!(
        plan.delays().len() >= 2,
        "the gate must run through PDC rings"
    );

    // Scheduled commands, landing throughout the gated run: into the
    // consumer's fan-in port (merged after its two sources) and its feedback
    // port, on time; one already late; one waiting on a beat the stopped
    // transport never reaches.
    let consumer = |port| EventIn {
        node: NodeKey(4),
        port,
    };
    for k in 0..200u64 {
        let kind = if k % 3 == 0 {
            EventKind::Ramp(ParamRamp::foreign(1, 0.5, Samples(16)))
        } else {
            EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0]))
        };
        ed.schedule(
            At::Frame(Frame(2_000 + k * 700)),
            consumer((k % 2) as u16),
            kind,
        )
        .expect("under capacity");
    }

    let input = vec![0.25f32; 256];
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    let transport = Transport::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    // Warm up: every size once, so any lazily-sized state is sized.
    for &n in &sizes {
        exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
    }
    // After the warm-up, so frame 0 is past: it lands late, inside the gate.
    ed.schedule(
        At::Frame(Frame(0)),
        consumer(0),
        EventKind::Midi(Ump([0; 4])),
    )
    .expect("under capacity");
    ed.schedule(
        At::Beat(Beat(4.0)),
        consumer(0),
        EventKind::Midi(Ump([0; 4])),
    )
    .expect("under capacity");
    assert_eq!(ed.commands_outstanding(), 202);

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            let n = sizes[i % sizes.len()];
            exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
        }
    });
    assert_eq!(exec.dropped_events(), 0);
    assert!(l.iter().any(|&x| x != 0.0), "the gate rendered signal");
    assert_eq!(exec.late_commands(), 1);
    assert_eq!(
        ed.commands_outstanding(),
        1,
        "every timed command landed inside the gate; the beat still waits"
    );
}

/// Crossfades never allocate on the audio thread: both units running, the
/// blend, a fade's end (its outgoing unit into the held commit, the commit
/// back on the return ring) and the start of the fade waiting behind it —
/// on an in-place stereo gain and on an event consumer (the general form).
/// The commits are applied before the gate, since applying allocates by
/// design; everything a fade does after that runs inside it.
///
/// Mutation: reserve `Commit::faded` with no room (`Vec::new()` in
/// `Commit::build`) → the first fade's end pushes past the reservation
/// inside the gate → `push_reserved`'s assert fires (a release build would
/// allocate there, and the gate abort) → fails. Mutation: allocate the blend's gains per block (collect them
/// into a `Vec` in `fading_node_op`) → aborts.
#[test]
fn crossfades_are_allocation_free() {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    ed.spec_mut().topology.inputs = ChannelLayout::STEREO;
    let gain = |g| TestNode::new(Kind::Gain { gain: g, width: 2 });
    ed.insert(NodeKey(1), "gain", gain(1.0));
    ed.insert(
        NodeKey(2),
        "emit",
        TestNode::new(Kind::Emitter {
            period: 9,
            phase: 0,
        }),
    );
    ed.insert(
        NodeKey(3),
        "consume",
        TestNode::new(Kind::Consumer { inputs: 1 }),
    );
    let consumer = EventIn {
        node: NodeKey(3),
        port: 0,
    };
    ed.spec_mut().connect_events(
        consumer,
        EventEdge::Direct(EventOut {
            node: NodeKey(2),
            port: 0,
        }),
    );
    let t = &mut ed.spec_mut().topology;
    t.edges.insert(at(1, 0), Edge::Direct(Source::Global(0)));
    t.edges.insert(at(1, 1), Edge::Direct(Source::Global(1)));
    t.outputs = vec![
        Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        }),
        Source::Node(OutPort {
            node: NodeKey(3),
            port: 0,
        }),
    ];
    ed.commit().expect("commits");

    let input = vec![0.25f32; 256];
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    let transport = Transport::default();
    let mut block = |exec: &mut tutti_graph::Executor, n: usize| {
        exec.process(
            n,
            &transport,
            &[&input[..], &input[..]],
            &mut [&mut l[..], &mut r[..]],
        );
    };
    block(&mut exec, 64);
    ed.collect();

    // A long fade, one queued behind it, and one on the event node.
    let fade = |n| Fade::new(Samples(n), CrossfadeCurve::EqualPower);
    ed.replace(NodeKey(1), gain(2.0), fade(3_000))
        .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 64);
    ed.replace(NodeKey(1), gain(4.0), fade(2_000))
        .expect("fits");
    ed.replace(
        NodeKey(3),
        TestNode::new(Kind::Consumer { inputs: 1 }),
        fade(500),
    )
    .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 64);
    assert_eq!(ed.in_flight(), 2, "both fade commits are held");

    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            block(&mut exec, sizes[i % sizes.len()]);
        }
    });
    // Three outgoing units came back: both fades at key 1 and the one at 3.
    let mut back = ed.collect();
    back.sort();
    assert_eq!(back, vec![NodeKey(1), NodeKey(1), NodeKey(3)]);
    assert_eq!(ed.in_flight(), 0);
}
