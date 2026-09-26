//! `Editor::replace`: swapping a running node's unit with a crossfade (doc 013
//! Phase 3, gap 3). The rules are in `src/fade.rs`; each test here pins one,
//! and names the mutation it was seen to fail under.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use common::{prepare, Kind, TestNode};
use tutti_graph::{
    compile, CommitError, CrossfadeCurve, Cx, Editor, EventIn, EventKind, Executor, Fade, Io, Node,
    Prepare, Shape, Status, Transport, Ump, Unforkable,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{At, AudioThread, ChannelLayout, NodeKey, Samples};

const NODE: NodeKey = NodeKey(1);

fn gain(g: f32) -> TestNode {
    TestNode::new(Kind::Gain { gain: g, width: 1 })
}

/// Global input 0 → `NODE` (a one-channel gain, aliased in place) → output
/// 0, committed and applied.
fn gain_graph(g: f32) -> (Editor, Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(NODE, "gain", Unforkable(gain(g)));
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NODE,
            port: 0,
        },
        Edge::Direct(Source::Global(0)),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NODE,
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(
        exec.plan().expect("applied").in_place(NODE).get(0),
        "the fade must run over an in-place op: the outgoing unit reads a slot the incoming one overwrites"
    );
    (ed, exec)
}

/// Render `n` frames of DC 1.0 into the graph; returns output 0.
fn dc(exec: &mut Executor, n: usize) -> Vec<f32> {
    let input = vec![1.0f32; n];
    let mut out = vec![0.0f32; n];
    exec.process(n, &Transport::default(), &[&input[..]], &mut [&mut out[..]]);
    out
}

/// Blocks of uneven length, so fades start and end mid-block.
const RAGGED: [usize; 7] = [7, 13, 64, 1, 50, 128, 33];

/// No step at either end of a fade: on a DC input, a gain of 1 replaced by a
/// gain of 3 moves by at most one fade step per sample — the steepest slope
/// of the curve over `len + 1` steps (15/8 · 2 for equal amplitude, √10 · π/2
/// for equal power; 5 bounds both) — where a swap moves by 2 in one sample.
///
/// Mutation: blend with `g_out = 0` (drop the outgoing unit) → the first fade
/// frame falls to ~0 → fails. Mutation: skip the blend → a step of 2 at the
/// swap → fails. Mutation: run the outgoing unit *after* the incoming one →
/// it reads the incoming unit's in-place output (gain 3 · 1) instead of the
/// input → the old half is 3× too loud → fails the slope bound.
#[test]
fn a_fade_has_no_step_at_either_end() {
    for curve in [CrossfadeCurve::EqualAmplitude, CrossfadeCurve::EqualPower] {
        let len = 100;
        let (mut ed, mut exec) = gain_graph(1.0);
        let mut out = dc(&mut exec, 40);
        ed.replace(NODE, Unforkable(gain(3.0)), Fade::new(Samples(len), curve))
            .expect("same shape");
        ed.commit().expect("commits");
        for &n in RAGGED.iter().cycle().take(12) {
            out.extend(dc(&mut exec, n));
        }
        let bound = 5.0 / (len + 1) as f32;
        let worst = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst <= bound,
            "{curve:?}: a step of {worst} (bound {bound})"
        );
        assert_eq!(out[0], 1.0);
        assert_eq!(*out.last().expect("rendered"), 3.0);
    }
}

/// The fade is exactly its length: from the first frame of the block the
/// commit lands on, `len` frames mix both units (strictly between the two
/// levels), and the frame after is the incoming unit alone — across ragged
/// blocks, so the end falls mid-block.
///
/// Mutation: `x = (k + 1) / len` in `CrossfadeCurve::gains` → the last fade
/// frame is already 3 → fails. Mutation: in `fading_node_op`, blend the
/// whole block (`n = frames`) → the gains are asked past the end → panics
/// in debug. (Never *ending* a fade leaves this output right — past its
/// length nothing is blended — and is caught by the retirement and waiting
/// tests below instead.)
#[test]
fn the_fade_is_exactly_its_length() {
    let len = 100;
    let (mut ed, mut exec) = gain_graph(1.0);
    let mut out = dc(&mut exec, 20);
    let start = out.len();
    ed.replace(
        NODE,
        Unforkable(gain(3.0)),
        Fade::new(Samples(len), CrossfadeCurve::EqualAmplitude),
    )
    .expect("same shape");
    ed.commit().expect("commits");
    for &n in RAGGED.iter().cycle().take(10) {
        out.extend(dc(&mut exec, n));
    }
    assert!(out[..start].iter().all(|&y| y == 1.0));
    for (k, &y) in out[start..start + len].iter().enumerate() {
        assert!(y > 1.0 && y < 3.0, "fade frame {k}: {y}");
    }
    assert!(
        out[start + len..].iter().all(|&y| y == 3.0),
        "after the fade: {:?}",
        &out[start + len..start + len + 4]
    );
}

/// A unit that records where it was dropped: 1 off the audio thread, 2 on it.
struct DropProbe {
    inner: TestNode,
    dropped: Arc<AtomicU8>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        let on_audio = AudioThread::is_current();
        self.dropped
            .store(if on_audio { 2 } else { 1 }, Ordering::SeqCst);
    }
}

impl Node for DropProbe {
    fn shape(&self) -> Shape {
        self.inner.shape()
    }
    fn prepare(&mut self, p: &Prepare) {
        self.inner.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.inner.process(cx, io)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// The outgoing unit retires on the control thread: the commit that started
/// the fade comes back at once, and the crossfade comes back on the
/// fade-return ring when it ends, carrying the outgoing unit, which
/// `collect` frees — never the executor.
///
/// Mutation: in `end_fades`, `drop` the ended crossfade instead of
/// `retire_fade` → the outgoing unit is freed on the audio thread (the
/// guards panic in debug; the probe would read 2) → fails. Mutation: never
/// end a fade (skip `end_fades`) → the outgoing unit never comes back →
/// fails. Mutation: don't drain the fade-return ring in `collect` → fails.
#[test]
fn the_outgoing_unit_retires_on_the_control_thread() {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let dropped = Arc::new(AtomicU8::new(0));
    ed.insert(
        NODE,
        "gain",
        Unforkable(DropProbe {
            inner: gain(1.0),
            dropped: Arc::clone(&dropped),
        }),
    );
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NODE,
            port: 0,
        },
        Edge::Direct(Source::Global(0)),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NODE,
        port: 0,
    })];
    ed.commit().expect("commits");
    dc(&mut exec, 16);
    ed.collect();

    ed.replace(
        NODE,
        Unforkable(gain(2.0)),
        Fade::new(Samples(100), CrossfadeCurve::EqualPower),
    )
    .expect("same shape");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    assert!(ed.collect().is_empty(), "nothing retires mid-fade");
    assert_eq!(ed.in_flight(), 0, "the fade does not hold its commit");
    assert_eq!(ed.fades_in_flight(), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);

    dc(&mut exec, 64);
    // The fade ended in that block: the box is back in the return ring, and
    // the unit in it is alive until the control thread drains it.
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "not freed by the executor"
    );
    assert_eq!(ed.collect(), vec![NODE]);
    assert_eq!(dropped.load(Ordering::SeqCst), 1, "freed by collect");
    assert_eq!(ed.fades_in_flight(), 0);
}

/// A fade does not hold its commit: ten replaces, each committed on its own
/// with a one-second fade still running, all go through, and so does a
/// re-prepare — `Backpressure` is about commits in the queue, not fades in
/// flight.
///
/// Mutation: hold a fade's commit until the fade ends (the design this
/// replaced) → the fifth commit is `Backpressure` → fails. Mutation: set
/// `FADE_CAPACITY` to 4 → the fifth is refused → fails.
#[test]
fn long_fades_do_not_block_commits() {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    let keys: Vec<NodeKey> = (1..=10).map(NodeKey).collect();
    for &k in &keys {
        ed.insert(k, "gain", Unforkable(gain(1.0)));
    }
    ed.commit().expect("commits");
    exec.process(64, &Transport::default(), &[], &mut []);
    let second = Fade::new(Samples(48_000), CrossfadeCurve::EqualAmplitude);
    for &k in &keys {
        ed.replace(k, Unforkable(gain(2.0)), second).expect("fits");
        ed.commit().expect("a running fade blocks nothing");
        exec.process(64, &Transport::default(), &[], &mut []);
    }
    assert_eq!(ed.fades_in_flight(), 10);
    ed.reprepare(prepare(64)).expect("nor a re-prepare");
}

/// A remove in the same commit as a long fade retires at once: its key is
/// reported by the `collect` after the next block, not after the fade.
///
/// Mutation: hold a fade's commit until the fade ends → the removed key
/// comes back only after the fade → fails.
#[test]
fn a_remove_beside_a_fade_retires_at_once() {
    let (mut ed, mut exec) = gain_graph(1.0);
    let other = NodeKey(2);
    ed.insert(other, "gain", Unforkable(gain(1.0)));
    ed.commit().expect("commits");
    dc(&mut exec, 16);
    ed.collect();
    ed.replace(
        NODE,
        Unforkable(gain(2.0)),
        Fade::new(Samples(48_000), CrossfadeCurve::EqualAmplitude),
    )
    .expect("fits");
    ed.remove(other);
    ed.commit().expect("commits");
    dc(&mut exec, 16);
    assert_eq!(ed.collect(), vec![other]);
    assert_eq!(ed.fades_in_flight(), 1, "the fade still runs");
}

/// A replace whose unit differs in shape is refused, naming the key, and
/// changes nothing: other ports, another latency, another in-place
/// acceptance, or another declared event capacity. A node with nothing running is refused too. And the verifier
/// refuses the same fade handed over in a delta built by hand.
///
/// Mutation: drop the latency comparison from `Editor::replace` → the
/// latency case is accepted → fails. Mutation: drop it from `verify_fades`
/// → `package` sends the hand-built delta → fails. Mutation: drop the
/// event-capacity comparison from `Editor::replace` → the capacity case is
/// accepted → fails.
#[test]
fn a_shape_mismatch_is_refused() {
    let fade = Fade::new(Samples(64), CrossfadeCurve::EqualAmplitude);
    let (mut ed, mut exec) = gain_graph(1.0);
    let wider = TestNode::new(Kind::Gain {
        gain: 1.0,
        width: 2,
    });
    assert_eq!(
        ed.replace(NODE, Unforkable(wider), fade),
        Err(CommitError::FadeShape { node: NODE })
    );
    let not_in_place = TestNode::new(Kind::Thru { width: 1 });
    assert_eq!(
        ed.replace(NODE, Unforkable(not_in_place), fade),
        Err(CommitError::FadeShape { node: NODE })
    );
    assert_eq!(
        ed.replace(NodeKey(9), Unforkable(gain(1.0)), fade),
        Err(CommitError::NotRunning { node: NodeKey(9) })
    );
    ed.commit().expect("nothing changed");
    assert_eq!(
        exec.plan().expect("applied").unit(NODE).expect("kept").gen,
        0
    );

    // Latency.
    let lagged = NodeKey(2);
    ed.insert(
        lagged,
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 3 })),
    );
    ed.spec_mut().topology.outputs.push(Source::Node(OutPort {
        node: lagged,
        port: 0,
    }));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(
        ed.replace(
            lagged,
            Unforkable(TestNode::new(Kind::Lag { latency: 5 })),
            fade
        ),
        Err(CommitError::FadeShape { node: lagged })
    );
    ed.replace(
        lagged,
        Unforkable(TestNode::new(Kind::Lag { latency: 3 })),
        fade,
    )
    .expect("the same latency fades");

    // Declared event capacity: the plan sizes the port's buffers from it.
    let burst = |cap| {
        TestNode::new(Kind::Burst {
            period: 200,
            phase: 0,
            burst: 1,
            cap,
        })
    };
    let bursting = NodeKey(3);
    ed.insert(bursting, "burst", Unforkable(burst(2)));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(
        ed.replace(bursting, Unforkable(burst(3)), fade),
        Err(CommitError::FadeShape { node: bursting })
    );
    ed.replace(bursting, Unforkable(burst(2)), fade)
        .expect("the same capacity fades");

    // The verifier, on a delta built by hand: a fade across a latency change.
    let (mut ed, mut exec) = Editor::new(prepare(128));
    ed.insert(
        lagged,
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 3 })),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: lagged,
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut spec = ed.spec().clone();
    spec.generations.insert(lagged, 1);
    let later = Kind::Lag { latency: 5 };
    let shape = TestNode::new(later.clone()).shape();
    spec.topology.nodes.insert(
        lagged,
        tutti_types::graph::NodeSpec::new("lag", shape.audio_in, shape.audio_out)
            .with_latency(shape.latency.samples())
            .with_tail(shape.tail),
    );
    let shapes: tutti_graph::Shapes = [(lagged, shape)].into_iter().collect();
    let valid = spec.validate().expect("valid");
    let (plan, mut delta) =
        compile(&valid, &shapes, &prepare(128), ed.base().map(|p| &**p)).expect("compiles");
    delta.fades = vec![(lagged, fade)];
    let units: BTreeMap<NodeKey, Box<dyn Node>> =
        [(lagged, Box::new(TestNode::new(later)) as Box<dyn Node>)]
            .into_iter()
            .collect();
    match ed.package(plan, delta, units) {
        Err(CommitError::Fade(e)) => assert!(e.0.contains("node 2"), "{e}"),
        other => panic!("a fade across a latency change was packaged: {other:?}"),
    }
}

/// A replace while a fade runs waits for it, then fades from its incoming
/// unit — from the block after the running fade ends; a third replace
/// while one waits supersedes the waiting unit, which never runs and
/// retires on the control thread. Checked sample for sample against the
/// rule written out here: gains 1 → 2 over 100 frames, then 2 → 8 over 50.
///
/// Mutation: start a later fade at once (in `apply`, treat a key with a
/// fade running like one without) → the running fade is lost (and freed on
/// the audio thread, which the unit box's guard refuses) → fails. Mutation:
/// in `end_fades`,
/// never start the waiting fade → the output stays at 2 → fails. Mutation:
/// let a later replace *not* supersede the waiting one (keep the first) →
/// the gain ends at 4 → fails.
#[test]
fn a_replace_during_a_fade_waits_for_it() {
    let (mut ed, mut exec) = gain_graph(1.0);
    let fade = |len| Fade::new(Samples(len), CrossfadeCurve::EqualAmplitude);
    ed.replace(NODE, Unforkable(gain(2.0)), fade(100))
        .expect("fits");
    ed.commit().expect("commits");
    let mut out = dc(&mut exec, 25);

    let never = Arc::new(AtomicUsize::new(0));
    let waiting = TestNode::counted(
        Kind::Gain {
            gain: 4.0,
            width: 1,
        },
        Arc::clone(&never),
    );
    ed.replace(NODE, Unforkable(waiting), fade(50))
        .expect("fits");
    ed.commit().expect("commits");
    out.extend(dc(&mut exec, 25));
    ed.replace(NODE, Unforkable(gain(8.0)), fade(50))
        .expect("fits");
    ed.commit().expect("commits");
    for _ in 0..8 {
        out.extend(dc(&mut exec, 25));
    }
    assert_eq!(
        never.load(Ordering::Relaxed),
        0,
        "a superseded unit never runs"
    );

    let expect: Vec<f32> = (0..out.len())
        .map(|f| match f {
            0..100 => {
                let (i, o) = CrossfadeCurve::EqualAmplitude.gains(f, 100);
                2.0 * i + 1.0 * o
            }
            100..150 => {
                let (i, o) = CrossfadeCurve::EqualAmplitude.gains(f - 100, 50);
                8.0 * i + 2.0 * o
            }
            _ => 8.0,
        })
        .collect();
    assert_eq!(out, expect);

    // Every unit that left came back to the control thread: the first
    // outgoing unit, the superseded one, and the second outgoing one.
    let mut back = ed.collect();
    back.sort();
    assert_eq!(back, vec![NODE; 3]);
    assert_eq!(ed.fades_in_flight(), 0, "every crossfade came back");
}

/// Counts the events it is handed; outputs the count.
struct EventCount(Arc<AtomicUsize>);

impl Node for EventCount {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.0.fetch_add(io.events(0).len(), Ordering::Relaxed);
        let n = self.0.load(Ordering::Relaxed) as f32;
        io.output(0).fill(n);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// During a fade, new events go to the incoming unit only: the outgoing one
/// finishes what it was sounding and hears nothing new.
///
/// Mutation: hand the outgoing unit the port's events (the incoming unit's
/// `SortedEvents`) → it counts the events sent during the fade → fails.
#[test]
fn events_go_to_the_incoming_unit_only() {
    // Events arrive both ways: from an emitter's edge (one every 16 frames)
    // and as scheduled commands.
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let (old, new) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let emitter = NodeKey(2);
    ed.insert(
        emitter,
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 16,
            phase: 0,
        })),
    );
    ed.insert(NODE, "count", Unforkable(EventCount(Arc::clone(&old))));
    let to = EventIn {
        node: NODE,
        port: 0,
    };
    ed.spec_mut().connect_events(
        to,
        tutti_graph::EventEdge::Direct(tutti_graph::EventOut {
            node: emitter,
            port: 0,
        }),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NODE,
        port: 0,
    })];
    ed.commit().expect("commits");
    let note = EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0]));
    let mut out = vec![0.0f32; 64];
    let mut block = |exec: &mut Executor| {
        exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    };
    ed.schedule(At::NextBlock, to, note).expect("room");
    block(&mut exec);
    assert_eq!(old.load(Ordering::Relaxed), 4 + 1);

    ed.replace(
        NODE,
        Unforkable(EventCount(Arc::clone(&new))),
        Fade::new(Samples(200), CrossfadeCurve::EqualPower),
    )
    .expect("same shape");
    ed.commit().expect("commits");
    for _ in 0..3 {
        ed.schedule(At::NextBlock, to, note).expect("room");
        block(&mut exec);
    }
    assert_eq!(
        old.load(Ordering::Relaxed),
        5,
        "the outgoing unit heard nothing new"
    );
    assert_eq!(new.load(Ordering::Relaxed), 3 * (4 + 1));
}

/// A hard edit at a fading key cuts the fade: removing the node, or an
/// `insert` over it, retires every unit there — the running one, the one
/// fading out and one waiting — and every one is reported by `collect`. A
/// re-prepare cuts it too, keeping the newest unit and reporting the two it
/// cut.
///
/// Mutation: in `apply`, retire a hard-replaced unit without its crossfades
/// (forget them instead of `retire_fade`) → two units never come back and
/// `fades_in_flight` stays up → fails. The same on suspend → the
/// re-prepare's cut units are never reported → fails. Mutation: on suspend,
/// check out the running unit rather than the waiting one → after the
/// re-prepare the gain is 2, not 4 → fails.
#[test]
fn a_hard_edit_cuts_a_fade() {
    let fade = Fade::new(Samples(1000), CrossfadeCurve::EqualAmplitude);
    for cut in ["remove", "insert"] {
        let (mut ed, mut exec) = gain_graph(1.0);
        ed.replace(NODE, Unforkable(gain(2.0)), fade).expect("fits");
        ed.commit().expect("commits");
        dc(&mut exec, 64);
        ed.replace(NODE, Unforkable(gain(4.0)), fade).expect("fits");
        ed.commit().expect("commits");
        dc(&mut exec, 64);
        assert_eq!(ed.fades_in_flight(), 2, "one running, one waiting");
        match cut {
            "remove" => {
                ed.remove(NODE);
            }
            _ => {
                ed.insert(NODE, "gain", Unforkable(gain(8.0)));
            }
        }
        ed.commit().expect("commits");
        let out = dc(&mut exec, 64);
        let back = ed.collect();
        assert_eq!(ed.fades_in_flight(), 0, "{cut}: every crossfade came back");
        assert_eq!(back.len(), 3, "{cut}: three units retired: {back:?}");
        if cut == "insert" {
            assert!(out.iter().all(|&y| y == 8.0), "a swap, not a fade");
        }
    }

    let (mut ed, mut exec) = gain_graph(1.0);
    ed.replace(NODE, Unforkable(gain(2.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    ed.replace(NODE, Unforkable(gain(4.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    ed.reprepare(prepare(64)).expect("re-prepares");
    dc(&mut exec, 64);
    assert_eq!(
        ed.collect(),
        vec![NODE; 2],
        "the re-prepare's cut units are reported"
    );
    let out = dc(&mut exec, 64);
    assert!(
        out.iter().all(|&y| y == 4.0),
        "the newest unit: {:?}",
        &out[..4]
    );
    ed.collect();
    assert_eq!(ed.in_flight(), 0);
    assert_eq!(ed.fades_in_flight(), 0);
}

/// `set_latency` at a key that is mid-fade is a hard edit for the fade: the
/// next commit cuts it, keeping the newest unit (here the one waiting), and
/// the two units it retires — the one fading out and the one displaced —
/// are reported by `collect`. Both units of a fade must share the latency
/// the plan compensates, so a fade cannot run across the change.
///
/// Mutation: don't attach `delta.cuts` in `Editor::commit` → the fade runs
/// on (the output is not yet 4) → fails. Mutation: in `cut_fades`, keep the
/// running unit rather than the waiting one → the gain is 2 → fails.
/// Mutation: in `cut_fades`, forget the cut crossfades → they are never
/// reported → fails.
#[test]
fn set_latency_cuts_a_fade() {
    let fade = Fade::new(Samples(1000), CrossfadeCurve::EqualAmplitude);
    let (mut ed, mut exec) = gain_graph(1.0);
    ed.replace(NODE, Unforkable(gain(2.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    ed.replace(NODE, Unforkable(gain(4.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    assert!(ed.collect().is_empty());
    assert_eq!(ed.fades_in_flight(), 2, "one running, one waiting");

    ed.set_latency(NODE, tutti_types::Latency::new(Samples(3)))
        .expect("sets");
    ed.commit().expect("commits");
    let out = dc(&mut exec, 64);
    assert!(
        out.iter().all(|&y| y == 4.0),
        "the newest unit: {:?}",
        &out[..4]
    );
    assert_eq!(ed.collect(), vec![NODE; 2], "the cut units are reported");
    assert_eq!(ed.fades_in_flight(), 0);
}

/// The reference derives the same rule from the graph value alone: a node
/// whose declared latency changes with no new generation has its fade cut,
/// keeping the newest unit.
///
/// Mutation: drop the latency-change cut from
/// `Reference::set_graph_with_fades` → the fade runs on → fails.
#[test]
fn the_reference_cuts_a_fade_on_a_latency_change() {
    use tutti_graph::{GraphSpec, Reference};
    use tutti_types::graph::NodeSpec;
    use tutti_types::Topology;

    let spec_at = |gen: u32, latency: usize| {
        let mut t = Topology {
            inputs: ChannelLayout::MONO,
            ..Topology::default()
        };
        t.nodes.insert(
            NODE,
            NodeSpec::new("gain", ChannelLayout::MONO, ChannelLayout::MONO)
                .with_latency(Samples(latency)),
        );
        t.edges.insert(
            InPort {
                node: NODE,
                port: 0,
            },
            Edge::Direct(Source::Global(0)),
        );
        t.outputs = vec![Source::Node(OutPort {
            node: NODE,
            port: 0,
        })];
        let mut spec = GraphSpec::new(t);
        spec.generations.insert(NODE, gen);
        spec.validate().expect("valid")
    };
    let unit = |g: f32| -> BTreeMap<NodeKey, Box<dyn Node>> {
        [(NODE, Box::new(gain(g)) as Box<dyn Node>)]
            .into_iter()
            .collect()
    };
    let fade = Fade::new(Samples(1000), CrossfadeCurve::EqualAmplitude);
    let fades: BTreeMap<NodeKey, Fade> = [(NODE, fade)].into_iter().collect();
    let mut r = Reference::new(prepare(128));
    let render = |r: &mut Reference| {
        let input = vec![1.0f32; 64];
        let mut out = vec![0.0f32; 64];
        r.process(
            64,
            &Transport::default(),
            &[&input[..]],
            &mut [&mut out[..]],
        );
        out
    };
    r.set_graph(&spec_at(0, 0), unit(1.0));
    render(&mut r);
    r.set_graph_with_fades(&spec_at(1, 0), unit(2.0), &fades);
    render(&mut r);
    r.set_graph_with_fades(&spec_at(2, 0), unit(4.0), &fades);
    render(&mut r);
    r.set_graph_with_fades(&spec_at(2, 3), unit(8.0), &fades);
    let out = render(&mut r);
    assert!(
        out.iter().all(|&y| y == 4.0),
        "the newest unit: {:?}",
        &out[..4]
    );
}
