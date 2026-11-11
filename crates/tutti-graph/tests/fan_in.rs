//! Audio fan-in, the op (doc 013, "Audio fan-in sums", staging item 3): several
//! sources wired to one input port, or to one global output channel, are
//! **summed** by `Op::Sum`, in `Edge` order, as the left fold
//! `((+0.0 + a_0) + a_1) + ..` in `f32`; one source is an alias and none is
//! silence.
//!
//! Every topology is hand-built (the session API is PR 4). Figures are
//! analytic: the expected value is the fold written out in the test. Where a
//! render is compared with another, it is the two the doc prescribes: the
//! compiled executor against the reference interpreter (`Pair`), and the
//! parallel executor against the serial one.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::{Kind, Pair, TestNode, bits, input_signal, prepare};
use tutti_graph::{
    Cx, Edge, Editor, Executor, FeedbackFrom, GraphSpec, InPort, Io, Node, NodeKey, NodeSpec, Op,
    OutPort, Plan, Prepare, Shape, Source, Status, Topology, Transport, Unforkable, compile,
};
use tutti_types::{ChannelLayout, Samples};

const BLOCK: usize = 64;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn at(node: u64, port: u16) -> InPort {
    InPort {
        node: key(node),
        port,
    }
}

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: key(node),
        port: 0,
    })
}

fn direct(node: u64) -> Edge {
    Edge::Direct(out(node))
}

fn dc(value: f32) -> Kind {
    Kind::Const { value, width: 1 }
}

fn spec_of(kind: &Kind) -> NodeSpec {
    let s = TestNode::new(kind.clone()).shape();
    NodeSpec::new("test", s.audio_in(), s.audio_out())
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

/// A topology of `kinds` keyed `1..` in the order given.
fn nodes(kinds: &[Kind]) -> (Topology, BTreeMap<NodeKey, Kind>) {
    let mut t = Topology::default();
    let mut map = BTreeMap::new();
    for (i, k) in kinds.iter().enumerate() {
        t.nodes.insert(key(i as u64 + 1), spec_of(k));
        map.insert(key(i as u64 + 1), k.clone());
    }
    (t, map)
}

fn spec(t: &Topology) -> tutti_graph::ValidGraph {
    GraphSpec::new(t.clone()).validate().expect("valid")
}

fn plan_of(t: &Topology, kinds: &BTreeMap<NodeKey, Kind>) -> Plan {
    compile(&spec(t), &common::shapes_of(kinds), &prepare(BLOCK), None)
        .expect("compiles")
        .0
}

fn sums(plan: &Plan) -> usize {
    plan.ops()
        .iter()
        .filter(|o| matches!(o, Op::Sum { .. }))
        .count()
}

/// Render `blocks` blocks of `BLOCK` frames of `t` on `workers`
/// participants, asserting each block is the reference interpreter's, and
/// return the executor's output, channel by channel.
fn render(
    t: &Topology,
    kinds: &BTreeMap<NodeKey, Kind>,
    workers: usize,
    blocks: usize,
) -> Vec<Vec<f32>> {
    let mut pair = Pair::with_workers(BLOCK, workers);
    pair.switch(&spec(t), kinds);
    let mut got: Vec<Vec<f32>> = vec![Vec::new(); pair.outputs];
    let mut frame = 0;
    for _ in 0..blocks {
        let input = input_signal(frame, BLOCK);
        let (a, b) = pair.block(BLOCK, &input);
        assert_eq!(bits(&a), bits(&b), "executor and reference diverge");
        for (g, a) in got.iter_mut().zip(a) {
            g.extend(a);
        }
        frame += BLOCK as u64;
    }
    got
}

/// Serial and parallel (four participants) renders, which must be bit for bit
/// the same (and each the reference's, `render`).
fn render_both(t: &Topology, kinds: &BTreeMap<NodeKey, Kind>, blocks: usize) -> Vec<Vec<f32>> {
    let serial = render(t, kinds, 1, blocks);
    let parallel = render(t, kinds, 4, blocks);
    assert_eq!(
        bits(&serial),
        bits(&parallel),
        "parallel differs from serial"
    );
    serial
}

/// `((+0.0 + a_0) + a_1) + ..`, one rounding per add.
fn fold(values: &[f32]) -> f32 {
    values.iter().fold(0.0f32, |acc, &v| acc + v)
}

/// `n` constants keyed `1..=n` summed at output channel 0 (`at_port: false`),
/// or at the one port of a pass-through node keyed `n + 1`.
fn fan_in(values: &[f32], at_port: bool) -> (Topology, BTreeMap<NodeKey, Kind>) {
    let mut kinds: Vec<Kind> = values.iter().map(|&v| dc(v)).collect();
    if at_port {
        kinds.push(Kind::Thru { width: 1 });
    }
    let (mut t, map) = nodes(&kinds);
    let n = values.len() as u64;
    if at_port {
        for k in 1..=n {
            t.connect(at(n + 1, 0), direct(k));
        }
        t.set_outputs(vec![out(n + 1)]);
    } else {
        t.outputs = vec![(1..=n).map(out).collect()];
    }
    (t, map)
}

/// A sum is the left fold over the sources in key order: three known
/// constants, at a port and at an output channel, serial and parallel.
///
/// The values are chosen so the order and the grouping show: `2^24 + 1`
/// rounds back to `2^24`, so `((0 + 2^24) + 1) + 1` is `2^24`, the reverse
/// `((0 + 1) + 1) + 2^24` is `2^24 + 2`, and the pairwise `2^24 + (1 + 1)`
/// is `2^24 + 2` as well.
///
/// Mutations (each run, each failed this test, then restored): sum the
/// addends in reverse in `sum_op`; sum pairwise (`a_0 + (a_1 + a_2)`); in
/// `compile`, drop the `sources.sort()` of the input loop and list the sources
/// reversed (the order test below holds this too).
#[test]
fn three_sources_are_the_left_fold() {
    let values = [16_777_216.0f32, 1.0, 1.0];
    let expected = fold(&values);
    assert_eq!(expected, 16_777_216.0, "the figure shows the order");
    assert_ne!(fold(&[1.0, 1.0, 16_777_216.0]), expected);
    for at_port in [false, true] {
        let (t, kinds) = fan_in(&values, at_port);
        let plan = plan_of(&t, &kinds);
        assert_eq!(sums(&plan), 1);
        for channel in render_both(&t, &kinds, 2) {
            assert!(
                channel.iter().all(|&x| x.to_bits() == expected.to_bits()),
                "at_port {at_port}: {:?}",
                &channel[..4]
            );
        }
    }
}

/// A fan-in past `MAX_PORTS` is a chain of ops and one flat left fold: the
/// bits do not depend on where the compiler cut. 100 sources, and the sizes at
/// each cut (a chain holds 64, then 63 more per op), each equal to the fold
/// written out here over all of them.
///
/// Mutations (each run, each failed): fold the chain's run of the first op
/// and the rest separately and add the two (`(a_0 + .. + a_63) + (a_64 + ..)`);
/// make a chain's later ops start from `+0.0` instead of the last result
/// (drops the earlier addends); cut the runs at `MAX_PORTS` rather than
/// `MAX_PORTS - 1` (the chain's shape test below).
#[test]
fn a_wide_fan_in_is_one_flat_fold() {
    for n in [2usize, 63, 64, 65, 100, 127, 128, 129] {
        // Values of many magnitudes, so no association gives the same bits.
        let values: Vec<f32> = (0..n)
            .map(|k| ((k * 37 % 11) as f32 + 1.0) * 0.1 * 10f32.powi((k % 5) as i32))
            .collect();
        let expected = fold(&values);
        if n == 100 {
            // The test can see a tree: summing in halves differs.
            let halves = fold(&values[..n / 2]) + fold(&values[n / 2..]);
            assert_ne!(halves.to_bits(), expected.to_bits(), "n = {n}");
        }
        for at_port in [false, true] {
            let (t, kinds) = fan_in(&values, at_port);
            let ops = sums(&plan_of(&t, &kinds));
            let chain = if n <= 64 {
                1
            } else {
                1 + (n - 64).div_ceil(63)
            };
            assert_eq!(ops, chain, "n = {n}: the chain's length");
            for channel in render_both(&t, &kinds, 1) {
                assert!(
                    channel.iter().all(|&x| x.to_bits() == expected.to_bits()),
                    "n = {n}, at_port {at_port}"
                );
            }
        }
    }
}

/// The accumulator starts at `+0.0`: two `-0.0` sources sum to `+0.0`, and so
/// does one live `-0.0` beside a silent source; a lone source is an alias and
/// keeps its `-0.0`. In place (the first addend read nowhere else) and not
/// (the first addend also read by another node).
///
/// Mutations (each run, each failed): start the fold from the first addend
/// (`*o = a` instead of `0.0 + a` in the fresh form) -> the not-in-place cases
/// keep a `-0.0`; delete the `*o += 0.0` of the in-place form -> the in-place
/// cases keep one; skip the silent addend by value rather than by flag is not
/// a mutation (it is the same bits).
#[test]
fn the_sum_starts_at_positive_zero() {
    let neg = -0.0f32;
    // Two `-0.0` sources at output 0.
    let (t, kinds) = fan_in(&[neg, neg], false);
    for ch in render_both(&t, &kinds, 1) {
        assert!(ch.iter().all(|x| x.to_bits() == 0), "-0.0 + -0.0 is +0.0");
    }
    // A `-0.0` beside a silent `+0.0`: one live addend, `+0.0 + a`.
    let (t, kinds) = fan_in(&[neg, 0.0], false);
    for ch in render_both(&t, &kinds, 1) {
        assert!(ch.iter().all(|x| x.to_bits() == 0), "one live addend");
    }
    // A lone source is the alias, `-0.0` and all.
    let (t, kinds) = fan_in(&[neg], false);
    assert_eq!(sums(&plan_of(&t, &kinds)), 0);
    for ch in render_both(&t, &kinds, 1) {
        assert!(ch.iter().all(|x| x.to_bits() == neg.to_bits()), "an alias");
    }
    // The first addend also read elsewhere (node 3 reads node 1, output 1
    // reads node 3): no in-place form.
    let (mut t, mut kinds) = fan_in(&[neg, neg], false);
    t.nodes.insert(key(3), spec_of(&Kind::Thru { width: 1 }));
    kinds.insert(key(3), Kind::Thru { width: 1 });
    t.connect(at(3, 0), direct(1));
    t.outputs.push(vec![out(3)]);
    let plan = plan_of(&t, &kinds);
    assert_eq!(sums(&plan), 1);
    for (c, ch) in render_both(&t, &kinds, 1).into_iter().enumerate() {
        let want = if c == 0 { 0 } else { neg.to_bits() };
        assert!(ch.iter().all(|x| x.to_bits() == want), "channel {c}");
    }
}

/// The in-place form is taken when the first addend is read nowhere else and
/// refused when it fans out to another reader that nothing orders before the
/// sum; the bits are the same either way (`a_wide_fan_in...` and the render
/// comparisons above hold the fold in both).
///
/// Mutations (each run, each failed): offer the alias without the colouring's
/// legality check (always `dst = srcs[0]`'s slot) -> the fanned-out first
/// addend is overwritten and the verifier or the render fails; never offer
/// it (delete the `Pre::Sum` arm of `in_place`) -> the first assert fails.
#[test]
fn the_first_addend_is_aliased_unless_it_fans_out() {
    let sum_slots = |plan: &Plan| {
        let Some(Op::Sum { srcs, dst }) = plan
            .ops()
            .iter()
            .copied()
            .find(|o| matches!(o, Op::Sum { .. }))
        else {
            panic!("a sum op");
        };
        (
            plan.audio_list()[srcs.start as usize..(srcs.start + srcs.len) as usize].to_vec(),
            dst,
        )
    };
    let (t, kinds) = fan_in(&[0.25, 0.5, 0.125], false);
    let (srcs, dst) = sum_slots(&plan_of(&t, &kinds));
    assert_eq!(dst, srcs[0], "the first addend is read only by the sum");
    assert!(!srcs[1..].contains(&dst));

    let (mut t, mut kinds) = fan_in(&[0.25, 0.5, 0.125], false);
    t.nodes.insert(key(4), spec_of(&Kind::Thru { width: 1 }));
    kinds.insert(key(4), Kind::Thru { width: 1 });
    t.connect(at(4, 0), direct(1));
    t.outputs.push(vec![out(4)]);
    let (srcs, dst) = sum_slots(&plan_of(&t, &kinds));
    assert_ne!(dst, srcs[0], "the first addend fans out elsewhere");
    assert!(!srcs.contains(&dst));
    // Both outputs are right: the sum and the untouched source.
    let got = render_both(&t, &kinds, 1);
    let want = fold(&[0.25, 0.5, 0.125]);
    assert!(got[0].iter().all(|&x| x == want));
    assert!(
        got[1].iter().all(|&x| x == 0.25),
        "source 1 was not overwritten"
    );
}

/// The order is a property of the wiring, not of how the spec lists it. The
/// sources `1.0` (key 1), `1e8` (key 2) and `-1e8` (key 3) sum to `0.0` in key
/// order (`1 + 1e8` is `1e8`) but to `1.0` in the order `2, 3, 1`.
///
/// Mutation (run, failed): delete `sources.sort()` in the input loop of
/// `compile` (and, for the output half, the `sources.sort()` of the output
/// loop) -> the listed order is folded and the figure is `1.0`.
#[test]
fn the_order_is_the_wiring_not_the_listing() {
    let values = [1.0f32, 1.0e8, -1.0e8];
    assert_eq!(fold(&values), 0.0);
    assert_eq!(fold(&[values[1], values[2], values[0]]), 1.0);
    for at_port in [false, true] {
        let (mut t, kinds) = fan_in(&values, at_port);
        // Raw writes, listed 2, 3, 1.
        if at_port {
            t.edges
                .insert(at(4, 0), vec![direct(2), direct(3), direct(1)]);
        } else {
            t.outputs = vec![vec![out(2), out(3), out(1)]];
        }
        for ch in render_both(&t, &kinds, 1) {
            assert!(ch.iter().all(|&x| x == 0.0), "at_port {at_port}");
        }
    }
}

/// A feedback addend sorts after every direct one; a direct and a feedback
/// addend sum together. `1.0` (key 1) and `1.0` (key 3) direct, `2^24`
/// (key 2) fed back by a block: the first block has the feedback slot silent
/// (skipped) and sums `1 + 1 = 2`; every later one sums
/// `((0 + 1) + 1) + 2^24 = 2^24 + 2`, where a feedback addend listed first
/// gives `((0 + 2^24) + 1) + 1 = 2^24`.
///
/// Mutation (run, failed): sort `Edge::Feedback` before `Direct` (a stable
/// `sort_by_key` putting feedback first) -> the later blocks read `2^24`.
#[test]
fn a_feedback_addend_sums_last() {
    let (mut t, kinds) = nodes(&[dc(1.0), dc(16_777_216.0), dc(1.0), Kind::Thru { width: 1 }]);
    t.edges.insert(
        at(4, 0),
        vec![
            Edge::Feedback(FeedbackFrom::new(
                OutPort {
                    node: key(2),
                    port: 0,
                },
                Samples(BLOCK),
            )),
            direct(3),
            direct(1),
        ],
    );
    t.set_outputs(vec![out(4)]);
    let got = render_both(&t, &kinds, 3);
    assert!(
        got[0][..BLOCK].iter().all(|&x| x == 2.0),
        "the first block: the feedback is silent and skipped"
    );
    assert!(
        got[0][BLOCK..].iter().all(|&x| x == 16_777_218.0),
        "later blocks: the feedback addend is last"
    );
}

/// A global source takes part like any other: `Node` sources sort before
/// `Global` ones, so `(0 + 0.25) + x`. At an output channel and at a port.
///
/// Mutation (run, failed): sum the global input first in `compile` (put the
/// `Global` sources ahead) -> the order differs and a sample's bits differ
/// (the figure `0.25 + x` is exact for the test's inputs, so the mutation that
/// shows is dropping the global source: skip the `Source::Global` arm).
#[test]
fn a_global_source_is_an_addend() {
    let mut t = Topology {
        inputs: ChannelLayout::MONO,
        ..Topology::default()
    };
    let kinds: Vec<Kind> = vec![dc(0.25), Kind::Thru { width: 1 }];
    let (nt, map) = nodes(&kinds);
    t.nodes = nt.nodes;
    t.outputs = vec![vec![Source::Global(0), out(1)]];
    t.connect(at(2, 0), Edge::Direct(Source::Global(0)));
    t.connect(at(2, 0), direct(1));
    t.outputs.push(vec![out(2)]);
    let got = render_both(&t, &map, 3);
    let input: Vec<f32> = (0..3)
        .flat_map(|b| input_signal(b * BLOCK as u64, BLOCK))
        .collect();
    for (i, &x) in input.iter().enumerate() {
        let want = (0.0f32 + 0.25) + x;
        assert_eq!(got[0][i].to_bits(), want.to_bits(), "output, frame {i}");
        assert_eq!(got[1][i].to_bits(), want.to_bits(), "port, frame {i}");
    }
}

/// Two paths of different latency meet at one sum: an `L`-frame lag and a
/// zero-latency source, each delayed to the arrival on its own before the
/// sum, so the lagged signal and the other arrive together. Both start at
/// frame 0 as `0.5`: the lag path is `0` for `L` frames and `0.5` after, the
/// other path, delayed by `L`, is the same, so the sum is `0` then `1.0`. At a
/// port (whose sink's arrival is `L`) and at the output channel.
///
/// Mutation (run, failed): skip the PDC delay of the shorter path
/// (`align_audio` returns `ARef::Val(v)` for a source whose `by` is nonzero
/// and whose key is the second) -> the sum reads `0.5` for the first `L`
/// frames.
#[test]
fn each_source_is_aligned_before_the_sum() {
    const L: usize = 50;
    for at_port in [false, true] {
        // 1: Const 0.5 (to the lag), 2: Lag(L), 3: Const 0.5 (direct), 4: sink.
        let (mut t, mut kinds) = nodes(&[
            dc(0.5),
            Kind::Lag { latency: L },
            dc(0.5),
            Kind::Thru { width: 1 },
        ]);
        t.connect(at(2, 0), direct(1));
        if at_port {
            t.connect(at(4, 0), direct(2));
            t.connect(at(4, 0), direct(3));
            t.set_outputs(vec![out(4)]);
        } else {
            t.outputs = vec![vec![out(2), out(3)]];
            t.nodes.remove(&key(4));
            kinds.remove(&key(4));
        }
        let plan = plan_of(&t, &kinds);
        assert_eq!(plan.total_latency().samples(), Samples(L));
        let got = render_both(&t, &kinds, 2);
        for (i, &x) in got[0].iter().enumerate() {
            let want = if i < L { 0.0 } else { 1.0 };
            assert_eq!(x, want, "at_port {at_port}, frame {i}");
        }
    }
}

/// An edge added or removed later realigns without a click in the other path:
/// the source that stays keeps its PDC ring (it is keyed by the sink port and
/// the source), the new one gets its own, and the removed one's goes. The
/// executor, over the commits, is the reference's, which keys its lines alike.
///
/// Mutation (run, failed): rebuild the rings on every commit (in
/// `Executor::apply`, take no old ring) -> the staying source's ring restarts
/// empty and the render diverges from the reference.
#[test]
fn adding_and_removing_a_source_keeps_the_others_rings() {
    // 1: a counter-like source (an emitter's count steps), 2: Lag(40) of it,
    // 3: a constant, 4: the sink.
    let (mut t, kinds) = nodes(&[
        Kind::Emitter {
            period: 7,
            phase: 2,
        },
        Kind::Lag { latency: 40 },
        dc(0.375),
        Kind::Thru { width: 1 },
    ]);
    t.connect(at(2, 0), direct(1));
    t.connect(at(4, 0), direct(1));
    t.connect(at(4, 0), direct(2));
    t.set_outputs(vec![out(4)]);
    let with_third = {
        let mut t = t.clone();
        t.connect(at(4, 0), direct(3));
        t
    };
    let one_source = {
        let mut t = t.clone();
        t.disconnect(at(4, 0), direct(2));
        t
    };
    assert_eq!(sums(&plan_of(&one_source, &kinds)), 0, "one source aliases");
    assert_eq!(sums(&plan_of(&t, &kinds)), 1);
    assert_eq!(sums(&plan_of(&with_third, &kinds)), 1);

    for workers in [1usize, 4] {
        let mut pair = Pair::with_workers(BLOCK, workers);
        let mut frame = 0;
        let sequence = [&t, &with_third, &t, &one_source, &t];
        for (round, topology) in sequence.into_iter().enumerate() {
            pair.switch(&spec(topology), &kinds);
            for _ in 0..4 {
                let input = input_signal(frame, BLOCK);
                let (a, b) = pair.block(BLOCK, &input);
                assert_eq!(bits(&a), bits(&b), "round {round}, frame {frame}");
                frame += BLOCK as u64;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Silence and constant flags.
// ---------------------------------------------------------------------------

/// Silent until switched on, then `1.0`; says which, so the executor flags it.
struct Switch(Arc<AtomicBool>);

impl Node for Switch {
    fn shape(&self) -> Shape {
        Shape::source_discrete(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if self.0.load(Ordering::Relaxed) {
            io.output(0).fill(1.0);
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {}
}

/// Counts its calls, and the calls whose input arrived flagged silent and
/// flagged constant; passes the input through.
struct Probe {
    calls: Arc<AtomicU64>,
    constant: Arc<AtomicU64>,
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::through_discrete(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if io.constant().get(0) {
            self.constant.fetch_add(1, Ordering::Relaxed);
        }
        if io.silent().get(0) {
            return Status::Silent;
        }
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Flags {
    ed: Editor,
    exec: Executor,
    switches: Vec<Arc<AtomicBool>>,
    calls: Arc<AtomicU64>,
}

/// `n` switches (keys `1..=n`) summed into a probe (key `n + 1`), whose output
/// is the graph's.
fn flags(n: u64) -> Flags {
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    let switches: Vec<_> = (0..n).map(|_| Arc::new(AtomicBool::new(false))).collect();
    for (i, s) in switches.iter().enumerate() {
        ed.insert(
            key(i as u64 + 1),
            "switch",
            Unforkable(Switch(Arc::clone(s))),
        );
    }
    let calls = Arc::new(AtomicU64::new(0));
    let constant = Arc::new(AtomicU64::new(0));
    ed.insert(
        key(n + 1),
        "probe",
        Unforkable(Probe {
            calls: Arc::clone(&calls),
            constant: Arc::clone(&constant),
        }),
    );
    for k in 1..=n {
        ed.spec_mut().topology.connect(at(n + 1, 0), direct(k));
    }
    ed.spec_mut().topology.set_outputs(vec![out(n + 1)]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    Flags {
        ed,
        exec,
        switches,
        calls,
    }
}

fn block(exec: &mut Executor) -> Vec<f32> {
    let mut out = vec![0.0f32; BLOCK];
    exec.process(BLOCK, &Transport::default(), &[], &mut [out.as_mut_slice()]);
    out
}

/// Silent addends make a silent sum: the probe (no tail) is called once and
/// then parked, its output is `+0.0`; the first live source after the silence
/// is heard on the very next block; and one live source among 70 (the 64-bit
/// input mask is the node's, never the sum's) is neither parked nor lost.
///
/// Mutations (each run, each failed): flag the sum `SILENT` unconditionally ->
/// the live-after-silence case is parked and not heard; never flag it silent
/// (`flag(false, constant)` for the all-silent case too) -> the probe is
/// called every block.
#[test]
fn silent_addends_make_a_silent_sum_and_a_live_one_is_heard() {
    for n in [3u64, 70] {
        let mut f = flags(n);
        for _ in 0..5 {
            assert!(block(&mut f.exec).iter().all(|&x| x.to_bits() == 0));
        }
        assert_eq!(
            f.calls.load(Ordering::Relaxed),
            1,
            "n = {n}: all-silent sources park the sink after one call"
        );
        // Source 66 (key 67 when there are 70; the middle one of three).
        let live = if n == 70 { 66 } else { 1 };
        f.switches[live].store(true, Ordering::Relaxed);
        let heard = block(&mut f.exec);
        assert!(heard.iter().all(|&x| x == 1.0), "n = {n}: heard at once");
        assert_eq!(f.calls.load(Ordering::Relaxed), 2);
        for _ in 0..3 {
            assert!(block(&mut f.exec).iter().all(|&x| x == 1.0));
        }
        assert_eq!(f.calls.load(Ordering::Relaxed), 5, "n = {n}: never parked");
        let _ = &f.ed;
    }
}

/// A sum is constant when every live addend is, and not when one is not;
/// silent addends do not count against it.
///
/// Mutation (run, failed): flag the output `CONSTANT` unconditionally -> the
/// non-constant case reports a constant input.
#[test]
fn a_sum_is_constant_when_its_live_addends_are() {
    let constant_calls = |kinds: &[Kind]| {
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        for (i, k) in kinds.iter().enumerate() {
            ed.insert(key(i as u64 + 1), "n", Unforkable(TestNode::new(k.clone())));
        }
        let n = kinds.len() as u64;
        let calls = Arc::new(AtomicU64::new(0));
        let constant = Arc::new(AtomicU64::new(0));
        ed.insert(
            key(n + 1),
            "probe",
            Unforkable(Probe {
                calls: Arc::clone(&calls),
                constant: Arc::clone(&constant),
            }),
        );
        for k in 1..=n {
            ed.spec_mut().topology.connect(at(n + 1, 0), direct(k));
        }
        ed.spec_mut().topology.set_outputs(vec![out(n + 1)]);
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        for _ in 0..4 {
            block(&mut exec);
        }
        (
            calls.load(Ordering::Relaxed),
            constant.load(Ordering::Relaxed),
        )
    };
    // Two constants and a silent one: constant on every block.
    assert_eq!(constant_calls(&[dc(0.5), dc(0.25), dc(0.0)]), (4, 4));
    // An emitter's output is not constant.
    let (calls, constant) = constant_calls(&[
        dc(0.5),
        Kind::Emitter {
            period: 3,
            phase: 0,
        },
    ]);
    assert_eq!((calls, constant), (4, 0));
}
