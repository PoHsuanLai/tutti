//! Wiring bus to bus (doc 013, "Buses and coercion between declared layouts",
//! B2): a bus edge is a fold between two declared topologies, lowered to
//! weighted lanes, and nothing in the compiler or the executors knows it.
//!
//! From the spec: each figure is analytic (the fold's own table, written out
//! here), never another render. A render is compared with another only as the
//! doc prescribes: the executor against the reference interpreter, and the
//! parallel executor against the serial one (`Pair`). Mutations are recorded
//! on each test (each run, each failed, then restored).

mod common;

use std::collections::BTreeMap;

use common::{Kind, Pair, TestNode, bits, input_signal, prepare, shapes_of};
use tutti_graph::Node;
use tutti_graph::{
    Bus, BusError, BusRef, BusRole, BusSide, BusSource, Buses, CommitError, CompileError, DelayKey,
    Edge, EdgeGain, EdgeId, Editor, GraphInvalid, GraphSpec, InPort, Lowering, NodeKey, NodeSpec,
    Op, OutPort, Plan, Sink, Source, Transport, Unforkable, ValidGraph, compile,
};
use tutti_types::{
    Amplitude, BusTopology, ChannelLayout, ChannelTopology, FoldError, M3DB, Samples, Tail,
};

const BLOCK: usize = 64;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::try_from(n).expect("a test width is small")
}

fn at(node: u64, port: u16) -> InPort {
    InPort {
        node: key(node),
        port,
    }
}

fn out_port(node: u64, port: u16) -> Source {
    Source::Node(OutPort {
        node: key(node),
        port,
    })
}

fn lane(sink: u64, port: u16, from: Source) -> EdgeId {
    EdgeId {
        sink: Sink::Port(at(sink, port)),
        from: Edge::Direct(from),
    }
}

fn main_bus(topology: BusTopology) -> Bus {
    Bus {
        topology,
        role: BusRole::Main,
    }
}

/// The SMPTE order of `n` channels (1, 2 or 6 here), as a main bus.
fn smpte(n: usize) -> Bus {
    let topology = ChannelTopology::smpte(ch(n)).expect("an SMPTE width");
    main_bus(BusTopology::try_from(&topology).expect("fits a bus"))
}

fn discrete(n: usize) -> Bus {
    main_bus(BusTopology::discrete(ch(n)).expect("fits a bus"))
}

fn buses(list: &[Bus]) -> Buses {
    Buses::new(list).expect("few enough buses")
}

fn bused(inner: Kind, ins: &[Bus], outs: &[Bus]) -> Kind {
    Kind::Bused {
        inner: Box::new(inner),
        buses_in: Box::new(buses(ins)),
        buses_out: Box::new(buses(outs)),
    }
}

/// A source of `n` channels: global input `c` scaled by one, on `n` ports,
/// declaring `out` as its output bus.
fn source(n: usize, out: Bus) -> Kind {
    bused(
        Kind::Gain {
            gain: 1.0,
            width: n,
        },
        &[discrete(n)],
        &[out],
    )
}

/// A pass-through of `n` channels declaring `input` as its input bus.
fn sink(n: usize, input: Bus) -> Kind {
    bused(Kind::Thru { width: n }, &[input], &[discrete(n)])
}

fn spec_of(kind: &Kind) -> NodeSpec {
    let s = TestNode::new(kind.clone()).shape();
    NodeSpec::new("test", s.audio_in, s.audio_out)
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

fn bus_ref(node: u64, bus: u8) -> BusRef {
    BusRef {
        node: key(node),
        bus,
    }
}

fn from(node: u64, bus: u8) -> BusSource {
    BusSource::Node(bus_ref(node, bus))
}

/// A graph value and the kinds that make its nodes, built by hand.
#[derive(Clone)]
struct Rig {
    spec: GraphSpec,
    kinds: BTreeMap<NodeKey, Kind>,
}

impl Rig {
    fn new(inputs: usize) -> Self {
        let mut spec = GraphSpec::default();
        spec.topology.inputs = ChannelLayout::from_count(inputs as u16);
        Self {
            spec,
            kinds: BTreeMap::new(),
        }
    }

    fn add(&mut self, node: u64, kind: Kind) {
        self.spec.topology.nodes.insert(key(node), spec_of(&kind));
        self.kinds.insert(key(node), kind);
    }

    /// Global input channel `c` into port `c` of `node`, for every port.
    fn feed_globals(&mut self, node: u64, width: usize) {
        for c in 0..width as u16 {
            self.spec
                .topology
                .connect(at(node, c), Edge::Direct(Source::Global(c)));
        }
    }

    /// Every output channel of `node`, in order, is a graph output.
    fn output_all(&mut self, node: u64, width: usize) {
        let sources: Vec<Source> = (0..width as u16).map(|c| out_port(node, c)).collect();
        self.spec.topology.set_outputs(sources);
    }

    fn lowering(&self) -> Result<Lowering, BusError> {
        self.spec.lowered(&shapes_of(&self.kinds))
    }

    fn valid(&self) -> ValidGraph {
        self.lowering()
            .expect("lowers")
            .spec
            .validate()
            .expect("valid")
    }

    fn plan(&self) -> Plan {
        compile(
            &self.valid(),
            &shapes_of(&self.kinds),
            &prepare(BLOCK),
            None,
        )
        .expect("compiles")
        .0
    }

    /// `blocks` blocks on `workers` participants, each asserted to be the
    /// reference interpreter's.
    fn render(&self, workers: usize, blocks: usize) -> Vec<Vec<f32>> {
        let mut pair = Pair::with_workers(BLOCK, workers);
        pair.switch(&self.valid(), &self.kinds);
        let mut got: Vec<Vec<f32>> = vec![Vec::new(); pair.outputs];
        for b in 0..blocks {
            let input = input_signal((b * BLOCK) as u64, BLOCK);
            let (a, r) = pair.block(BLOCK, &input);
            assert_eq!(bits(&a), bits(&r), "executor and reference diverge");
            for (g, a) in got.iter_mut().zip(a) {
                g.extend(a);
            }
        }
        got
    }

    /// Serial and parallel renders, which must be bit for bit the same.
    fn render_both(&self, blocks: usize) -> Vec<Vec<f32>> {
        let serial = self.render(1, blocks);
        let parallel = self.render(4, blocks);
        assert_eq!(
            bits(&serial),
            bits(&parallel),
            "parallel differs from serial"
        );
        serial
    }
}

/// Global input channel `c` over `blocks` blocks: the signal scaled by `2^-c`
/// (the harness's rule), exact.
fn global(c: usize, blocks: usize) -> Vec<f32> {
    (0..blocks)
        .flat_map(|b| input_signal((b * BLOCK) as u64, BLOCK))
        .map(|x| x * 0.5f32.powi(c as i32))
        .collect()
}

/// `src` channels of SMPTE global input through a source node (key 1) into a
/// pass-through (key 2) of `dst` SMPTE channels by one bus edge; every channel
/// of the pass-through is an output.
fn fold_rig(src: usize, dst: usize) -> Rig {
    let mut rig = Rig::new(src);
    rig.add(1, source(src, smpte(src)));
    rig.feed_globals(1, src);
    rig.add(2, sink(dst, smpte(dst)));
    assert!(rig.spec.connect_bus(from(1, 0), bus_ref(2, 0)));
    rig.output_all(2, dst);
    rig
}

fn weighted_ops(plan: &Plan) -> usize {
    plan.ops()
        .iter()
        .filter(|o| matches!(o, Op::WeightedSum { .. }))
        .count()
}

/// `((0 + w_0 * a_0) + w_1 * a_1) + ..`: a multiply and an add per lane, two
/// roundings, in source order: how a sink sums its weighted lanes.
fn weighted_fold(terms: &[(f32, &[f32])], i: usize) -> f32 {
    terms.iter().fold(0.0f32, |acc, &(w, a)| acc + w * a[i])
}

// ---------------------------------------------------------------------------
// Analytic folds.
// ---------------------------------------------------------------------------

/// Stereo into mono is `0.5 L + 0.5 R`, exactly: two weighted lanes of `0.5`
/// into the one channel, summed in source order, serial and parallel alike.
///
/// Mutations (run), each failing this test: the lowering giving every lane
/// double its weight (`weight * 2.0`); the lowering dropping the weighted
/// entries (every lane plain, so `L + R`); the lowering taking only the
/// first contribution of each destination channel.
#[test]
fn stereo_into_mono_is_half_left_plus_half_right() {
    let rig = fold_rig(2, 1);
    let lowering = rig.lowering().unwrap();
    // The lanes are the spec's figures: one per source channel, 0.5 each.
    assert_eq!(
        lowering.lanes.values().map(|w| w.get()).collect::<Vec<_>>(),
        [0.5, 0.5]
    );
    let blocks = 6;
    let got = rig.render_both(blocks);
    let (l, r) = (global(0, blocks), global(1, blocks));
    let expected: Vec<u32> = (0..l.len())
        .map(|i| weighted_fold(&[(0.5, &l), (0.5, &r)], i).to_bits())
        .collect();
    assert_eq!(bits(&got), [expected]);
    // The signal is not silent, so the test is not vacuous.
    assert!(got[0].iter().any(|&x| x != 0.0));
}

/// 5.1 into stereo is the fold's ITU table: each side takes its front, the
/// centre at -3 dB and its surround at -3 dB, and the LFE is dropped. Each
/// destination channel is the sum of its lanes in source-port order.
///
/// Mutations (run), each failing this test: the lowering swapping each
/// contribution's source channel for its pair (`k ^ 1`); the lowering using the
/// destination channel as the source channel (`k = c`); the lane's sink port
/// always the first channel of the bus.
#[test]
fn five_one_into_stereo_follows_the_itu_rows() {
    let rig = fold_rig(6, 2);
    // The fold's rows (`Fold::contributions`): left is FL, C at -3 dB and Ls
    // at -3 dB; right is FR, C and Rs. The LFE (channel 3) feeds neither.
    let lowering = rig.lowering().unwrap();
    let weights: Vec<(EdgeId, f32)> = lowering
        .lanes
        .iter()
        .map(|(&id, w)| (id, w.get()))
        .collect();
    let expect = [
        (lane(2, 0, out_port(1, 0)), 1.0),
        (lane(2, 0, out_port(1, 2)), M3DB),
        (lane(2, 0, out_port(1, 4)), M3DB),
        (lane(2, 1, out_port(1, 1)), 1.0),
        (lane(2, 1, out_port(1, 2)), M3DB),
        (lane(2, 1, out_port(1, 5)), M3DB),
    ];
    assert_eq!(weights, expect);

    let blocks = 6;
    let got = rig.render_both(blocks);
    let ch6: Vec<Vec<f32>> = (0..6).map(|c| global(c, blocks)).collect();
    let side = |front: usize, surround: usize| -> Vec<u32> {
        (0..ch6[0].len())
            .map(|i| {
                // The unity lane is a plain addend, the others weighted.
                weighted_fold(
                    &[(1.0, &ch6[front]), (M3DB, &ch6[2]), (M3DB, &ch6[surround])],
                    i,
                )
                .to_bits()
            })
            .collect()
    };
    assert_eq!(bits(&got), [side(0, 4), side(1, 5)]);
}

/// Mono into stereo is a copy of the one channel to both sides at unity: two
/// plain edges, no weighted lane anywhere in the plan.
///
/// Mutations (run), each failing this test: the lowering weighting the copy
/// by `0.5`; the lowering writing only the first destination channel.
#[test]
fn mono_into_stereo_copies_to_both_sides() {
    let rig = fold_rig(1, 2);
    let lowering = rig.lowering().unwrap();
    assert_eq!(
        lowering.lanes.keys().copied().collect::<Vec<_>>(),
        [lane(2, 0, out_port(1, 0)), lane(2, 1, out_port(1, 0)),]
    );
    assert!(lowering.lanes.values().all(|w| w.get() == 1.0));
    assert!(
        lowering.spec.gains.is_empty(),
        "unity lanes are plain edges"
    );
    assert_eq!(weighted_ops(&rig.plan()), 0);

    let blocks = 4;
    let got = rig.render_both(blocks);
    let x: Vec<u32> = global(0, blocks).iter().map(|v| v.to_bits()).collect();
    assert_eq!(bits(&got), [x.clone(), x]);
}

/// 7.1 into stereo, read out as the matrix: the lowering's lane weight from
/// source channel `k` into destination channel `o` is the fold's entry, with
/// a missing lane a zero. `Lo` takes FL and, at -3 dB, C, Ls and Lb; `Ro` the
/// mirror; the LFE neither. (This is what `DownmixNode`'s unit-pulse test
/// pinned, now through the bus edge it was deleted for.)
///
/// Mutations (run), each failing this test: the lowering reading destination
/// channel 0 for every sink port; the lowering swapping each source for its
/// pair (`k ^ 1`).
#[test]
fn seven_one_into_stereo_reads_out_the_matrix() {
    let m = M3DB;
    let want = [
        [1.0, 0.0, m, 0.0, m, 0.0, m, 0.0],
        [0.0, 1.0, m, 0.0, 0.0, m, 0.0, m],
    ];
    let lowering = fold_rig(8, 2).lowering().unwrap();
    for (o, row) in want.iter().enumerate() {
        for (k, &entry) in row.iter().enumerate() {
            let got = lowering
                .lanes
                .get(&lane(2, o as u16, out_port(1, k as u16)))
                .map_or(0.0, |w| w.get());
            assert_eq!(got, entry, "destination {o}, source {k}");
        }
    }
}

/// Widening zero-fills: stereo into 5.1 passes the pair through at unity and
/// leaves the four new channels silent; nothing invents a centre (placement
/// is a panner's, put there on purpose). A silent destination channel has no
/// lane at all.
///
/// Mutations (run), each failing this test: the lowering feeding the centre
/// from channel 0; the lowering dropping the pair's second channel.
#[test]
fn widening_zero_fills_the_new_channels() {
    let rig = fold_rig(2, 6);
    let lowering = rig.lowering().unwrap();
    assert_eq!(
        lowering.lanes.keys().copied().collect::<Vec<_>>(),
        [lane(2, 0, out_port(1, 0)), lane(2, 1, out_port(1, 1))]
    );
    let blocks = 4;
    let got = rig.render_both(blocks);
    let (l, r) = (global(0, blocks), global(1, blocks));
    assert_eq!(bits(&got[..2]), [bits_of(&l), bits_of(&r)]);
    assert!(l.iter().any(|&x| x != 0.0), "not a vacuous test");
    for (c, channel) in got.iter().enumerate().skip(2) {
        assert!(channel.iter().all(|&x| x.to_bits() == 0), "channel {c}");
    }
}

fn bits_of(x: &[f32]) -> Vec<u32> {
    x.iter().map(|v| v.to_bits()).collect()
}

// ---------------------------------------------------------------------------
// The identity: plain edges, the hand-wired plan.
// ---------------------------------------------------------------------------

/// Equal topologies lower to plain per-channel edges, and the plan is the
/// hand-wired graph's, op for op (the plans are compared, not renders).
///
/// Mutations (run), each failing this test: dropping the unity shortcut (every
/// lane given an `EdgeGain` entry, so the plan holds weighted sums); a unity
/// lane weighted by `0.5`.
#[test]
fn a_same_topology_bus_edge_is_the_hand_wired_plan() {
    let mut bus = Rig::new(2);
    bus.add(1, source(2, smpte(2)));
    bus.feed_globals(1, 2);
    bus.add(2, sink(2, smpte(2)));
    bus.spec.connect_bus(from(1, 0), bus_ref(2, 0));
    bus.output_all(2, 2);

    let mut wired = bus.clone();
    wired.spec.bus_edges.clear();
    for c in 0..2 {
        wired
            .spec
            .topology
            .connect(at(2, c), Edge::Direct(out_port(1, c)));
    }

    let lowering = bus.lowering().unwrap();
    assert_eq!(lowering.spec.topology, wired.spec.topology);
    assert!(lowering.spec.gains.is_empty());
    assert!(lowering.spec.bus_edges.is_empty());
    assert_eq!(bus.plan(), wired.plan());
    assert_eq!(weighted_ops(&bus.plan()), 0);
}

// ---------------------------------------------------------------------------
// A fold beside an ordinary source.
// ---------------------------------------------------------------------------

/// A bus edge and an ordinary source on the same input channel sum both, in
/// source order (the bus edge's source node sorts first here).
///
/// Mutation (run): the lowering replacing the sink port's sources instead of
/// adding to them (`edges.insert`) -> the plain `0.25` is gone and the figure
/// misses it.
#[test]
fn a_bus_edge_sums_with_a_plain_source_on_the_same_channel() {
    let mut rig = fold_rig(2, 1);
    rig.add(
        3,
        Kind::Const {
            value: 0.25,
            width: 1,
        },
    );
    rig.spec
        .topology
        .connect(at(2, 0), Edge::Direct(out_port(3, 0)));
    let blocks = 6;
    let got = rig.render_both(blocks);
    let (l, r) = (global(0, blocks), global(1, blocks));
    let expected: Vec<u32> = (0..l.len())
        .map(|i| {
            let fold = weighted_fold(&[(0.5, &l), (0.5, &r)], i);
            (fold + 0.25).to_bits()
        })
        .collect();
    assert_eq!(bits(&got), [expected]);
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

fn editor(kinds: &[(u64, Kind)], inputs: usize) -> (Editor, tutti_graph::Executor) {
    let (mut ed, exec) = Editor::new(prepare(BLOCK));
    for (n, kind) in kinds {
        ed.insert(key(*n), "test", Unforkable(TestNode::new(kind.clone())));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(inputs as u16);
    (ed, exec)
}

/// A bus edge whose topologies cannot be folded is refused at the call, with
/// the fold's own reason, and nothing is stored: three discrete channels into
/// two have no place (`Unplaced`), and discrete channels meet a named bus of
/// another width only by a relabelling that is not there (`Discrete`).
///
/// Mutations (run), each failing this test: `connect_bus` storing the edge
/// before the check; `connect_bus` not checking at all.
#[test]
fn an_unfoldable_bus_edge_is_refused_at_the_call_and_stores_nothing() {
    let (mut ed, _exec) = editor(
        &[
            (1, source(3, discrete(3))),
            (2, sink(2, discrete(2))),
            (3, sink(2, smpte(2))),
        ],
        0,
    );
    let refused = ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap_err();
    assert!(
        matches!(
            refused,
            BusError::Fold {
                error: FoldError::Unplaced { .. },
                ..
            }
        ),
        "{refused:?}"
    );
    let refused = ed.connect_bus(from(1, 0), bus_ref(3, 0)).unwrap_err();
    assert_eq!(
        refused,
        BusError::Fold {
            from: from(1, 0),
            to: bus_ref(3, 0),
            error: FoldError::Discrete {
                src: ch(3),
                dst: ch(2)
            }
        }
    );
    assert!(ed.spec().bus_edges.is_empty(), "nothing is stored");
}

/// The global input is a discrete bus: with no global input at all there is
/// nothing to read (`Empty`), and one wider than a bus is refused by name.
///
/// Mutation (run): `BusSource::Global` taking the sink's width instead of the
/// global input's -> the empty input folds.
#[test]
fn the_global_input_is_a_discrete_bus() {
    let (mut ed, _exec) = editor(&[(1, sink(2, discrete(2)))], 0);
    let refused = ed
        .connect_bus(BusSource::Global, bus_ref(1, 0))
        .unwrap_err();
    assert!(
        matches!(
            refused,
            BusError::Fold {
                error: FoldError::Empty(_),
                ..
            }
        ),
        "{refused:?}"
    );
    // Discrete(2) into discrete(2): the same width folds.
    ed.spec_mut().topology.inputs = ch(2);
    ed.connect_bus(BusSource::Global, bus_ref(1, 0)).unwrap();
    // Wider than a bus holds.
    ed.spec_mut().topology.inputs = ch(17);
    ed.spec_mut()
        .disconnect_bus(BusSource::Global, bus_ref(1, 0));
    let refused = ed
        .connect_bus(BusSource::Global, bus_ref(1, 0))
        .unwrap_err();
    assert_eq!(refused, BusError::GlobalTooWide { width: 17 });
}

/// A node or a bus that does not exist, and a bus asked for on the wrong side,
/// are named; a source names an **output** bus and a sink an **input** bus.
///
/// Mutations (run), each failing this test: reading the sink's bus from
/// `buses_out`; reading the source's from `buses_in`.
#[test]
fn a_missing_node_a_missing_bus_and_a_wrong_side_are_named() {
    // Node 1: one input bus, two output buses. Node 2: two input buses, one
    // output.
    let two_out = bused(
        Kind::Gain {
            gain: 1.0,
            width: 4,
        },
        &[discrete(4)],
        &[smpte(2), smpte(2)],
    );
    let two_in = bused(
        Kind::Thru { width: 4 },
        &[smpte(2), smpte(2)],
        &[discrete(4)],
    );
    let (mut ed, _exec) = editor(&[(1, two_out), (2, two_in)], 0);

    assert_eq!(
        ed.connect_bus(from(9, 0), bus_ref(2, 0)),
        Err(BusError::NoSuchNode { node: key(9) })
    );
    assert_eq!(
        ed.connect_bus(from(1, 0), bus_ref(9, 0)),
        Err(BusError::NoSuchNode { node: key(9) })
    );
    // Node 1 has two output buses and one input bus: bus 1 as a *sink* is the
    // wrong side.
    assert_eq!(
        ed.connect_bus(from(2, 0), bus_ref(1, 1)),
        Err(BusError::NoSuchBus {
            bus: bus_ref(1, 1),
            side: BusSide::Input
        })
    );
    // Node 2 has two input buses and one output bus: bus 1 as a *source* is
    // the wrong side.
    assert_eq!(
        ed.connect_bus(from(2, 1), bus_ref(1, 0)),
        Err(BusError::NoSuchBus {
            bus: bus_ref(2, 1),
            side: BusSide::Output
        })
    );
    assert!(ed.spec().bus_edges.is_empty());
    // And the buses that do exist connect: node 1's second output bus into
    // node 2's second input bus.
    ed.connect_bus(from(1, 1), bus_ref(2, 1)).unwrap();
    let lanes = ed.lowered().unwrap().lanes;
    assert_eq!(
        lanes.keys().copied().collect::<Vec<_>>(),
        [lane(2, 2, out_port(1, 2)), lane(2, 3, out_port(1, 3)),],
        "a bus is the run of ports after the earlier buses"
    );
}

/// A lane that lands on an edge the spec already has is refused, whichever
/// came first: at the call when the plain edge is there, at the commit when it
/// came after (the commit says `CompileError::Bus`).
///
/// Mutations (run), each failing this test: dropping the `has_edge` check
/// (`check_lane`, which the call and the lowering share: the duplicate
/// collapses silently at the commit and the call accepts); `connect_bus`
/// skipping the lane check alone (the call accepts).
#[test]
fn a_lane_on_a_plain_edge_is_refused() {
    let kinds = [(1, source(2, smpte(2))), (2, sink(2, smpte(2)))];
    let (mut ed, mut exec) = editor(&kinds, 2);
    ed.spec_mut()
        .topology
        .connect(at(2, 0), Edge::Direct(out_port(1, 0)));
    let id = lane(2, 0, out_port(1, 0));
    assert_eq!(
        ed.connect_bus(from(1, 0), bus_ref(2, 0)),
        Err(BusError::Overlaps { id })
    );
    assert!(ed.spec().bus_edges.is_empty());

    // The other way round: the bus edge first, then the plain edge.
    let (mut ed, _exec2) = editor(&kinds, 2);
    ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap();
    ed.spec_mut()
        .topology
        .connect(at(2, 0), Edge::Direct(out_port(1, 0)));
    assert_eq!(
        ed.commit(),
        Err(CommitError::Compile(CompileError::Bus(
            BusError::Overlaps { id }
        )))
    );
    exec.apply_pending();
}

/// A node replaced by one whose bus no longer folds fails the commit with the
/// bus error, never a silent drop; the plan sent last stays.
///
/// Mutation (run): `lower_and_validate` ignoring a lowering failure (compiling
/// the spec without its bus edges) -> the commit succeeds.
#[test]
fn a_replaced_node_whose_bus_no_longer_folds_fails_the_commit() {
    let (mut ed, mut exec) = editor(&[(1, source(2, smpte(2))), (2, sink(2, smpte(2)))], 2);
    ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap();
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    // A source of three discrete channels: no place in a named stereo bus.
    ed.insert(
        key(1),
        "test",
        Unforkable(TestNode::new(source(3, discrete(3)))),
    );
    let refused = ed.commit().unwrap_err();
    assert!(
        matches!(
            refused,
            CommitError::Compile(CompileError::Bus(BusError::Fold { .. }))
        ),
        "{refused:?}"
    );
}

/// A bus edge's lanes carry the fold's weights and no gain of the author's:
/// `set_edge_gain` refuses a lane by name, a hand-written entry at a lane's
/// edge fails the commit, and the lane's cell is nobody's knob.
///
/// Mutations (run), each failing this test: `check_gain_edge` answering
/// `GainWithoutEdge` for a lane; the lowering overwriting an author's entry
/// at a lane's edge; the bus lane's cell kept among the editor's own, with a
/// knob (`edge_gain_cell` answers).
#[test]
fn a_bus_lane_takes_no_gain_of_the_authors() {
    let (mut ed, mut exec) = editor(&[(1, source(2, smpte(2))), (2, sink(1, smpte(1)))], 2);
    ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap();
    let id = lane(2, 0, out_port(1, 0));
    assert_eq!(
        ed.set_edge_gain(id, Amplitude::new(0.25)),
        Err(GraphInvalid::GainOnBusLane { id })
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(
        ed.set_edge_gain(id, Amplitude::new(0.25)),
        Err(GraphInvalid::GainOnBusLane { id })
    );
    assert!(ed.edge_gain_cell(id).is_none(), "no knob on a bus lane");
    assert!(ed.spec().gains.is_empty(), "the author's spec has no entry");
    // An edge that is neither the spec's nor a lane is still a plain miss.
    let none = lane(2, 0, out_port(7, 0));
    assert_eq!(
        ed.set_edge_gain(none, Amplitude::new(0.25)),
        Err(GraphInvalid::GainWithoutEdge { id: none })
    );
    // A hand-written entry at a lane's edge is the author's weight where the
    // fold's goes.
    ed.spec_mut().set_edge_gain(id, EdgeGain::default());
    assert_eq!(
        ed.commit(),
        Err(CommitError::Compile(CompileError::Bus(
            BusError::GainOnLane { id }
        )))
    );
}

// ---------------------------------------------------------------------------
// The lowering is a pure function.
// ---------------------------------------------------------------------------

/// The same inputs lower to the same edges and weights, however the bus edges
/// were declared: sources listed in either order, and the lowering run twice.
///
/// Mutation (run): `connect_bus` appending instead of inserting in source
/// order (declaration order decides).
#[test]
fn the_lowering_is_a_pure_function_of_the_spec() {
    let build = |order: &[u64]| {
        let mut rig = Rig::new(2);
        rig.add(1, source(2, smpte(2)));
        rig.add(3, source(2, smpte(2)));
        rig.add(2, sink(1, smpte(1)));
        for &n in order {
            rig.spec.connect_bus(from(n, 0), bus_ref(2, 0));
        }
        rig
    };
    let (a, b) = (build(&[1, 3]), build(&[3, 1]));
    assert_eq!(a.spec.bus_edges, b.spec.bus_edges, "a set, in source order");
    let first = a.lowering().unwrap();
    assert_eq!(first, a.lowering().unwrap());
    assert_eq!(first, b.lowering().unwrap());
    // Four lanes: two source channels from each of two nodes.
    assert_eq!(first.lanes.len(), 4);
}

// ---------------------------------------------------------------------------
// The editor's commit, end to end.
// ---------------------------------------------------------------------------

fn render_block(
    exec: &mut tutti_graph::Executor,
    inputs: &[Vec<f32>],
    outs: usize,
) -> Vec<Vec<f32>> {
    let frames = inputs[0].len();
    let ins: Vec<&[f32]> = inputs.iter().map(Vec::as_slice).collect();
    let mut bufs = vec![vec![0.0f32; frames]; outs];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(frames, &Transport::default(), &ins, &mut refs);
    bufs
}

/// The editor commits a bus edge as its lanes, and removing it removes the
/// lanes and their weights, not the author's own edges: the render is the fold
/// while it is connected and the author's lane alone after.
///
/// Mutations (run), each failing this test: the commit sending the lanes'
/// cells at unity (the weights lost: the first figure is `L + R`);
/// `disconnect_bus` clearing the spec's own gains too.
#[test]
fn connecting_and_removing_a_bus_edge_round_trips_through_the_editor() {
    let (mut ed, mut exec) = editor(
        &[
            (1, source(2, smpte(2))),
            (2, sink(1, smpte(1))),
            (
                3,
                Kind::Const {
                    value: 0.5,
                    width: 1,
                },
            ),
        ],
        2,
    );
    for c in 0..2u16 {
        ed.spec_mut()
            .topology
            .connect(at(1, c), Edge::Direct(Source::Global(c)));
    }
    ed.spec_mut().topology.set_outputs(vec![out_port(2, 0)]);
    // The author's own lane: the constant into the sink at a weight of 0.25.
    let own = lane(2, 0, out_port(3, 0));
    ed.spec_mut()
        .topology
        .connect(at(2, 0), Edge::Direct(out_port(3, 0)));
    ed.spec_mut()
        .set_edge_gain(own, EdgeGain::new(Amplitude::new(0.25)));

    ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap();
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();

    let x: Vec<f32> = input_signal(0, BLOCK);
    let inputs = vec![x.clone(), x.iter().map(|v| v * 0.5).collect()];
    let got = render_block(&mut exec, &inputs, 1);
    let expected: Vec<u32> = (0..BLOCK)
        .map(|i| {
            // Source order: the source node 1's lanes, then node 3's.
            let fold = weighted_fold(&[(0.5, &inputs[0]), (0.5, &inputs[1])], i);
            (fold + 0.25 * 0.5).to_bits()
        })
        .collect();
    assert_eq!(bits(&got), [expected]);

    // Removal: the lanes and their weights go, the author's stay.
    assert!(ed.disconnect_bus(from(1, 0), bus_ref(2, 0)));
    assert!(
        !ed.disconnect_bus(from(1, 0), bus_ref(2, 0)),
        "already gone"
    );
    assert!(ed.lowered().unwrap().lanes.is_empty());
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(ed.spec().has_edge(own));
    assert!(ed.spec().gains.contains_key(&own));
    let got = render_block(&mut exec, &inputs, 1);
    // The weighted lane ramps from the weight the plan before gave it only
    // when the lane is kept; a lane the new plan has starts at its value.
    let alone: Vec<u32> = (0..BLOCK).map(|_| (0.25f32 * 0.5).to_bits()).collect();
    assert_eq!(bits(&got), [alone]);
}

/// Removing a node removes the bus edges that touch it, as it removes its
/// edges; a stale bus edge would fail every later commit.
///
/// Mutation (run): `Editor::remove` leaving the removed node among another
/// bus edge's sources -> the stale source is still listed.
#[test]
fn removing_a_node_removes_its_bus_edges() {
    let (mut ed, mut exec) = editor(
        &[
            (1, source(2, smpte(2))),
            (2, sink(1, smpte(1))),
            (3, source(2, smpte(2))),
        ],
        0,
    );
    ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap();
    ed.connect_bus(from(3, 0), bus_ref(2, 0)).unwrap();
    ed.remove(key(1));
    assert_eq!(ed.spec().bus_edges[&bus_ref(2, 0)], [from(3, 0)]);
    ed.remove(key(2));
    assert!(ed.spec().bus_edges.is_empty());
    ed.commit().expect("commits");
    exec.apply_pending();
}

// ---------------------------------------------------------------------------
// PDC.
// ---------------------------------------------------------------------------

fn latent(n: usize, latency: usize, out: Bus) -> Kind {
    bused(
        Kind::Spec {
            behaviour: common::SpecBehaviour::Gain(1.0),
            ins: n,
            outs: n,
            latency,
            tail: Tail::None,
        },
        &[discrete(n)],
        &[out],
    )
}

fn delays(plan: &Plan) -> BTreeMap<DelayKey, Samples> {
    plan.delays().iter().map(|d| (d.key, d.len)).collect()
}

/// A bus edge's sources align like any edges, lane by lane: a stereo source of
/// 4 frames of latency and a second of 9 into one mono bus, and an ordinary
/// source of none, are all brought to 9: each lane of the first by 5 (a ring
/// per `(sink port, source)`), the ordinary one by 9, the latest by none.
///
/// Mutations (run), each failing this test: the lowering connecting only its
/// unity lanes (the weighted ones are out of the compiler's latency fold: no
/// delay on them); the lane's source port taken from the destination channel.
#[test]
fn a_bus_edges_lanes_align_like_any_edges() {
    let mut rig = Rig::new(2);
    rig.add(1, latent(2, 4, smpte(2)));
    rig.feed_globals(1, 2);
    rig.add(4, latent(2, 9, smpte(2)));
    rig.feed_globals(4, 2);
    rig.add(
        3,
        Kind::Const {
            value: 0.25,
            width: 1,
        },
    );
    rig.add(2, sink(1, smpte(1)));
    rig.spec.connect_bus(from(1, 0), bus_ref(2, 0));
    rig.spec.connect_bus(from(4, 0), bus_ref(2, 0));
    rig.spec
        .topology
        .connect(at(2, 0), Edge::Direct(out_port(3, 0)));
    rig.output_all(2, 1);
    let got = delays(&rig.plan());
    let ring = |from: Source| DelayKey::Audio { at: at(2, 0), from };
    let expected = BTreeMap::from([
        (ring(out_port(1, 0)), Samples(5)),
        (ring(out_port(1, 1)), Samples(5)),
        (ring(out_port(3, 0)), Samples(9)),
    ]);
    // Node 1 declares 4 frames and node 4 declares 9: a lane of node 1 is
    // 5 frames early.
    assert_eq!(got, expected);
    // The lowered spec is what the latency fold reads, too.
    let lowered = rig.lowering().unwrap().spec;
    assert_eq!(tutti_graph::latency_plan(&lowered).total(), Samples(9),);
    let unlowered = tutti_graph::latency_plan(&rig.spec);
    assert_ne!(unlowered, tutti_graph::latency_plan(&lowered));
}

// ---------------------------------------------------------------------------
// Forks.
// ---------------------------------------------------------------------------

/// A fork of a graph with a bus edge is the lowered graph: the bus edge's
/// lanes are edges and its weights gains in the fork, and the fork renders the
/// fold (the fork is reachability over the lanes, too: the source is forked).
///
/// Mutations (run), each failing this test: `fork_gather` reading the
/// unlowered spec (`self.spec()`: the stereo source is not upstream of the
/// output, the fork renders silence).
#[test]
fn a_fork_of_a_bus_edge_renders_the_fold() {
    use tutti_graph::{Cx, ForkByClone, ForkMode, ForkTarget, Io, Renderer, Shape, Status};

    #[derive(Clone)]
    struct Stereo;
    impl Node for Stereo {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
                .with_buses_out(buses(&[smpte(2)]))
        }
        fn prepare(&mut self, _: &tutti_graph::Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0)[0] = 0.25;
            io.output(1)[0] = 0.75;
            Status::Constant
        }
        fn reset(&mut self) {}
    }
    #[derive(Clone)]
    struct Mono;
    impl Node for Mono {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_buses_in(buses(&[smpte(1)]))
        }
        fn prepare(&mut self, _: &tutti_graph::Prepare) {}
        fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
            Status::Bypass
        }
        fn reset(&mut self) {}
    }

    let (mut ed, exec) = Editor::new(prepare(BLOCK));
    ed.insert(key(1), "stereo", ForkByClone(Stereo));
    ed.insert(key(2), "mono", ForkByClone(Mono));
    ed.connect_bus(from(1, 0), bus_ref(2, 0)).unwrap();
    ed.spec_mut().topology.set_outputs(vec![out_port(2, 0)]);
    ed.commit().expect("commits");
    let (fork_ed, fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(BLOCK))
        .expect("forkable");
    let got = Renderer::new(fork_ed, fork_exec).render(2 * BLOCK);
    assert_eq!(got.len(), 1);
    let want = (0.0f32 + 0.5 * 0.25) + 0.5 * 0.75;
    assert!(got[0].iter().all(|&x| x == want), "{:?}", &got[0][..4]);
    drop((ed, exec));
}
