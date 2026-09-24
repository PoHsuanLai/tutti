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
use tutti_types::{ChannelLayout, NodeKey, Samples, Topology};

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
        9 if rng.chance(50) => Kind::ThruInPlace {
            width: 1 + rng.below(2) as usize,
        },
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
        } else if roll < 76 {
            let width = u64::from(desc.spec.topology.inputs.count());
            Some(Edge::Direct(Source::Global(rng.below(width) as u16)))
        } else if roll < 80 {
            Some(Edge::Direct(Source::Zero))
        } else if roll < 90 {
            // Feedback delays of at least a block, sometimes longer.
            let delay = Samples(MAX_BLOCK + 17 * rng.below(3) as usize);
            rng.pick(&any)
                .map(|from| Edge::Feedback(FeedbackFrom::new(from, delay)))
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
        // Up to six sources, so fan-in wider than the common two or three is
        // exercised — and now and then a hub fed by every earlier event port
        // (a > 64-wide fan-in has its own test in `events.rs`).
        let hub = rng.chance(30);
        let tries = if hub {
            direct_ev.len() as u64 * 2
        } else {
            1 + rng.below(6)
        };
        for _ in 0..tries {
            let e = if hub || rng.chance(85) {
                rng.pick(&direct_ev).map(EventEdge::Direct)
            } else {
                let delay = Samples(MAX_BLOCK + 17 * rng.below(3) as usize);
                rng.pick(&any_ev).map(|f| EventEdge::feedback(f, delay))
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
    let width = u64::from(desc.spec.topology.inputs.count());
    desc.spec.topology.outputs = (0..1 + rng.below(4))
        .map(|_| match rng.below(10) {
            0 => Source::Zero,
            1 => Source::Global(rng.below(width) as u16),
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
            inputs: ChannelLayout::from_count(1 + rng.below(3) as u16),
            ..Topology::default()
        }),
    };
    let n = 2 + rng.below(11) as usize;
    // Some graphs are mostly event nodes, so that wide event fan-in (which
    // needs several earlier event outputs) turns up regularly.
    let event_heavy = rng.chance(25);
    for _ in 0..n {
        let key = fresh_key(&mut desc, &mut rng);
        let kind = if event_heavy && rng.chance(80) {
            match rng.below(4) {
                0 | 1 => Kind::Emitter {
                    period: 3 + rng.below(40),
                    phase: rng.below(10),
                },
                2 => Kind::EventLag {
                    latency: rng.below(30) as usize,
                },
                _ => Kind::Consumer {
                    inputs: 1 + rng.below(2) as u16,
                },
            }
        } else {
            random_kind(&mut rng)
        };
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
                Edge::Direct(Source::Node(p)) | Edge::Feedback(FeedbackFrom { from: p, .. }) => {
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

    /// With scheduled commands landing on every event input — on emitters'
    /// frames (ties), on each other's frames, already late, and at the next
    /// block — the two still agree bit for bit: the order-sensitive
    /// `Consumer` sees scheduled events after the port's own on a tie, in
    /// scheduling order, in both.
    ///
    /// Mutation: in `CommandRx::overlay`, merge the scheduled events
    /// *before* the port's own (`[due, base]`) → ties flip in the executor
    /// only → diverges. Mutation: sort `due` by *reversed* scheduling order
    /// on a tie → equal-offset commands reorder → diverges.
    #[test]
    fn scheduled_commands_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("generated graphs are valid");
        let mut pair = Pair::new(MAX_BLOCK);
        pair.switch(&valid, &desc.kinds);
        let ports: Vec<EventIn> = desc
            .kinds
            .keys()
            .flat_map(|&k| (0..shape(&desc.kinds[&k]).event_in).map(move |port| EventIn { node: k, port }))
            .collect();
        let mut rng = Rng::new(seed ^ 0xC0DE);
        let mut frame = 0;
        let blocks = schedule(which, seed, 700);
        let (first, rest) = blocks.split_at(blocks.len() / 2);
        run(&mut pair, first, &mut frame);
        for _ in 0..rng.below(40) {
            let Some(to) = rng.pick(&ports) else { break };
            // Few distinct frames, so commands tie with each other and with
            // emitters; some already past.
            let at = match rng.below(8) {
                0 => tutti_types::At::NextBlock,
                1 => tutti_types::At::Frame(tutti_types::Frame(rng.below(frame + 1))),
                _ => tutti_types::At::Frame(tutti_types::Frame(frame + 3 * rng.below(100))),
            };
            let kind = tutti_graph::EventKind::Midi(tutti_graph::Ump([rng.below(1000) as u32, 0, 0, 0]));
            pair.editor.schedule(at, to, kind).expect("room");
            pair.reference.schedule(at, to, kind);
        }
        run(&mut pair, rest, &mut frame);
        prop_assert_eq!(pair.exec.late_commands(), pair.reference.late_commands());
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
    let (mut wide, mut global_delays, mut bypass_in_place, mut multi_io) = (0, 0, 0, 0);
    for seed in 0..512u64 {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) = tutti_graph::compile(
            &valid,
            &common::shapes_of(&desc.kinds),
            &common::prepare(MAX_BLOCK),
            None,
        )
        .expect("compiles");
        if desc.spec.topology.inputs.count() > 1 && desc.spec.topology.outputs.len() > 2 {
            multi_io += 1;
        }
        for d in plan.delays() {
            if matches!(
                d.key,
                tutti_graph::DelayKey::Audio {
                    from: Source::Global(_),
                    ..
                }
            ) {
                global_delays += 1;
            }
        }
        for (k, kind) in &desc.kinds {
            if matches!(kind, Kind::ThruInPlace { .. }) && plan.in_place(*k).0 != 0 {
                bypass_in_place += 1;
            }
        }
        for op in plan.ops() {
            match op {
                tutti_graph::Op::EventMerge { srcs, .. } if srcs.len > 3 => {
                    wide += 1;
                    merges += 1;
                }
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
        ("event fan-ins wider than 3", wide),
        ("delayed global inputs", global_delays),
        ("in-place nodes returning Bypass", bypass_in_place),
        ("graphs with >1 input and >2 outputs", multi_io),
    ];
    for (what, n) in all {
        eprintln!("{what}: {n}");
    }
    // The four added with the review are rarer by construction (each needs
    // two features to meet); ten over 512 graphs still exercises each.
    for (i, (what, n)) in all.iter().enumerate() {
        let min = if i < 6 { 25 } else { 10 };
        assert!(*n > min, "only {n} {what} over 512 graphs");
    }
}

/// Add a direct back edge — audio or event — from a node to one created no
/// later than it, closing a cycle no feedback edge breaks.
fn close_a_cycle(desc: &mut Desc, rng: &mut Rng) -> bool {
    let n = desc.order.len();
    for _ in 0..32 {
        let (i, j) = (rng.below(n as u64) as usize, rng.below(n as u64) as usize);
        let (early, late) = (desc.order[i.min(j)], desc.order[i.max(j)]);
        let (se, sl) = (shape(&desc.kinds[&early]), shape(&desc.kinds[&late]));
        // `late` must depend on `early` for the back edge to close a cycle.
        let reaches = |d: &Desc| {
            let mut stack = vec![early];
            let mut seen = BTreeSet::new();
            while let Some(k) = stack.pop() {
                if k == late {
                    return true;
                }
                if !seen.insert(k) {
                    continue;
                }
                for (at, e) in &d.spec.topology.edges {
                    if let Edge::Direct(Source::Node(p)) = e {
                        if p.node == k {
                            stack.push(at.node);
                        }
                    }
                }
                for (at, v) in &d.spec.events {
                    if v.iter()
                        .any(|e| matches!(e, EventEdge::Direct(f) if f.node == k))
                    {
                        stack.push(at.node);
                    }
                }
            }
            false
        };
        if !reaches(desc) {
            continue;
        }
        if se.audio_in.count() > 0 && sl.audio_out.count() > 0 {
            desc.spec.topology.edges.insert(
                InPort {
                    node: early,
                    port: 0,
                },
                Edge::Direct(Source::Node(OutPort {
                    node: late,
                    port: 0,
                })),
            );
            return true;
        }
        let from = EventOut {
            node: late,
            port: 0,
        };
        let listed = desc
            .spec
            .events
            .get(&EventIn {
                node: early,
                port: 0,
            })
            .is_some_and(|v| v.iter().any(|e| e.from() == from));
        if se.event_in > 0 && sl.event_out > 0 && !listed {
            desc.spec.connect_events(
                EventIn {
                    node: early,
                    port: 0,
                },
                EventEdge::Direct(EventOut {
                    node: late,
                    port: 0,
                }),
            );
            return true;
        }
    }
    false
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// A cycle no feedback edge breaks is rejected — by `validate` when it is
    /// all audio, by `compile` (naming its edges) when it runs through an
    /// event edge — and never reaches an executor.
    ///
    /// Mutation: in `compile`, skip the SCC error (`if !cycle.is_empty()`) →
    /// a graph with an event back edge compiles (and `kahn` asserts in debug)
    /// → fails.
    #[test]
    fn unbroken_cycles_are_rejected(seed in any::<u64>()) {
        let mut desc = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0xC1C1E);
        prop_assume!(close_a_cycle(&mut desc, &mut rng));
        let rejected = match desc.spec.validate() {
            Err(errs) => errs.iter().any(|e| matches!(
                e,
                tutti_graph::GraphInvalid::Topology(tutti_types::graph::Invalid::Cycle { .. })
            )),
            Ok(valid) => matches!(
                tutti_graph::compile(
                    &valid,
                    &common::shapes_of(&desc.kinds),
                    &common::prepare(MAX_BLOCK),
                    None,
                ),
                Err(tutti_graph::CompileError::Cycle { .. })
            ),
        };
        prop_assert!(rejected, "a direct cycle got through");
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
        let (plan, _) = tutti_graph::compile(&valid, &common::shapes_of(&desc.kinds), &common::prepare(MAX_BLOCK), None)
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
        Edge::Feedback(FeedbackFrom::one_block(
            OutPort { node: c, port: 0 },
            Samples(MAX_BLOCK),
        )),
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
            .delay(tutti_graph::DelayKey::Audio {
                at: InPort { node: mix, port: 1 },
                from: Source::Node(OutPort { node: c, port: 0 })
            }),
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

/// Every way the executor borrows a node's buffers renders what the
/// reference renders:
///
/// - the one-channel direct forms: a source, a one-in/one-out node in two
///   slots, and one in place;
/// - the stereo direct form in each of its shapes (0→2, 1→2, 2→1 including
///   two ports on one slot, 2→2 split, in place and half in place);
/// - the audio-only walk in each stack-table bucket: up to 4, up to 16, and
///   a 64-input sum (65 audio ports, no events);
/// - the general walk (event ports) in each of its buckets:
///   - `<4, 4>`, with and without an in-place channel;
///   - `<16, 4>`, six in-place channels plus an event input;
///   - `<MAX_PORTS, MAX_PORTS>`, reached both by 20 audio channels with
///     events and by six event inputs.
///
/// The random generator keeps nodes narrow, so the wide buckets are covered
/// here and nowhere else.
///
/// Mutation: in `NodeTables::lower`, map the `(&[input], &[out], true)` shape
/// to `Form::InPlace { slot: out }` → the verifier refuses the plan inside
/// `compile` (and the one-input `Sum` would otherwise get no input buffer) →
/// fails. Mutation: in the `Form::Audio` arm, send every width above 16 to
/// `run_audio::<16>` → the 20- and 64-wide nodes overrun the table → panics.
/// Mutation: in the `Form::General` arm, always call `run::<4, 4>` → the
/// wider general nodes overrun the tables → panics (so does sending only the
/// `<16, 4>` arm there). Mutation: build the general call's `Io` with
/// `InPlaceMask::NONE` → the in-place mixers are handed no input → fails.
/// Mutation: in `Arena::direct`, hand output `c` to channel `O - 1 - c` →
/// the stereo nodes swap channels → diverges. Mutation: make every direct
/// input read `reads[0]` → the stereo-to-mono sum reads one slot twice →
/// diverges.
#[test]
fn every_borrow_form_matches_the_reference() {
    let key = NodeKey;
    let mut kinds = BTreeMap::new();
    let (src, split, inplace, six, twenty, sum, emit, consume) = (
        key(1),
        key(2),
        key(3),
        key(4),
        key(5),
        key(6),
        key(7),
        key(8),
    );
    let (pre6, mix6, mixwide, manyev, pre2, mix2) =
        (key(9), key(10), key(11), key(12), key(13), key(14));
    kinds.insert(
        src,
        Kind::Const {
            value: 0.375,
            width: 1,
        },
    );
    kinds.insert(split, Kind::Sum { inputs: 1 });
    kinds.insert(
        inplace,
        Kind::Gain {
            gain: 0.5,
            width: 1,
        },
    );
    kinds.insert(
        six,
        Kind::Gain {
            gain: 0.75,
            width: 6,
        },
    );
    kinds.insert(
        twenty,
        Kind::Gain {
            gain: 1.25,
            width: 20,
        },
    );
    kinds.insert(sum, Kind::Sum { inputs: 64 });
    kinds.insert(
        emit,
        Kind::Emitter {
            period: 5,
            phase: 2,
        },
    );
    kinds.insert(consume, Kind::Consumer { inputs: 1 });
    kinds.insert(
        pre6,
        Kind::Gain {
            gain: 0.5,
            width: 6,
        },
    );
    kinds.insert(
        mix6,
        Kind::Mixed {
            width: 6,
            events_in: 1,
            events_out: 0,
        },
    );
    kinds.insert(
        mixwide,
        Kind::Mixed {
            width: 20,
            events_in: 1,
            events_out: 1,
        },
    );
    kinds.insert(
        manyev,
        Kind::Mixed {
            width: 1,
            events_in: 6,
            events_out: 2,
        },
    );
    kinds.insert(
        pre2,
        Kind::Gain {
            gain: 0.25,
            width: 2,
        },
    );
    kinds.insert(
        mix2,
        Kind::Mixed {
            width: 2,
            events_in: 1,
            events_out: 1,
        },
    );

    let mut t = Topology {
        inputs: ChannelLayout::STEREO,
        ..Topology::default()
    };
    for (&k, kind) in &kinds {
        t.nodes.insert(k, spec_for(kind));
    }
    let out = |node, port| Source::Node(OutPort { node, port });
    let mut wire = |node, port, from| {
        t.edges.insert(InPort { node, port }, Edge::Direct(from));
    };
    wire(split, 0, Source::Global(0));
    wire(inplace, 0, out(split, 0));
    for c in 0..6 {
        wire(
            six,
            c,
            if c % 2 == 0 {
                Source::Global(1)
            } else {
                out(src, 0)
            },
        );
    }
    for c in 0..20 {
        wire(
            twenty,
            c,
            if c < 6 {
                out(six, c)
            } else {
                Source::Global(0)
            },
        );
    }
    // Every other port of the sum reads something different; the rest read
    // nothing, so the zero slot is borrowed many times over.
    let feeds: Vec<Source> = (0..20)
        .map(|c| out(twenty, c))
        .chain([out(inplace, 0), out(emit, 0), out(consume, 0)])
        .collect();
    for c in (0..64u16).step_by(2) {
        wire(sum, c, feeds[c as usize / 2 % feeds.len()]);
    }
    // `pre6` and `pre2` feed only their mixer, so the mixer's channels are
    // aliased in place; `mixwide` reads a global input, which cannot be.
    for c in 0..6 {
        wire(pre6, c, Source::Global(c % 2));
        wire(mix6, c, out(pre6, c));
    }
    for c in 0..20 {
        wire(mixwide, c, Source::Global(1 - c % 2));
    }
    for c in 0..2 {
        wire(pre2, c, Source::Global(c));
        wire(mix2, c, out(pre2, c));
    }
    // The direct stereo shapes: a stereo source, mono to stereo, stereo to
    // mono (once from two slots, once with both ports on one slot, so the
    // record dedupes a read), stereo to stereo split, fully in place and
    // half in place; and a three-wide node, which stays on the walk.
    let (st_src, fan12, sum2, sum2dup, st_pre, st_in, half, w3) = (
        key(20),
        key(21),
        key(22),
        key(23),
        key(24),
        key(25),
        key(26),
        key(27),
    );
    let direct_kinds = [
        (
            st_src,
            Kind::Const {
                value: 0.625,
                width: 2,
            },
        ),
        (
            fan12,
            Kind::Spec {
                behaviour: common::SpecBehaviour::Fan,
                ins: 1,
                outs: 2,
                latency: 0,
                tail: tutti_types::Tail::None,
            },
        ),
        (sum2, Kind::Sum { inputs: 2 }),
        (sum2dup, Kind::Sum { inputs: 2 }),
        (
            st_pre,
            Kind::Gain {
                gain: 0.5,
                width: 2,
            },
        ),
        (
            st_in,
            Kind::Gain {
                gain: 1.5,
                width: 2,
            },
        ),
        (
            half,
            Kind::Gain {
                gain: 0.75,
                width: 2,
            },
        ),
        (
            w3,
            Kind::Gain {
                gain: 2.0,
                width: 3,
            },
        ),
    ];
    for (k, kind) in direct_kinds {
        t.nodes.insert(k, spec_for(&kind));
        kinds.insert(k, kind);
    }
    let mut wire = |node, port, from| {
        t.edges.insert(InPort { node, port }, Edge::Direct(from));
    };
    wire(fan12, 0, Source::Global(1));
    wire(sum2, 0, out(st_src, 0));
    wire(sum2, 1, out(fan12, 1));
    wire(sum2dup, 0, Source::Global(0));
    wire(sum2dup, 1, Source::Global(0));
    for c in 0..2 {
        wire(st_pre, c, Source::Global(c));
        wire(st_in, c, out(st_pre, c));
    }
    // `half` is the last reader of `fan12`'s port 0 only: channel 0 is in
    // place, channel 1 reads a global input.
    wire(half, 0, out(fan12, 0));
    wire(half, 1, Source::Global(1));
    for c in 0..3 {
        wire(w3, c, Source::Global(c % 2));
    }
    t.outputs = vec![
        out(sum, 0),
        out(inplace, 0),
        out(twenty, 19),
        out(mix6, 5),
        out(mixwide, 19),
        out(manyev, 0),
        out(mix2, 1),
        out(st_src, 1),
        out(sum2, 0),
        out(sum2dup, 0),
        out(st_in, 1),
        out(half, 0),
        out(half, 1),
        out(w3, 2),
    ];
    let mut g = GraphSpec::new(t);
    let ev = |node, port| EventOut { node, port };
    let mut connect = |node, port, from| {
        g.connect_events(EventIn { node, port }, EventEdge::Direct(from));
    };
    connect(consume, 0, ev(emit, 0));
    connect(mix6, 0, ev(emit, 0));
    connect(mixwide, 0, ev(emit, 0));
    for p in 0..6 {
        connect(
            manyev,
            p,
            if p % 2 == 0 {
                ev(emit, 0)
            } else {
                ev(mixwide, 0)
            },
        );
    }
    connect(mix2, 0, ev(manyev, 1));
    let valid = g.validate().expect("valid");

    let mut pair = Pair::new(MAX_BLOCK);
    pair.switch(&valid, &kinds);

    // The graph really reaches every form and bucket (derived from the
    // public op the same way `NodeTables::lower` and the executor do).
    let plan = pair.plan.clone().expect("switched");
    let mut forms = BTreeSet::new();
    for op in plan.ops() {
        if let tutti_graph::Op::Node {
            audio_in,
            audio_out,
            event_in,
            event_out,
            in_place,
            ..
        } = *op
        {
            let events = event_in.len + event_out.len > 0;
            let wide = audio_in.len.max(audio_out.len);
            let ewide = event_in.len.max(event_out.len);
            if events && in_place.0 != 0 {
                forms.insert("general, in place");
            }
            forms.insert(match (audio_in.len, audio_out.len, events) {
                (_, _, true) if wide <= 4 && ewide <= 4 => "general 4/4",
                (_, _, true) if wide <= 16 && ewide <= 4 => "general 16/4",
                (_, _, true) => "general wide",
                (0, 1, false) => "source",
                (1, 1, false) if in_place.get(0) => "in place",
                (1, 1, false) => "split",
                (0, 2, false) => "direct 0-2",
                (1, 2, false) => "direct 1-2",
                (2, 1, false) => "direct 2-1",
                (2, 2, false) => match in_place.0 {
                    0 => "direct 2-2 split",
                    0b11 => "direct 2-2 in place",
                    _ => "direct 2-2 half in place",
                },
                _ if wide <= 4 => "audio up to 4",
                _ if wide <= 16 => "audio up to 16",
                _ => "audio up to 64",
            });
        }
    }
    assert_eq!(
        forms,
        BTreeSet::from([
            "general 4/4",
            "general 16/4",
            "general wide",
            "general, in place",
            "source",
            "in place",
            "split",
            "direct 0-2",
            "direct 1-2",
            "direct 2-1",
            "direct 2-2 split",
            "direct 2-2 in place",
            "direct 2-2 half in place",
            "audio up to 4",
            "audio up to 16",
            "audio up to 64"
        ]),
        "the graph must reach every borrow form and bucket"
    );
    // Both routes into the widest general bucket, and in place in the two
    // narrower ones.
    let node = |k| {
        plan.ops()
            .iter()
            .find_map(|op| match *op {
                tutti_graph::Op::Node {
                    unit,
                    audio_in,
                    event_in,
                    in_place,
                    ..
                } if plan.units()[unit as usize].key == k => {
                    Some((audio_in.len, event_in.len, in_place.0))
                }
                _ => None,
            })
            .expect("placed")
    };
    assert_eq!(node(mixwide).0, 20);
    assert_eq!(node(manyev).1, 6);
    assert_eq!(node(mix6).2, 0b11_1111, "six in-place channels");
    assert_eq!(node(mix2).2, 0b11, "two in-place channels");

    let mut frame = 0;
    for which in 0..5 {
        run(&mut pair, &schedule(which, 7, 300), &mut frame);
    }
}
