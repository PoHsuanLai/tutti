//! Regression gate: `Executor::process` never allocates once a plan is
//! applied — through PDC rings, event delays, event fan-in merges, feedback of
//! both kinds, in-place aliasing, the silence skip, the `Legacy` adapter
//! (draining a full settings ring),
//! scheduled commands landing (on time, late, and still waiting), writers
//! refusing past a declared event capacity, and block lengths that change
//! every call.
//!
//! What this cannot cover, stated rather than implied: `apply` allocates by
//! design in phase 1 (see `exec.rs`), so it runs outside the gate; and a node
//! that allocates is the node's bug, which this test catches only for the
//! nodes it runs.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use common::{prepare, Kind, TestNode};
use fundsp::prelude32::lowpass_hz;
use tutti_graph::{
    CrossfadeCurve, Cx, Delivery, Editor, EventEdge, EventIn, EventKind, EventOut, Fade, Io,
    Legacy, Node, ParamRamp, Prepare, Shape, Status, Transport, Ump, LEGACY_SETTINGS_CAPACITY,
};
use tutti_node::Setting;
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{At, Beat, ChannelLayout, Frame, Latency, NodeKey, Samples, Tail};

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
/// channel's pending list with `Vec::new()` → aborts. Mutation: allocate
/// once per setting in `Legacy::process`'s settings drain → aborts.
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
    // Controlled, so the gate drains a settings ring; pure, so it runs the
    // silence scan too.
    let (lowpass, mut settings) = Legacy::controlled(&mut ed, lowpass_hz(800.0, 0.7));
    ed.insert(NodeKey(8), "lowpass", lowpass.assume_pure());
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
    // A full ring of settings, drained into `AudioUnit::set` by the first
    // block inside the gate.
    for k in 0..LEGACY_SETTINGS_CAPACITY {
        assert_eq!(
            settings.set(Setting::center(700.0 + k as f32)),
            Delivery::Queued
        );
    }

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
/// blend, a fade's end (the crossfade and its outgoing unit onto the
/// fade-return ring) and the start of the fade waiting behind it —
/// on an in-place stereo gain and on an event consumer (the general form).
/// The commits are applied before the gate, since applying allocates by
/// design; everything a fade does after that runs inside it.
///
/// Mutation: build the fade-return ring with room for one crossfade
/// (`FADE_CAPACITY` 1 in `channels`) → the second ending fade finds it full
/// inside the gate → the push's assert fires → fails. Mutation: allocate
/// the blend's gains per block (collect them into a `Vec` in
/// `fading_node_op`) → aborts.
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
    assert!(ed.collect().is_empty(), "nothing has retired yet");
    assert_eq!(ed.in_flight(), 0, "fades hold no commit");
    assert_eq!(ed.fades_in_flight(), 3);

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
    assert_eq!(ed.fades_in_flight(), 0);
}

/// Pushes `burst` events on the first frame of every 256-frame bar into a
/// port declaring `cap` per block, counting what its writer refuses.
struct Spray {
    cap: u32,
    burst: u32,
    refused: Arc<AtomicU64>,
}

impl Node for Spray {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(self.cap)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for at in cx.env.offsets() {
            if cx.env.frame_at(at).get().is_multiple_of(256) {
                for i in 0..self.burst {
                    let e = tutti_graph::Event::midi(at, [0x2090_3c64 | i, 0, 0, 0]);
                    if io.event_out(0).push(e).is_err() {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Declares 141 frames of latency and an event output it never writes: it
/// puts PDC delays on its sink's other event edges.
struct LateEvents;

impl Node for LateEvents {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_latency(Latency::new(Samples(141)))
            .with_tail(Tail::Finite(Samples(141)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Counts the events on each of its two event inputs.
struct Tally([Arc<AtomicU64>; 2]);

impl Node for Tally {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(2, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        for (p, n) in self.0.iter().enumerate() {
            n.fetch_add(io.events(p).len() as u64, Ordering::Relaxed);
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Declared event capacities (`Shape::event_capacity`) never allocate on
/// the audio thread: two ports declaring 4 events per block, each handed a
/// burst of 6 every bar (the writer refusing 2 inside the gate), fan into
/// one input through a 141-frame PDC delay, and one also feeds back, with
/// the executor's default capacity at **1** — so every buffer on the way
/// must have been sized from the declarations. Nothing but the writers
/// refuses: the executor's drop count is exactly what the nodes were told.
///
/// Mutation: size event slots from the default alone
/// (`Vec::with_capacity(cap)` in `Executor::rebuild`) → the writers grow
/// their slots on the first bar (in the warm-up, outside the gate) and the
/// merges, sized for one per source, drop what the writers accepted →
/// fails. Mutation: size the PDC FIFOs from the default (`rate(from)` →
/// `cap`) → they overflow and drop more than the writers refused → fails.
#[test]
fn declared_event_capacities_are_allocation_free() {
    let (mut ed, mut exec) = Editor::with_event_capacity(prepare(256), 1);
    let refused = Arc::new(AtomicU64::new(0));
    let seen = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    for k in [1, 2] {
        ed.insert(
            NodeKey(k),
            "spray",
            Spray {
                cap: 4,
                burst: 6,
                refused: Arc::clone(&refused),
            },
        );
    }
    ed.insert(NodeKey(3), "late", LateEvents);
    ed.insert(NodeKey(4), "tally", Tally(seen.clone()));
    let ev = |node| EventOut {
        node: NodeKey(node),
        port: 0,
    };
    let tally = |port| EventIn {
        node: NodeKey(4),
        port,
    };
    let spec = ed.spec_mut();
    for from in [1, 2, 3] {
        spec.connect_events(tally(0), EventEdge::Direct(ev(from)));
    }
    spec.connect_events(tally(1), EventEdge::feedback(ev(1), Samples(300)));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let plan = exec.plan().expect("applied");
    assert!(
        plan.delays().len() >= 2,
        "both sprays reach the tally through a PDC delay"
    );

    let transport = Transport::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    for &n in &sizes {
        exec.process(n, &transport, &[], &mut []);
    }
    let before = refused.load(Ordering::Relaxed);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            exec.process(sizes[i % sizes.len()], &transport, &[], &mut []);
        }
    });
    let refused = refused.load(Ordering::Relaxed);
    assert!(refused > before, "writers refused inside the gate");
    assert_eq!(
        exec.dropped_events(),
        refused,
        "only the writers refused anything"
    );
    assert!(seen[0].load(Ordering::Relaxed) > 0 && seen[1].load(Ordering::Relaxed) > 0);
}
