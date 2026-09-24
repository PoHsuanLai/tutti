//! The differential suite: the serial plan executor against the reference
//! interpreter, bit for bit, on random graphs (doc 013 §3, "a naive reference
//! interpreter … and the optimized executor must be bit-identical on
//! proptest-generated topologies").
//!
//! The graphs mix every test node in `common`: generators (`Const` returning
//! `Status::Constant`, `EnvProbe` reading `Env` and `Cx::arrival`), in-place
//! gains, sums, one-sample memory, latency-declaring delays (so PDC inserts
//! rings), bypass nodes, event emitters, event-latency nodes and
//! order-sensitive event consumers with fan-in — and feedback edges of both
//! kinds. Block sizes include 1, non-powers of two, and a schedule that varies
//! every block.
//!
//! Mutation notes for the suite as a whole (each was applied and seen to
//! fail, then reverted):
//!
//! - `kernels::AudioRing::run`: write before read (ring passes through) → the
//!   first graph with a PDC ring diverges.
//! - `event::merge_into`: `<` → `<=` (ties go to the later source) → a
//!   `Consumer` fed by two emitters on one tick diverges.
//! - `compile::colour::colour`: drop the `reach.before(u.writer, w)` half of
//!   `finished_before` → the verifier panics inside `compile` (debug) on the
//!   first graph with an unread output.
//! - `exec`: skip test `tail_elapsed` returning `true` for `Tail::Unbounded` →
//!   `Consumer` stops being called during silence and freezes → diverges.
//! - `exec::Executor::apply`: always build fresh rings (no carry) → the
//!   recompile property diverges on the first case with a surviving PDC ring.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{bits, input_signal, Kind, Pair};
use proptest::prelude::*;
use tutti_graph::{EventEdge, EventIn, EventOut, GraphSpec};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, NodeSpec, OutPort, Source};
use tutti_types::{ChannelLayout, NodeKey, Topology};

use tutti_graph::Node;

const MAX_BLOCK: usize = 128;

/// xorshift64* — the graph is a pure function of the seed, so a failing
/// proptest case replays exactly.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> Option<T> {
        (!xs.is_empty()).then(|| xs[self.below(xs.len() as u64) as usize])
    }
}

#[derive(Clone)]
struct Desc {
    kinds: BTreeMap<NodeKey, Kind>,
    /// Creation order. Direct edges only ever run earlier → later in it, so
    /// every generated graph is acyclic without feedback.
    order: Vec<NodeKey>,
    spec: GraphSpec,
    /// Every key ever used. A removed key is never reused: a new unit at an
    /// old key must come with a new generation, or it *is* the old node as far
    /// as the value can tell (`Editor` enforces this with a counter).
    used: BTreeSet<NodeKey>,
}

fn random_kind(rng: &mut Rng) -> Kind {
    // Latency-bearing nodes and multi-input sinks are weighted up: PDC only
    // happens where paths of different latency meet.
    match rng.below(16) {
        0 => Kind::Const {
            value: (rng.below(9) as f32 - 4.0) * 0.25 + 0.125,
            width: 1 + rng.below(2) as usize,
        },
        1 => Kind::Gain {
            gain: 0.25 + rng.below(8) as f32 * 0.25,
            width: 1 + rng.below(3) as usize,
        },
        2 | 10 | 11 => Kind::Sum {
            inputs: 1 + rng.below(3) as usize,
        },
        3 => Kind::Smooth,
        4 | 12 | 13 => Kind::Lag {
            latency: rng.below(24) as usize,
        },
        5 | 15 => Kind::Emitter {
            period: 3 + rng.below(40),
            phase: rng.below(10),
        },
        6 => Kind::Consumer {
            inputs: 1 + rng.below(2) as u16,
        },
        7 | 14 => Kind::EventLag {
            latency: rng.below(30) as usize,
        },
        8 => Kind::EnvProbe,
        _ => Kind::Thru {
            width: 1 + rng.below(2) as usize,
        },
    }
}

fn shape(kind: &Kind) -> tutti_graph::Shape {
    common::TestNode::new(kind.clone()).shape()
}

fn audio_outs(desc: &Desc, among: &[NodeKey]) -> Vec<OutPort> {
    among
        .iter()
        .flat_map(|&k| {
            (0..shape(&desc.kinds[&k]).audio_out.count()).map(move |port| OutPort { node: k, port })
        })
        .collect()
}

fn event_outs(desc: &Desc, among: &[NodeKey]) -> Vec<EventOut> {
    among
        .iter()
        .flat_map(|&k| {
            (0..shape(&desc.kinds[&k]).event_out).map(move |port| EventOut { node: k, port })
        })
        .collect()
}

/// (Re)wire every input of the node at creation index `j`.
fn wire_node(desc: &mut Desc, j: usize, rng: &mut Rng) {
    let key = desc.order[j];
    let s = shape(&desc.kinds[&key]);
    let earlier: Vec<NodeKey> = desc.order[..j].to_vec();
    let all = desc.order.clone();
    let direct = audio_outs(desc, &earlier);
    let any = audio_outs(desc, &all);
    for port in 0..s.audio_in.count() {
        let at = InPort { node: key, port };
        let roll = rng.below(100);
        let edge = if roll < 64 {
            rng.pick(&direct).map(|p| Edge::Direct(Source::Node(p)))
        } else if roll < 71 {
            Some(Edge::Direct(Source::Global(0)))
        } else if roll < 76 {
            Some(Edge::Direct(Source::Zero))
        } else if roll < 86 {
            rng.pick(&any)
                .map(|from| Edge::Feedback(FeedbackFrom { from }))
        } else {
            None
        };
        match edge {
            Some(e) => {
                desc.spec.topology.edges.insert(at, e);
            }
            None => {
                desc.spec.topology.edges.remove(&at);
            }
        }
    }
    let direct_ev = event_outs(desc, &earlier);
    let any_ev = event_outs(desc, &all);
    for port in 0..s.event_in {
        let at = EventIn { node: key, port };
        let mut seen = BTreeSet::new();
        let mut sources = Vec::new();
        for _ in 0..1 + rng.below(3) {
            let e = if rng.chance(85) {
                rng.pick(&direct_ev).map(EventEdge::Direct)
            } else {
                rng.pick(&any_ev).map(EventEdge::Feedback)
            };
            if let Some(e) = e {
                if seen.insert(e.from()) {
                    sources.push(e);
                }
            }
        }
        desc.spec.events.insert(at, sources);
    }
}

fn spec_for(kind: &Kind) -> NodeSpec {
    let s = shape(kind);
    NodeSpec::new("test", s.audio_in, s.audio_out)
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

fn wire_outputs(desc: &mut Desc, rng: &mut Rng) {
    let all = desc.order.clone();
    let outs = audio_outs(desc, &all);
    desc.spec.topology.outputs = (0..2)
        .map(|_| match rng.below(10) {
            0 => Source::Zero,
            1 => Source::Global(0),
            _ => rng.pick(&outs).map_or(Source::Zero, Source::Node),
        })
        .collect();
}

fn fresh_key(desc: &mut Desc, rng: &mut Rng) -> NodeKey {
    loop {
        let k = NodeKey(rng.below(1000));
        if desc.used.insert(k) {
            return k;
        }
    }
}

fn random_graph(seed: u64) -> Desc {
    let mut rng = Rng::new(seed);
    let mut desc = Desc {
        kinds: BTreeMap::new(),
        order: Vec::new(),
        used: BTreeSet::new(),
        spec: GraphSpec::new(Topology {
            inputs: ChannelLayout::MONO,
            ..Topology::default()
        }),
    };
    let n = 2 + rng.below(11) as usize;
    for _ in 0..n {
        let key = fresh_key(&mut desc, &mut rng);
        let kind = random_kind(&mut rng);
        desc.spec.topology.nodes.insert(key, spec_for(&kind));
        desc.kinds.insert(key, kind);
        desc.order.push(key);
    }
    for j in 0..n {
        wire_node(&mut desc, j, &mut rng);
    }
    wire_outputs(&mut desc, &mut rng);
    desc
}

/// An edit of `desc`: remove a node, add one, regenerate one, rewire some
/// ports — keeping every other key, so their state must carry.
fn mutate(desc: &Desc, rng: &mut Rng) -> Desc {
    let mut d = desc.clone();
    if d.order.len() > 1 && rng.chance(35) {
        let victim = d.order.remove(rng.below(d.order.len() as u64) as usize);
        d.kinds.remove(&victim);
        d.spec.generations.remove(&victim);
        let t = &mut d.spec.topology;
        t.nodes.remove(&victim);
        t.edges.retain(|at, e| {
            let from = match *e {
                Edge::Direct(Source::Node(p)) | Edge::Feedback(FeedbackFrom { from: p }) => {
                    Some(p.node)
                }
                Edge::Direct(_) => None,
            };
            at.node != victim && from != Some(victim)
        });
        for s in &mut t.outputs {
            if matches!(s, Source::Node(p) if p.node == victim) {
                *s = Source::Zero;
            }
        }
        d.spec.events.retain(|at, _| at.node != victim);
        for v in d.spec.events.values_mut() {
            v.retain(|e| e.from().node != victim);
        }
    }
    if rng.chance(40) {
        let key = fresh_key(&mut d, rng);
        let kind = random_kind(rng);
        d.spec.topology.nodes.insert(key, spec_for(&kind));
        d.kinds.insert(key, kind);
        d.order.push(key);
        let j = d.order.len() - 1;
        wire_node(&mut d, j, rng);
    }
    if rng.chance(35) {
        // Regenerate one node: a new unit (fresh state) at the same key, and
        // maybe a different kind — which can change its latency, and so every
        // PDC ring downstream of it.
        let j = rng.below(d.order.len() as u64) as usize;
        let key = d.order[j];
        let gen = d.spec.generation(key) + 1;
        d.spec.generations.insert(key, gen);
        if rng.chance(50) {
            let old = shape(&d.kinds[&key]);
            let kind = random_kind(rng);
            let new = shape(&kind);
            // Keep the port shape so edges stay valid; only swap when it
            // matches (a latency change, typically).
            if (old.audio_in, old.audio_out, old.event_in, old.event_out)
                == (new.audio_in, new.audio_out, new.event_in, new.event_out)
            {
                d.spec.topology.nodes.insert(key, spec_for(&kind));
                d.kinds.insert(key, kind);
            }
        }
    }
    for _ in 0..rng.below(3) {
        let j = rng.below(d.order.len() as u64) as usize;
        wire_node(&mut d, j, rng);
    }
    if rng.chance(30) {
        wire_outputs(&mut d, rng);
    }
    d
}

/// Block lengths for a run: fixed 1, 7, 64 or 100, or a different length
/// every block.
fn schedule(which: usize, seed: u64, frames: usize) -> Vec<usize> {
    let mut rng = Rng::new(seed ^ 0xB10C);
    let mut out = Vec::new();
    let mut total = 0;
    while total < frames {
        let n = match which {
            0 => 1,
            1 => 7,
            2 => 64,
            3 => 100,
            _ => 1 + rng.below(MAX_BLOCK as u64) as usize,
        };
        out.push(n);
        total += n;
    }
    out
}

fn run(pair: &mut Pair, blocks: &[usize], frame: &mut u64) {
    for &n in blocks {
        let input = input_signal(*frame, n);
        let (a, b) = pair.block(n, &input);
        assert_eq!(
            bits(&a),
            bits(&b),
            "executor and reference diverge in the block at frame {frame} ({n} frames)"
        );
        *frame += n as u64;
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The executor renders every random graph exactly as the reference does,
    /// at every block schedule.
    #[test]
    fn executor_is_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("generated graphs are valid");
        let mut pair = Pair::new(MAX_BLOCK);
        pair.switch(&valid, &desc.kinds);
        let mut frame = 0;
        run(&mut pair, &schedule(which, seed, 700), &mut frame);
    }

    /// The same across a recompile: nodes, rings and feedback slots that
    /// survive by key carry their state, in both interpreters, identically.
    #[test]
    fn recompiles_preserve_state_identically(seed in any::<u64>(), which in 0usize..5) {
        let first = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0x5EED);
        let second = mutate(&first, &mut rng);
        let third = mutate(&second, &mut rng);

        let mut pair = Pair::new(MAX_BLOCK);
        let mut frame = 0;
        for (i, desc) in [&first, &second, &third].into_iter().enumerate() {
            let valid = desc.spec.validate().expect("mutations stay valid");
            pair.switch(&valid, &desc.kinds);
            run(&mut pair, &schedule(which, seed.wrapping_add(i as u64), 300), &mut frame);
        }
    }
}

/// The generator is not vacuous: over a fixed range of seeds it produces the
/// features the suite claims to cover.
///
/// Mutation: make `random_kind` never return `Lag` → the PDC count is zero →
/// fails. Make `wire_node` never pick feedback → fails.
#[test]
fn the_generator_covers_what_the_suite_claims() {
    let (mut delays, mut event_delays, mut feedback, mut event_fb, mut merges, mut in_place) =
        (0, 0, 0, 0, 0, 0);
    for seed in 0..256u64 {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) =
            tutti_graph::compile(&valid, &common::shapes_of(&desc.kinds), None).expect("compiles");
        for op in plan.ops() {
            match op {
                tutti_graph::Op::Delay { .. } => delays += 1,
                tutti_graph::Op::EventDelay { .. } => event_delays += 1,
                tutti_graph::Op::Capture { .. } => feedback += 1,
                tutti_graph::Op::EventCapture { .. } => event_fb += 1,
                tutti_graph::Op::EventMerge { .. } => merges += 1,
                tutti_graph::Op::Node { in_place: m, .. } if m.0 != 0 => in_place += 1,
                _ => {}
            }
        }
    }
    let all = [
        ("audio PDC rings", delays),
        ("event PDC delays", event_delays),
        ("audio feedback", feedback),
        ("event feedback", event_fb),
        ("event fan-in merges", merges),
        ("in-place nodes", in_place),
    ];
    for (what, n) in all {
        eprintln!("{what}: {n}");
    }
    for (what, n) in all {
        assert!(n > 25, "only {n} {what} over 256 graphs");
    }
}

/// Every pair of values that share a slot, checked against reachability
/// computed here from scratch (a DFS over the plan's successor lists) rather
/// than by the compiler's bitsets or its verifier: either every op touching
/// one happens-before the other's writer, or the second is an in-place
/// overwrite by the first's last reader. Anything else could be run
/// concurrently by a parallel executor and would clobber a live buffer.
fn brute_force_interference(plan: &tutti_graph::Plan) {
    use tutti_graph::PortKind;
    let n = plan.ops().len();
    let succ = plan.op_successors();
    // reach[a] = every op reachable from a (strictly).
    let reach: Vec<BTreeSet<u32>> = (0..n)
        .map(|a| {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<u32> = succ.row(a).to_vec();
            while let Some(x) = stack.pop() {
                if seen.insert(x) {
                    stack.extend_from_slice(succ.row(x as usize));
                }
            }
            seen
        })
        .collect();
    let before = |a: u32, b: u32| reach[a as usize].contains(&b);
    let readers = |v: &tutti_graph::Value| {
        let r = v.readers;
        &plan.value_readers()[r.start as usize..(r.start + r.len) as usize]
    };
    for kind in [PortKind::Audio, PortKind::Event] {
        let values = plan.values(kind);
        for (i, u) in values.iter().enumerate() {
            for v in &values[i + 1..] {
                if u.slot != v.slot {
                    continue;
                }
                let (first, second) = if before(u.writer, v.writer) {
                    (u, v)
                } else {
                    (v, u)
                };
                let clean = std::iter::once(first.writer)
                    .chain(readers(first).iter().copied())
                    .all(|t| before(t, second.writer));
                let in_place = readers(first).contains(&second.writer)
                    && readers(first)
                        .iter()
                        .all(|&r| r == second.writer || before(r, second.writer));
                assert!(
                    clean || in_place,
                    "{kind:?} values written by ops {} and {} share slot {} but may overlap",
                    first.writer,
                    second.writer,
                    u.slot
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Colouring is correct under any parallel schedule: the verifier accepts
    /// every plan, and an independent brute-force check agrees.
    ///
    /// Mutation: in `colour::colour`, check only the occupant's *readers*
    /// against the new writer (drop the writer term of `finished_before`) →
    /// an unread output shares a slot with a concurrent value → the verifier
    /// panics inside `compile` (debug) and the brute force fails (release).
    #[test]
    fn colouring_is_safe_under_any_schedule(seed in any::<u64>()) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) = tutti_graph::compile(&valid, &common::shapes_of(&desc.kinds), None)
            .expect("compiles");
        tutti_graph::verify(&plan).expect("the verifier accepts it");
        brute_force_interference(&plan);
    }
}

/// State continuity, stated directly: recompiling to the same graph plus an
/// unrelated node renders exactly what never recompiling renders. The PDC
/// ring, the feedback slot, and the `Smooth`/`Lag` unit state all have to
/// carry for this to hold.
///
/// Mutation: in `Executor::apply`, build every ring fresh instead of taking
/// the old one → the PDC ring on `mix` restarts from silence → fails.
/// Mutation: in `apply`, do not copy old feedback slots → fails.
#[test]
fn an_unrelated_edit_does_not_disturb_the_running_graph() {
    let (a, b, c, mix, orphan) = (NodeKey(1), NodeKey(2), NodeKey(3), NodeKey(4), NodeKey(9));
    let mut kinds = BTreeMap::new();
    kinds.insert(a, Kind::EnvProbe);
    kinds.insert(b, Kind::Lag { latency: 37 });
    kinds.insert(c, Kind::Smooth);
    kinds.insert(mix, Kind::Sum { inputs: 3 });
    let mut t = Topology::default();
    for (&k, kind) in &kinds {
        t.nodes.insert(k, spec_for(kind));
    }
    let src = |node| Edge::Direct(Source::Node(OutPort { node, port: 0 }));
    t.edges.insert(InPort { node: b, port: 0 }, src(a));
    t.edges.insert(InPort { node: c, port: 0 }, src(a));
    t.edges.insert(InPort { node: mix, port: 0 }, src(b));
    t.edges.insert(InPort { node: mix, port: 1 }, src(c));
    t.edges.insert(
        InPort { node: mix, port: 2 },
        Edge::Feedback(FeedbackFrom {
            from: OutPort { node: c, port: 0 },
        }),
    );
    t.outputs = vec![Source::Node(OutPort { node: mix, port: 0 })];
    let before = GraphSpec::new(t.clone());
    let mut after_t = t;
    let mut kinds_after = kinds.clone();
    kinds_after.insert(
        orphan,
        Kind::Const {
            value: 1.0,
            width: 1,
        },
    );
    after_t
        .nodes
        .insert(orphan, spec_for(&kinds_after[&orphan]));
    let after = GraphSpec::new(after_t);

    let mut steady = Pair::new(MAX_BLOCK);
    let mut edited = Pair::new(MAX_BLOCK);
    steady.switch(&before.validate().unwrap(), &kinds);
    edited.switch(&before.validate().unwrap(), &kinds);
    assert_eq!(
        steady
            .plan
            .as_ref()
            .unwrap()
            .delay(tutti_graph::DelayKey::Audio(InPort { node: mix, port: 1 })),
        tutti_types::Samples(37),
        "the edit must land while a PDC ring is full of signal"
    );
    for block in 0..20u64 {
        let input = vec![0.0; 50];
        if block == 7 {
            edited.switch(&after.validate().unwrap(), &kinds_after);
        }
        let (x, _) = steady.block(50, &input);
        let (y, _) = edited.block(50, &input);
        assert_eq!(bits(&x), bits(&y), "block {block}");
    }
}
