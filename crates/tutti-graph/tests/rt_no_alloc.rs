//! Regression gate: `Executor::process` never allocates once a plan is
//! applied — through PDC rings, event delays, event fan-in merges, feedback of
//! both kinds, in-place aliasing, the silence skip, scheduled commands
//! landing (on time, late, and still waiting), writers refusing past a declared event capacity, and block lengths that change
//! every call.
//!
//! Each gate runs twice: on the serial executor and on the parallel one
//! (`src/par.rs`), whose every participant — the helper
//! threads as well as the caller — runs its share of each block inside
//! `assert_no_alloc` (`common::TestPool::no_alloc`), so an allocation or a
//! free on any of them aborts. What the parallel gate cannot see: the pool's
//! own idle loop between jobs (the test pool spins; `tutti_graph::WorkerPool`'s
//! park and wake are covered by `tutti-engine`'s `rt_no_alloc_engine`).
//!
//! What this cannot cover, stated rather than implied: applying a commit
//! allocates (see `exec.rs`), so it runs outside the gate; and a node
//! that allocates is the node's bug, which this test catches only for the
//! nodes it runs.

use tutti_graph::testing as common;
use tutti_graph::{Curvature, Exponent};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_fixtures::midi::m1_note_on;
use tutti_graph::NodeKey;
use tutti_graph::{
    Buses, CrossfadeCurve, Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Fade, Io, Law,
    Node, ParamRamp, ParamSegment, Prepare, Shape, Status, Transport, Unforkable,
};
use tutti_graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{At, Beat, ChannelLayout, Frame, Latency, Samples, Tail};
use tutti_ump::{MidiChannel, NoteNumber, U7, Ump};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Run `exec`'s blocks on a [`common::TestPool`] of `workers` participants
/// whose every participant runs its share inside `assert_no_alloc` — the
/// parallel path, helpers included (`1`: serial, no pool). Set before the
/// first commit is applied, so the parallel state is built with the plan,
/// outside the gate, as applying is.
fn use_pool(exec: &mut tutti_graph::Executor, workers: usize) {
    if workers > 1 {
        exec.set_pool(Some(common::TestPool::no_alloc(workers)));
    }
}

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

fn process_is_allocation_free_in_steady_state_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        NodeKey(1),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 5,
            phase: 0,
        })),
    );
    ed.insert(
        NodeKey(2),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 7,
            phase: 3,
        })),
    );
    ed.insert(
        NodeKey(3),
        "evlag",
        Unforkable(TestNode::new(Kind::EventLag { latency: 11 })),
    );
    ed.insert(
        NodeKey(4),
        "consume",
        Unforkable(TestNode::new(Kind::Consumer { inputs: 2 })),
    );
    ed.insert(
        NodeKey(5),
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 40 })),
    );
    ed.insert(
        NodeKey(6),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.insert(
        NodeKey(7),
        "sum",
        Unforkable(TestNode::new(Kind::Sum { inputs: 3 })),
    );
    ed.insert(
        NodeKey(8),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.insert(
        NodeKey(9),
        "gain",
        Unforkable(TestNode::new(Kind::Gain {
            gain: 0.5,
            width: 1,
        })),
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
    t.edges
        .insert(at(5, 0), vec![Edge::Direct(Source::Global(0))]);
    t.edges.insert(at(6, 0), vec![from(4, 0)]);
    t.edges.insert(at(7, 0), vec![from(5, 0)]);
    t.edges.insert(at(7, 1), vec![from(6, 0)]);
    t.edges.insert(
        at(7, 2),
        vec![Edge::Feedback(FeedbackFrom::one_block(
            OutPort {
                node: NodeKey(9),
                port: 0,
            },
            Samples(256),
        ))],
    );
    t.edges.insert(at(8, 0), vec![from(7, 0)]);
    t.edges.insert(at(9, 0), vec![from(8, 0)]);
    t.set_outputs(vec![
        Source::Node(OutPort {
            node: NodeKey(9),
            port: 0,
        }),
        Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        }),
    ]);
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
            EventKind::Midi(m1_note_on(
                MidiChannel::FIRST,
                NoteNumber::of(60),
                U7::of(100),
            ))
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

    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            let n = sizes[i % sizes.len()];
            exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
        }
    });
    assert_eq!(exec.dropped_event_count(), 0);
    assert!(l.iter().any(|&x| x != 0.0), "the gate rendered signal");
    assert_eq!(exec.late_command_count(), 1);
    assert_eq!(
        ed.commands_outstanding(),
        1,
        "every timed command landed inside the gate; the beat still waits"
    );
}

#[test]
fn process_is_allocation_free_in_steady_state() {
    process_is_allocation_free_in_steady_state_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_process_is_allocation_free_in_steady_state() {
    process_is_allocation_free_in_steady_state_on(4);
}

/// Crossfades never allocate on the audio thread: both units running, the
/// blend, a fade's end (the crossfade and its outgoing unit onto the
/// fade-return ring) and the start of the fade waiting behind it —
/// on an in-place stereo gain and on an event consumer (the general form).
/// The commits are applied before the gate, since applying allocates by
/// design; everything a fade does after that runs inside it.
fn crossfades_are_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::STEREO;
    let gain = |g| TestNode::new(Kind::Gain { gain: g, width: 2 });
    ed.insert(NodeKey(1), "gain", Unforkable(gain(1.0)));
    ed.insert(
        NodeKey(2),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 9,
            phase: 0,
        })),
    );
    ed.insert(
        NodeKey(3),
        "consume",
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
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
    t.edges
        .insert(at(1, 0), vec![Edge::Direct(Source::Global(0))]);
    t.edges
        .insert(at(1, 1), vec![Edge::Direct(Source::Global(1))]);
    t.set_outputs(vec![
        Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        }),
        Source::Node(OutPort {
            node: NodeKey(3),
            port: 0,
        }),
    ]);
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
    ed.replace(NodeKey(1), Unforkable(gain(2.0)), fade(3_000))
        .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 64);
    ed.replace(NodeKey(1), Unforkable(gain(4.0)), fade(2_000))
        .expect("fits");
    ed.replace(
        NodeKey(3),
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
        fade(500),
    )
    .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 64);
    assert!(ed.collect().is_empty(), "nothing has retired yet");
    assert_eq!(ed.in_flight(), 0, "fades hold no commit");
    assert_eq!(ed.fades_in_flight(), 3);

    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
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

#[test]
fn crossfades_are_allocation_free() {
    crossfades_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_crossfades_are_allocation_free() {
    crossfades_are_allocation_free_on(4);
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
        Shape::EMPTY.with_events(0, 1).with_event_capacity(self.cap)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for at in cx.env.offsets() {
            if cx.env.frame_at(at).get().is_multiple_of(256) {
                for i in 0..self.burst {
                    let e = tutti_graph::Event::midi(
                        at,
                        m1_note_on(
                            MidiChannel::FIRST,
                            NoteNumber::of(60),
                            U7::of(100 | i as u8),
                        ),
                    );
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
        Shape::EMPTY
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
        Shape::EMPTY.with_events(2, 0)
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
fn declared_event_capacities_are_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::with_event_capacity(prepare(256), 1);
    use_pool(&mut exec, workers);
    let refused = Arc::new(AtomicU64::new(0));
    let seen = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    for k in [1, 2] {
        ed.insert(
            NodeKey(k),
            "spray",
            Unforkable(Spray {
                cap: 4,
                burst: 6,
                refused: Arc::clone(&refused),
            }),
        );
    }
    ed.insert(NodeKey(3), "late", Unforkable(LateEvents));
    ed.insert(NodeKey(4), "tally", Unforkable(Tally(seen.clone())));
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
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            exec.process(sizes[i % sizes.len()], &transport, &[], &mut []);
        }
    });
    let refused = refused.load(Ordering::Relaxed);
    assert!(refused > before, "writers refused inside the gate");
    assert_eq!(
        exec.dropped_event_count(),
        refused,
        "only the writers refused anything"
    );
    assert!(seen[0].load(Ordering::Relaxed) > 0 && seen[1].load(Ordering::Relaxed) > 0);
}

#[test]
fn declared_event_capacities_are_allocation_free() {
    declared_event_capacities_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_declared_event_capacities_are_allocation_free() {
    declared_event_capacities_are_allocation_free_on(4);
}

/// More places lose events than a block's log holds (64) and the control side
/// never drains the ring (256 entries): 80 writers, each two events over its
/// port, 100 blocks. The log fills every block, the ring fills in the fourth,
/// and nothing allocates on any participant, serial or parallel. What no entry
/// names is counted instead, so entries plus the unreported count are exactly
/// the total.
fn losses_past_the_log_and_the_ring_are_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    let refused = Arc::new(AtomicU64::new(0));
    for k in 1..=80u64 {
        ed.insert(
            NodeKey(k),
            "spray",
            Unforkable(Spray {
                cap: 4,
                burst: 6,
                refused: Arc::clone(&refused),
            }),
        );
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    let transport = Transport::default();
    // No warm-up block: the first one is the first to fill the log, and a log
    // that grows would show there.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..101 {
            exec.process(256, &transport, &[], &mut []);
        }
    });
    let total = exec.dropped_event_count();
    assert_eq!(total, 101 * 80 * 2);
    let named: u64 = ed.take_event_losses().map(|l| u64::from(l.count)).sum();
    let unreported = ed.take_unreported_event_losses();
    assert!(unreported > 0, "the log and the ring were full");
    assert_eq!(named + unreported, total, "nothing is lost silently");
}

#[test]
fn losses_past_the_log_and_the_ring_do_not_allocate() {
    losses_past_the_log_and_the_ring_are_allocation_free_on(1);
}

#[test]
fn parallel_losses_past_the_log_and_the_ring_do_not_allocate() {
    losses_past_the_log_and_the_ring_are_allocation_free_on(4);
}

/// Declares `Cutoff` and `Q` (bases 1.0 and 2.0); writes the sum of what it
/// reads for them.
struct ParamSink;

impl Node for ParamSink {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .with_params(&[tutti_types::UnitParam::Cutoff, tutti_types::UnitParam::Q])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let a = io.param(0).frames().map_or(1.0, |v| v[i]);
            let b = io.param(1).frames().map_or(2.0, |v| v[i]);
            io.output(0)[i] = a + b;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        [1.0, 2.0].get(port).copied()
    }
}

/// A ramp or a curve segment on `Cutoff` every 37 frames (every shape
/// class, a resumed one).
struct RampEvery;

impl Node for RampEvery {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(16)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for i in 0..io.frames() {
            let f = start + i as u64;
            if f.is_multiple_of(37) {
                let o = io.offset(i).expect("inside");
                let event = match (f / 37) % 3 {
                    // A linear ramp, from wherever the source is.
                    0 => tutti_graph::Event::ramp(
                        o,
                        ParamRamp::new(
                            tutti_types::ParamKey::<tutti_types::Hz>::CUTOFF,
                            tutti_types::Hz((f % 5) as f32),
                            Samples(20),
                        ),
                    ),
                    // A curve segment spanning the next event: interrupted.
                    k => {
                        let shape = [
                            Law::PowerIn(Exponent::of(2.0)),
                            Law::Exp(Curvature::of(3.0)),
                            Law::PowerInOut(Exponent::of(1.5)),
                            Law::Hold,
                        ][(f / 111) as usize % 4];
                        let seg = ParamSegment::new(
                            tutti_types::ParamKey::<tutti_types::Hz>::CUTOFF,
                            tutti_types::Hz((f % 3) as f32),
                            tutti_types::Hz((f % 7) as f32),
                            Samples(50),
                            shape,
                        );
                        let seg = if k == 2 {
                            seg.resumed(
                                tutti_types::SamplePosition(3.5),
                                tutti_types::SamplePosition(41.25),
                            )
                        } else {
                            seg
                        };
                        tutti_graph::Event::segment(o, seg)
                    }
                };
                let _ = io.event_out(0).push(event);
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Compiler-owned param modulation never allocates on the audio thread:
/// the fused step over audio sources (one delayed by PDC behind a latent
/// sibling), an event source of ramps and curve segments (every shape
/// class) and LUT shapings, the declick of a connection
/// made and one broken, blocks of every length, and ranges moved between
/// blocks without a commit. The commits are applied before the gate
/// (applying allocates by design); the declicks they start, and every block
/// that reads a moved range, run inside it.
fn modulated_params_are_allocation_free_on(workers: usize) {
    use tutti_graph::ParamIn;
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(NodeKey(1), "sink", Unforkable(ParamSink));
    ed.insert(
        NodeKey(2),
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 31 })),
    );
    ed.insert(NodeKey(4), "ramps", Unforkable(RampEvery));
    {
        let t = &mut ed.spec_mut().topology;
        t.edges
            .insert(at(2, 0), vec![Edge::Direct(Source::Global(0))]);
        t.edges.insert(at(1, 0), vec![from(2, 0)]);
        t.set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })]);
    }
    let cut = ParamIn {
        node: NodeKey(1),
        param: tutti_types::UnitParam::Cutoff.into(),
    };
    let q = ParamIn {
        node: NodeKey(1),
        param: tutti_types::UnitParam::Q.into(),
    };
    // The ramps are the early path: the sink sits behind the lag's PDC, so
    // the plan delays its event source.
    let ramps = EventOut {
        node: NodeKey(4),
        port: 0,
    };
    ed.spec_mut().connect_param(cut, ramps);
    ed.commit().expect("commits");

    let input: Vec<f32> = (0..256).map(|i| (i % 13) as f32 / 13.0).collect();
    let mut out = vec![0.0f32; 256];
    let transport = Transport::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    let mut run = |exec: &mut tutti_graph::Executor| {
        for i in 0..200 {
            let n = sizes[i % sizes.len()];
            exec.process(n, &transport, &[&input[..n]], &mut [&mut out[..n]]);
        }
    };
    exec.apply_pending();
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    assert_no_alloc::assert_no_alloc(|| run(&mut exec));

    // A connection made (Q, behind the lag's PDC: the ramps are the early
    // path) and one broken: both declick inside the gate.
    ed.collect();
    ed.spec_mut().connect_param(q, ramps);
    ed.spec_mut().disconnect_param(cut, ramps);
    ed.commit().expect("commits");
    exec.apply_pending();
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    assert_no_alloc::assert_no_alloc(|| run(&mut exec));

    // Ranges moved between blocks (`Editor::set_param_range`, on this
    // control thread, outside the gate): every block that reads a moved
    // range runs inside it. No commit, so nothing to apply.
    ed.collect();
    let ranges = [
        tutti_graph::ParamRange::new(0.0, 0.5),
        tutti_graph::ParamRange::new(2.0, -1.0),
        tutti_graph::ParamRange::UNBOUNDED,
    ];
    for i in 0..60 {
        ed.set_param_range(cut, ranges[i % 3]).expect("a range");
        ed.set_param_range(q, ranges[(i + 1) % 3]).expect("a range");
        let n = sizes[i % sizes.len()];
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(n, &transport, &[&input[..n]], &mut [&mut out[..n]]);
        });
    }
    assert_eq!(ed.in_flight(), 0, "a range move sends nothing");
    let plan = exec.plan().expect("a plan");
    assert!(
        plan.delays()
            .iter()
            .any(|d| matches!(d.key, tutti_graph::DelayKey::ParamEvent { .. })),
        "a param source was delayed by PDC inside the gate"
    );
}

#[test]
fn modulated_params_are_allocation_free() {
    modulated_params_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_modulated_params_are_allocation_free() {
    modulated_params_are_allocation_free_on(4);
}

/// `contract::Direct`, the by-hand driver an instrument's allocation gate
/// runs its node through, allocates nothing per block while it plays a full
/// block of events into a node's event input, and while it collects a
/// node's event output (both lists are reserved at `new`), under a rolling
/// transport.
#[test]
fn direct_driving_events_does_not_allocate() {
    use tutti_graph::contract::{Direct, Emitter, Pulse};
    use tutti_graph::{Event, Offset};
    use tutti_types::{Bpm, SampleRate};

    let rate = SampleRate(48_000.0);
    let rolling = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut pulse = Direct::new(Pulse::new(Latency::ZERO), rate, 64);
    pulse.set_transport(rolling);
    let events: Vec<Event> = (0..64)
        .map(|k| {
            Event::midi(
                Offset::new(k, Samples(64)).expect("inside"),
                m1_note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(100)),
            )
        })
        .collect();
    let note = EventKind::Midi(m1_note_on(
        MidiChannel::FIRST,
        NoteNumber::of(60),
        U7::of(100),
    ));
    let mut emitter = Direct::new(Emitter::new(Frame(70), note), rate, 64);
    emitter.set_transport(rolling);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            pulse.events(0, &events);
            pulse.block();
            emitter.block();
        }
    });
    assert!(
        pulse.output(0).iter().any(|&s| s != 0.0),
        "the events played"
    );
}

/// Reporting a tail and parking on it never allocates: pulses into ringers
/// that narrow their ceiling every block, through the serial executor and a
/// pool whose every participant runs inside the gate.
fn reporting_nodes_are_allocation_free_on(workers: usize) {
    use common::{Pulse, Ringer};
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    let mut outputs = Vec::new();
    for i in 0..4u64 {
        let (src, fx) = (NodeKey(10 + i), NodeKey(20 + i));
        ed.insert(src, "pulse", Unforkable(Pulse::new(100 + 40 * i as usize)));
        ed.insert(
            fx,
            "ringer",
            Unforkable(Ringer::new(Tail::Finite(Samples(4_000)), 200, 200)),
        );
        ed.spec_mut()
            .topology
            .edges
            .insert(at(fx.0, 0), vec![from(src.0, 0)]);
        outputs.push(Source::Node(OutPort { node: fx, port: 0 }));
    }
    ed.spec_mut().topology.set_outputs(outputs);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    let [mut a, mut b, mut c, mut d] = std::array::from_fn(|_| vec![0.0f32; 64]);
    let mut heard = false;
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            exec.process(
                64,
                &Transport::default(),
                &[],
                &mut [&mut a[..], &mut b[..], &mut c[..], &mut d[..]],
            );
            heard |= [&a, &b, &c, &d].iter().any(|o| o.iter().any(|&x| x != 0.0));
        }
    });
    assert!(heard, "the gate rendered signal");
}

#[test]
fn reporting_nodes_are_allocation_free() {
    reporting_nodes_are_allocation_free_on(1);
}

#[test]
fn parallel_reporting_nodes_are_allocation_free() {
    reporting_nodes_are_allocation_free_on(4);
}

/// Audio fan-in never allocates: `Op::Sum` over 8, 64 and 100 sources (one
/// op, one full op, and a chain of two), at a node's port and at a global
/// output channel, with every source live, every source silent, and sources
/// that fall silent one by one (a pulse each), so the silent-addend skip, the
/// all-silent fill and the in-place form all run inside the gate, on the
/// parallel executor's helpers too.
fn fan_in_is_allocation_free_on(workers: usize) {
    use common::Pulse;
    for n in [8u64, 64, 100] {
        for source in ["live", "silent", "pulses"] {
            for at_port in [false, true] {
                let (mut ed, mut exec) = Editor::new(prepare(256));
                use_pool(&mut exec, workers);
                for k in 1..=n {
                    let name = "src";
                    match source {
                        "live" => ed.insert(
                            NodeKey(k),
                            name,
                            Unforkable(TestNode::new(Kind::Const {
                                value: 0.01 * k as f32,
                                width: 1,
                            })),
                        ),
                        "silent" => ed.insert(
                            NodeKey(k),
                            name,
                            Unforkable(TestNode::new(Kind::Const {
                                value: 0.0,
                                width: 1,
                            })),
                        ),
                        _ => ed.insert(
                            NodeKey(k),
                            name,
                            Unforkable(Pulse::new(300 + 17 * k as usize)),
                        ),
                    };
                }
                let sink = NodeKey(n + 1);
                if at_port {
                    ed.insert(
                        sink,
                        "thru",
                        Unforkable(TestNode::new(Kind::Thru { width: 1 })),
                    );
                    for k in 1..=n {
                        ed.spec_mut().topology.connect(at(sink.0, 0), from(k, 0));
                    }
                    ed.spec_mut()
                        .topology
                        .set_outputs(vec![Source::Node(OutPort {
                            node: sink,
                            port: 0,
                        })]);
                } else {
                    ed.spec_mut().topology.outputs = vec![
                        (1..=n)
                            .map(|k| {
                                Source::Node(OutPort {
                                    node: NodeKey(k),
                                    port: 0,
                                })
                            })
                            .collect(),
                    ];
                }
                ed.commit().expect("commits");
                exec.apply_pending();
                ed.collect();
                assert_eq!(exec.is_parallel(), workers > 1);
                let mut out = vec![0.0f32; 128];
                let mut heard = false;
                assert_no_alloc::assert_no_alloc(|| {
                    for _ in 0..40 {
                        exec.process(128, &Transport::default(), &[], &mut [&mut out[..]]);
                        heard |= out.iter().any(|&x| x != 0.0);
                    }
                });
                assert_eq!(
                    heard,
                    source != "silent",
                    "n = {n}, {source}, port {at_port}"
                );
            }
        }
    }
}

#[test]
fn fan_in_is_allocation_free() {
    fan_in_is_allocation_free_on(1);
}

#[test]
fn parallel_fan_in_is_allocation_free() {
    fan_in_is_allocation_free_on(4);
}

/// A weighted fan-in never allocates, ramping: 64 lanes (one full op) and 100
/// (a chain of two), at a node's port and at a global output channel, every
/// lane weighted, every one moved before every block (so each block ramps
/// every lane), with live and pulsing sources (the silent-addend skip runs
/// with weights), on the parallel executor's helpers too. The moves are the
/// control thread's; every block renders inside the gate.
///
/// With `driven`, every lane is also driven by one of four lane nodes (two of
/// them declaring latency, so the lanes behind them get event rings): each
/// lane runs the param port's step, with its clamp and its values, per block.
fn weighted_fan_in_is_allocation_free_on(workers: usize, driven: bool) {
    use common::Pulse;
    use tutti_graph::{EdgeGain, EdgeId, Sink};
    use tutti_types::Amplitude;
    for n in [64u64, 100] {
        for at_port in [false, true] {
            let (mut ed, mut exec) = Editor::new(prepare(256));
            use_pool(&mut exec, workers);
            for k in 1..=n {
                if k % 2 == 0 {
                    ed.insert(
                        NodeKey(k),
                        "src",
                        Unforkable(TestNode::new(Kind::Const {
                            value: 0.01 * k as f32,
                            width: 1,
                        })),
                    );
                } else {
                    ed.insert(
                        NodeKey(k),
                        "pulse",
                        Unforkable(Pulse::new(300 + 17 * k as usize)),
                    );
                }
            }
            let sink = NodeKey(n + 1);
            let edge_of = |k: u64| EdgeId {
                sink: if at_port {
                    Sink::Port(at(sink.0, 0))
                } else {
                    Sink::Output(0)
                },
                from: from(k, 0),
            };
            if at_port {
                ed.insert(
                    sink,
                    "thru",
                    Unforkable(TestNode::new(Kind::Thru { width: 1 })),
                );
                for k in 1..=n {
                    ed.spec_mut().topology.connect(at(sink.0, 0), from(k, 0));
                }
                ed.spec_mut()
                    .topology
                    .set_outputs(vec![Source::Node(OutPort {
                        node: sink,
                        port: 0,
                    })]);
            } else {
                ed.spec_mut().topology.outputs = vec![
                    (1..=n)
                        .map(|k| {
                            Source::Node(OutPort {
                                node: NodeKey(k),
                                port: 0,
                            })
                        })
                        .collect(),
                ];
            }
            for k in 1..=n {
                ed.spec_mut()
                    .set_edge_gain(edge_of(k), EdgeGain::new(Amplitude::new(0.5)));
            }
            if driven {
                for (i, latency) in [0usize, 0, 40, 150].into_iter().enumerate() {
                    ed.insert(
                        NodeKey(5000 + i as u64),
                        "lane",
                        Unforkable(TestNode::new(Kind::GainDriver {
                            period: 8 + 9 * i as u64,
                            latency,
                        })),
                    );
                }
                for k in 1..=n {
                    ed.spec_mut().connect_edge_gain(
                        edge_of(k),
                        EventOut {
                            node: NodeKey(5000 + k % 4),
                            port: 0,
                        },
                    );
                }
            }
            ed.commit().expect("commits");
            exec.apply_pending();
            ed.collect();
            assert_eq!(exec.is_parallel(), workers > 1);
            let mut out = vec![0.0f32; 128];
            let mut heard = false;
            for block in 0..40u64 {
                for k in 1..=n {
                    let w = ((block + k) % 7) as f32 * 0.25 - 0.5;
                    ed.set_edge_gain(edge_of(k), Amplitude::new(w))
                        .expect("an edge with an entry");
                }
                assert_no_alloc::assert_no_alloc(|| {
                    exec.process(128, &Transport::default(), &[], &mut [&mut out[..]]);
                });
                heard |= out.iter().any(|&x| x != 0.0);
            }
            assert!(heard, "n = {n}, port {at_port}");
        }
    }
}

#[test]
fn weighted_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(1, false);
}

#[test]
fn parallel_weighted_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(4, false);
}

#[test]
fn driven_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(1, true);
}

#[test]
fn parallel_driven_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(4, true);
}

/// A node of any audio width, declared latency and event ports that does
/// nothing allocating, `reset` included: audio is half the input (`0.25`
/// past the inputs), events are forwarded (or, with `tick`, made every
/// `period` frames).
struct Wire {
    ins: usize,
    outs: usize,
    latency: usize,
    ein: u16,
    eout: u16,
    tick: Option<(u64, u64)>,
    state: f32,
}

impl Wire {
    fn new(ins: usize, outs: usize, latency: usize) -> Self {
        Self {
            ins,
            outs,
            latency,
            ein: 0,
            eout: 0,
            tick: None,
            state: 0.0,
        }
    }

    fn events(mut self, ein: u16, eout: u16) -> Self {
        (self.ein, self.eout) = (ein, eout);
        self
    }

    fn ticks(period: u64, phase: u64) -> Self {
        let mut w = Self::new(0, 0, 0).events(0, 1);
        w.tick = Some((period, phase));
        w
    }
}

impl Node for Wire {
    fn shape(&self) -> Shape {
        let ch = |n| ChannelLayout::try_from(n).expect("a width");
        // The first `outs` inputs are its main bus, which its bypass and mix
        // pass by index; a wider input's rest are ports.
        let main = ch(self.ins.min(self.outs));
        Shape::buses(Buses::unassigned(main), Buses::unassigned(main))
            .with_ports(ch(self.ins), ch(self.outs))
            .with_latency(Latency::new(Samples(self.latency)))
            .with_events(self.ein, self.eout)
            .with_tail(Tail::None)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some((period, phase)) = self.tick {
            for at in cx.env.offsets() {
                let f = cx.env.frame_at(at).get() + phase;
                if f.is_multiple_of(period) {
                    // A note-on, never ended: keys are held on every port the
                    // events reach, for a bypass to close.
                    let note = 60 + (f / period) % 4;
                    let on = Ump([0x2090_0064 | ((note as u32) << 8), 0, 0, 0]);
                    let _ = io.event_out(0).push(tutti_graph::Event::midi(at, on));
                }
            }
        }
        if self.ein > 0 {
            for q in 0..self.eout as usize {
                for &e in io.events(q % self.ein as usize) {
                    let _ = io.event_out(q).push(e);
                }
            }
        }
        let n = io.frames();
        let (ins, mut outs) = io.split();
        for c in 0..self.outs {
            for i in 0..n {
                outs.get(c)[i] = if c < self.ins {
                    0.5 * ins.get(c)[i] + self.state
                } else {
                    0.25
                };
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.state = 0.0;
    }
}

/// The graph the mode and mix gates run: latent nodes (lines to feed and to
/// run), event nodes (held-key tables to observe and to close), a node with
/// no audio input, and sums, committed and running on `workers` participants.
fn mode_graph(workers: usize) -> (Editor, tutti_graph::Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let node = |ed: &mut Editor, k: u64, n: Wire| {
        ed.insert(NodeKey(k), "node", Unforkable(n));
    };
    // (The test nodes of `common` allocate in `reset`, which this gate runs
    // on every un-bypass: a node that allocates is the node's bug.)
    node(&mut ed, 1, Wire::ticks(5, 0));
    node(&mut ed, 2, Wire::ticks(7, 3));
    node(&mut ed, 3, Wire::new(0, 0, 11).events(1, 1));
    node(&mut ed, 4, Wire::new(0, 1, 0).events(1, 0));
    node(&mut ed, 5, Wire::new(1, 1, 40));
    node(&mut ed, 6, Wire::new(1, 1, 0));
    node(&mut ed, 7, Wire::new(1, 1, 150));
    node(&mut ed, 8, Wire::new(2, 1, 0));
    node(&mut ed, 9, Wire::new(1, 1, 0));
    let ev = |node, port| EventOut {
        node: NodeKey(node),
        port,
    };
    let spec = ed.spec_mut();
    spec.events.insert(
        EventIn {
            node: NodeKey(3),
            port: 0,
        },
        vec![EventEdge::Direct(ev(1, 0)), EventEdge::Direct(ev(2, 0))],
    );
    spec.events.insert(
        EventIn {
            node: NodeKey(4),
            port: 0,
        },
        vec![EventEdge::Direct(ev(3, 0))],
    );
    let t = &mut spec.topology;
    t.connect(at(5, 0), Edge::Direct(Source::Global(0)));
    t.connect(at(6, 0), from(5, 0));
    t.connect(at(7, 0), from(6, 0));
    t.connect(at(8, 0), from(4, 0));
    t.connect(at(8, 1), from(7, 0));
    t.connect(at(9, 0), from(8, 0));
    t.set_outputs(vec![Source::Node(OutPort {
        node: NodeKey(9),
        port: 0,
    })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    (ed, exec)
}

/// Toggling modes and rendering inside `assert_no_alloc`, on `workers`
/// participants (doc 013, "Bypass and mute are graph state"): every node of a
/// graph with latent nodes (lines to feed and to run), event nodes (held-key
/// tables to observe and to close), a node with no audio input, and sums, has
/// its mode moved every few blocks, so every block runs some node in a fade,
/// parked, un-bypassing (reset, warm-up) or muted. Blocks of four lengths.
fn toggling_modes_are_allocation_free_on(workers: usize) {
    use tutti_graph::NodeMode;
    let (mut ed, mut exec) = mode_graph(workers);
    let input: Vec<f32> = (0..256).map(|i| ((i % 19) as f32 - 9.0) * 0.1).collect();
    let mut out = vec![0.0f32; 256];
    let lens = [64usize, 100, 256, 7];
    let mut heard = false;
    for block in 0..120usize {
        for k in 1..=9u64 {
            // Each node on its own rhythm, so fades overlap and restart.
            let mode = match (block / (1 + k as usize % 5) + k as usize) % 3 {
                0 => NodeMode::Active,
                1 => NodeMode::Bypassed,
                _ => NodeMode::Muted,
            };
            ed.set_mode(NodeKey(k), mode).expect("a node");
        }
        let n = lens[block % lens.len()];
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(
                n,
                &Transport::default(),
                &[&input[..n]],
                &mut [&mut out[..n]],
            );
        });
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}

#[test]
fn toggling_modes_are_allocation_free() {
    toggling_modes_are_allocation_free_on(1);
}

#[test]
fn parallel_toggling_modes_are_allocation_free() {
    toggling_modes_are_allocation_free_on(4);
}

/// Moving mixes and rendering inside `assert_no_alloc`, on `workers`
/// participants (doc 013, "Bypass and mute are graph state", M4): the graph of
/// the mode gate with every node's mix moved every few blocks (the ends and
/// between, so a node runs steady WET, steady DRY, a steady blend and a ramp)
/// and a third of the nodes' modes moved with them, so a mix also meets a
/// fade, a warm-up and a parked node. Blocks of four lengths.
fn moving_mixes_are_allocation_free_on(workers: usize) {
    use tutti_graph::NodeMode;
    use tutti_types::Mix;
    let (mut ed, mut exec) = mode_graph(workers);
    let input: Vec<f32> = (0..256).map(|i| ((i % 19) as f32 - 9.0) * 0.1).collect();
    let mut out = vec![0.0f32; 256];
    let lens = [64usize, 100, 256, 7];
    let mixes = [1.0f32, 0.0, 0.5, 0.25, 1.0, 0.75, 0.0, 1.0];
    let mut heard = false;
    for block in 0..120usize {
        for k in 1..=9u64 {
            let at = block / (1 + k as usize % 3) + k as usize;
            ed.set_mix(NodeKey(k), Mix::new(mixes[at % mixes.len()]))
                .expect("a node");
            if k % 3 == 0 {
                let mode = match (block / 7 + k as usize) % 3 {
                    0 => NodeMode::Active,
                    1 => NodeMode::Bypassed,
                    _ => NodeMode::Muted,
                };
                ed.set_mode(NodeKey(k), mode).expect("a node");
            }
        }
        let n = lens[block % lens.len()];
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(
                n,
                &Transport::default(),
                &[&input[..n]],
                &mut [&mut out[..n]],
            );
        });
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}

#[test]
fn moving_mixes_are_allocation_free() {
    moving_mixes_are_allocation_free_on(1);
}

#[test]
fn parallel_moving_mixes_are_allocation_free() {
    moving_mixes_are_allocation_free_on(4);
}

/// Driven mixes and rendering inside `assert_no_alloc`, on `workers`
/// participants (doc 013, "Bypass and mute are graph state", M4b): the graph
/// of the mode gate, with a commit that then drives the mix of five of its
/// kept nodes from lanes of four periods and latencies (two of them late
/// enough to give a node an event ring), and a mix, a range and a third of the
/// modes moved every few blocks, so a lane meets a steady blend, a fade, a
/// warm-up, a parked node and a lane that has just been let go (a source
/// disconnected halfway, its fade running out). Blocks of four lengths.
fn driven_mixes_are_allocation_free_on(workers: usize) {
    use tutti_graph::{NodeMix, NodeMode, ParamIn, ParamRange};
    use tutti_types::Mix;
    let (mut ed, mut exec) = mode_graph(workers);
    for (i, latency) in [0usize, 0, 40, 150].into_iter().enumerate() {
        ed.insert(
            NodeKey(5000 + i as u64),
            "lane",
            Unforkable(TestNode::new(Kind::MixDriver {
                period: 8 + 9 * i as u64,
                latency,
            })),
        );
    }
    let port = |k: u64| ParamIn {
        node: NodeKey(k),
        param: NodeMix::ADDR,
    };
    let lane = |k: u64| EventOut {
        node: NodeKey(5000 + k % 4),
        port: 0,
    };
    for k in 5..=9u64 {
        ed.spec_mut().connect_param(port(k), lane(k));
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let input: Vec<f32> = (0..256).map(|i| ((i % 19) as f32 - 9.0) * 0.1).collect();
    let mut out = vec![0.0f32; 256];
    let lens = [64usize, 100, 256, 7];
    let mixes = [1.0f32, 0.0, 0.5, 0.25, 1.0, 0.75, 0.0, 1.0];
    let ranges = [(0.0f32, 1.0f32), (0.25, 0.75), (-2.0, 2.0)];
    let mut heard = false;
    for block in 0..120usize {
        if block == 60 {
            // Let go of two sources: their lanes fade to the base.
            for k in [6u64, 8] {
                ed.spec_mut().disconnect_param(port(k), lane(k));
            }
            ed.commit().expect("commits");
            exec.apply_pending();
            ed.collect();
        }
        for k in 1..=9u64 {
            let at = block / (1 + k as usize % 3) + k as usize;
            ed.set_mix(NodeKey(k), Mix::new(mixes[at % mixes.len()]))
                .expect("a node");
            if k >= 5 && block % 11 == k as usize {
                let (min, max) = ranges[(block / 11 + k as usize) % ranges.len()];
                ed.set_param_range(port(k), ParamRange::new(min, max))
                    .expect("a node");
            }
            if k % 3 == 0 {
                let mode = match (block / 7 + k as usize) % 3 {
                    0 => NodeMode::Active,
                    1 => NodeMode::Bypassed,
                    _ => NodeMode::Muted,
                };
                ed.set_mode(NodeKey(k), mode).expect("a node");
            }
        }
        let n = lens[block % lens.len()];
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(
                n,
                &Transport::default(),
                &[&input[..n]],
                &mut [&mut out[..n]],
            );
        });
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}

#[test]
fn driven_mixes_are_allocation_free() {
    driven_mixes_are_allocation_free_on(1);
}

#[test]
fn parallel_driven_mixes_are_allocation_free() {
    driven_mixes_are_allocation_free_on(4);
}

/// Edge fades (doc 030 section 4) inside `assert_no_alloc`, on `workers`
/// participants: eight constants summed at the output, each edge fading in or
/// out over several blocks of different lengths, the ended fades reported on
/// the ring (a push of a plain value), then a block rendered after the editor
/// settled them (outside the gate: settling edits the spec) and recommitted.
fn fading_edges_are_allocation_free_on(workers: usize) {
    use tutti_graph::{EdgeFade, EdgeGain, EdgeId, Sink};
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    for k in 1..=8u64 {
        ed.insert(
            NodeKey(k),
            "const",
            Unforkable(TestNode::new(Kind::Const {
                value: k as f32,
                width: 1,
            })),
        );
    }
    let edge = |k: u64| EdgeId {
        sink: Sink::Output(0),
        from: from(k, 0),
    };
    ed.spec_mut().topology.outputs = vec![
        (1..=8u64)
            .map(|k| {
                Source::Node(OutPort {
                    node: NodeKey(k),
                    port: 0,
                })
            })
            .collect(),
    ];
    for k in 1..=8u64 {
        ed.spec_mut()
            .set_edge_gain(edge(k), EdgeGain::new(tutti_types::Amplitude::new(0.5)));
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    for k in 1..=8u64 {
        let fade = if k % 2 == 0 {
            EdgeFade::In
        } else {
            EdgeFade::Out
        };
        ed.set_edge_fade(edge(k), fade).expect("edge");
    }
    // Weights that move under the fades, set between blocks.
    let lens = [64usize, 17, 64, 5, 33];
    let mut out = vec![0.0f32; 64];
    let mut heard = false;
    for block in 0..14 {
        if block % 3 == 0 {
            for k in 1..=8u64 {
                let w = 0.25 * ((block + k as usize) % 5) as f32;
                ed.set_edge_gain(edge(k), tutti_types::Amplitude::new(w))
                    .expect("edge");
            }
        }
        let n = lens[block % lens.len()];
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(n, &Transport::default(), &[], &mut [&mut out[..n]]);
        });
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    ed.collect();
    assert!(ed.settle_fades(), "the ended fades were reported");
}

#[test]
fn fading_edges_are_allocation_free() {
    fading_edges_are_allocation_free_on(1);
}

#[test]
fn parallel_fading_edges_are_allocation_free() {
    fading_edges_are_allocation_free_on(4);
}

/// A node with state to hand over: it writes `STATE` messages when replaced
/// (inside the gate, which the executor's chase buffer must satisfy) and
/// counts every event it is sent.
struct Chaser {
    seen: Arc<AtomicU64>,
}

const STATE: u8 = 4;

impl Node for Chaser {
    fn shape(&self) -> Shape {
        Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::MONO))
            .with_events(1, 0)
            .with_chase(u32::from(STATE))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.seen
            .fetch_add(io.events(0).len() as u64, Ordering::Relaxed);
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn chase(&self, out: &mut tutti_graph::Chase<'_>) {
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..STATE {
                let controller = tutti_ump::CCNumber::of(i);
                assert!(out.push(tutti_fixtures::midi::m1_cc(
                    MidiChannel::FIRST,
                    controller,
                    U7::of(1)
                )));
            }
        });
    }
}

/// A swap's block never allocates: the replaced node writes its state into
/// the executor's buffer inside the gate, and the block that delivers it to
/// the successor (and every block after) runs inside it. Applying the commit
/// itself allocates, as it always does, and runs outside.
fn a_swap_is_allocation_free_on(workers: usize) {
    const NODES: u64 = 4;
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    let counts = |_| (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let seen: Vec<_> = (0..NODES).map(counts).collect();
    for (k, (first, _)) in (1..).zip(&seen) {
        ed.insert(
            NodeKey(k),
            "chaser",
            Unforkable(Chaser {
                seen: Arc::clone(first),
            }),
        );
    }
    ed.spec_mut().topology.set_outputs((1..=NODES).map(|k| {
        Source::Node(OutPort {
            node: NodeKey(k),
            port: 0,
        })
    }));
    ed.commit().expect("commits");
    let transport = Transport::default();
    let mut outs: [Vec<f32>; NODES as usize] = std::array::from_fn(|_| vec![0.0f32; 256]);
    let mut block = |exec: &mut tutti_graph::Executor, n: usize| {
        let [a, b, c, d] = &mut outs;
        exec.process(
            n,
            &transport,
            &[],
            &mut [&mut a[..n], &mut b[..n], &mut c[..n], &mut d[..n]],
        );
    };
    block(&mut exec, 256);
    for (k, (_, second)) in (1..).zip(&seen) {
        ed.replace(
            NodeKey(k),
            Unforkable(Chaser {
                seen: Arc::clone(second),
            }),
            Fade::new(Samples(0), CrossfadeCurve::EqualPower),
        )
        .expect("fits");
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for n in [256usize, 1, 7, 64] {
            block(&mut exec, n);
        }
    });
    for (first, second) in &seen {
        assert_eq!(second.load(Ordering::Relaxed), u64::from(STATE));
        assert_eq!(first.load(Ordering::Relaxed), 0);
    }
    assert_eq!(exec.dropped_event_count(), 0);
}

#[test]
fn a_swap_is_allocation_free() {
    a_swap_is_allocation_free_on(1);
}

#[test]
fn parallel_swap_is_allocation_free() {
    a_swap_is_allocation_free_on(4);
}
