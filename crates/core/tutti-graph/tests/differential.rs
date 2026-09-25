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
    /// Keys `mutate_with_fades` crossfaded in its last round: where fades
    /// are likeliest to be running or waiting, so the next round aims
    /// replaces and `set_latency` there often enough to queue and cut them.
    recent: Vec<NodeKey>,
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
        recent: Vec::new(),
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

/// A transport, played: it rolls at `tempo`, and `block` sometimes seeks,
/// stops or starts, changes the tempo inside a block (a linear ramp, or a
/// step at a random offset), or toggles a short loop.
struct Script {
    beat: f64,
    playing: bool,
    tempo: f64,
    looping: Option<(f64, f64)>,
}

impl Script {
    /// The transport for the next block of `n` frames at 48 kHz, then
    /// advance past it.
    fn block(&mut self, n: usize, rng: &mut Rng) -> tutti_graph::Transport {
        // A tempo change is ramped linearly across this block, as a host
        // automating tempo reports it: the block says its starting tempo,
        // and the position advances by the average.
        let mut next_tempo = self.tempo;
        match rng.below(40) {
            0 | 1 => self.beat = rng.below(4000) as f64 / 1000.0,
            2 => self.playing = !self.playing,
            3 | 4 => next_tempo = 600.0 + rng.below(1800) as f64,
            5 | 6 => {
                self.looping = match self.looping {
                    Some(_) => None,
                    None => {
                        let start = (self.beat + rng.below(100) as f64 / 1000.0 - 0.02).max(0.0);
                        Some((start, start + 0.05 + rng.below(250) as f64 / 1000.0))
                    }
                }
            }
            _ => {}
        }
        let t = tutti_graph::Transport::new(
            self.playing,
            tutti_types::Bpm(self.tempo),
            tutti_types::Beat(self.beat),
            self.looping.map(|(start, end)| tutti_graph::LoopRange {
                start: tutti_types::Beat(start),
                end: tutti_types::Beat(end),
            }),
        );
        // Half the changes ramp linearly across the block; half step at a
        // random offset inside it. Either way the block reports its starting
        // tempo, and the next block the new one.
        let beats = |frames: f64, tempo: f64| frames * tempo / 60.0 / 48_000.0;
        let advance = if next_tempo != self.tempo && rng.chance(50) {
            let at = rng.below(n as u64 + 1) as f64;
            beats(at, self.tempo) + beats(n as f64 - at, next_tempo)
        } else {
            beats(n as f64, 0.5 * (self.tempo + next_tempo))
        };
        self.tempo = next_tempo;
        if self.playing {
            let x = self.beat + advance;
            self.beat = match self.looping {
                Some((start, end)) if self.beat < end && x >= end => {
                    start + (x - end) % (end - start)
                }
                _ => x,
            };
        }
        t
    }
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
    /// frames (ties), on each other's frames, late, and at the next block —
    /// the two still agree bit for bit, and on late and unrouted counts.
    ///
    /// The transport is played by a small script: it rolls, seeks, stops and
    /// starts, changes tempo between blocks, and loops (short loops, so they
    /// wrap many times). Commands are timed by frame and by beat — beats
    /// ahead of the playhead, behind it, and inside the loop — scheduled all
    /// through the run, and the graph is recompiled halfway, so some lose
    /// their node. The reference resolves beats with its own interval-based
    /// playhead, sharing no code with the executor's `Playhead` and
    /// `Env::due_at_arrival`, so a time-logic bug in either diverges here.
    ///
    /// Mutation: after a wrap, call the whole loop crossed (`beat < loop end`
    /// in `Playhead::crossed`, the reviewed bug) → a beat ahead of the
    /// playhead fires late in the executor only → diverges. Mutation: in
    /// `CommandRx::overlay`, merge the scheduled events *before* the port's
    /// own → ties flip → diverges. Mutation: never feed the executor's
    /// playhead → diverges. Mutation: accept only the steady advance at the
    /// block's own tempo in the executor → a tempo ramp or step breaks its
    /// run but not the reference's → diverges. (The
    /// frame-sized slack itself is pinned by `time::tests`: the script's
    /// positions are exact, so a 1e-6-beat slack would pass here.)
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
        let mut script = Script {
            beat: 0.0,
            playing: true,
            tempo: 1_200.0,
            looping: None,
        };
        let blocks = schedule(which, seed, 900);
        let half = blocks.len() / 2;
        let mut frame = 0u64;
        for (i, &n) in blocks.iter().enumerate() {
            if i == half {
                // Recompile before most of the commands land.
                let edited = mutate(&desc, &mut rng);
                pair.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
            }
            let t = script.block(n, &mut rng);
            if rng.chance(25) {
                for _ in 0..1 + rng.below(3) {
                    let Some(to) = rng.pick(&ports) else { break };
                    let at = match rng.below(10) {
                        0 => tutti_types::At::NextBlock,
                        1 => tutti_types::At::Frame(tutti_types::Frame(rng.below(frame + 1))),
                        2 => tutti_types::At::Frame(tutti_types::Frame(frame + 3 * rng.below(100))),
                        3 if script.looping.is_some() => {
                            let (a, b) = script.looping.expect("looping");
                            tutti_types::At::Beat(tutti_types::Beat(a + (b - a) * rng.below(1000) as f64 / 1000.0))
                        }
                        // Around the playhead: behind it by up to 0.05
                        // beat, ahead by up to 0.25.
                        _ => tutti_types::At::Beat(tutti_types::Beat(
                            (t.beat().get() + (rng.below(3000) as f64 - 500.0) / 10_000.0).max(0.0),
                        )),
                    };
                    let kind = tutti_graph::EventKind::Midi(tutti_graph::Ump([rng.below(1000) as u32, 0, 0, 0]));
                    // Pending beats hold credit: when it runs out, neither
                    // side gets the command.
                    match pair.editor.schedule(at, to, kind) {
                        Ok(_) => {
                            pair.reference.schedule(at, to, kind);
                        }
                        // Or its node went in the recompile.
                        Err(tutti_graph::ScheduleError::Backpressure | tutti_graph::ScheduleError::NoSuchPort { .. }) => {}
                        Err(e) => panic!("{e}"),
                    }
                }
            }
            let input = input_signal(frame, n);
            let (a, b) = pair.block_at(n, &input, &t);
            prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {}", frame);
            prop_assert_eq!(pair.exec.dropped_events(), 0);
            prop_assert_eq!(pair.exec.late_commands(), pair.reference.late_commands(), "late, frame {}", frame);
            frame += n as u64;
        }
        prop_assert_eq!(pair.exec.unrouted_commands(), pair.reference.unrouted_commands());
    }

    /// Scheduled commands and `Env`, with the transport changing **inside**
    /// blocks (doc 013 §6, the engine's timestamped transport commands):
    /// both interpreters see the same changes, resolve a beat against the
    /// transport in force where playback reaches it (a beat just after a
    /// mid-block start or seek lands in that block), and agree on late.
    ///
    /// Mutation: make `Env::due` resolve beats against the block-start
    /// transport only (drop the segment walk) → a beat reached after a
    /// mid-block start or seek lands in the executor a block later or never
    /// → diverges. Make `Playhead::observe` observe only the block's first
    /// segment → late counts diverge after a mid-block seek. Drop `changes`
    /// from the `Env` the executor builds → the probe's per-frame transport
    /// differs → diverges.
    #[test]
    fn transport_changes_inside_blocks_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("generated graphs are valid");
        let mut pair = Pair::new(MAX_BLOCK);
        pair.switch(&valid, &desc.kinds);
        let ports: Vec<EventIn> = desc
            .kinds
            .keys()
            .flat_map(|&k| (0..shape(&desc.kinds[&k]).event_in).map(move |port| EventIn { node: k, port }))
            .collect();
        let mut rng = Rng::new(seed ^ 0x7A45);
        let mut script = Script {
            beat: 0.0,
            playing: false,
            tempo: 1_200.0,
            looping: None,
        };
        let mut frame = 0u64;
        for n in schedule(which, seed, 900) {
            // Cut the block at up to three offsets; each piece is a script
            // step of its own, so a piece may start with a seek, a start or
            // stop, a tempo step or a loop edit.
            let mut cuts: Vec<usize> = (0..rng.below(4))
                .filter(|_| n > 1)
                .map(|_| 1 + rng.below(n as u64 - 1) as usize)
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            let mut bounds = vec![0];
            bounds.extend(&cuts);
            bounds.push(n);
            let start = script.block(bounds[1] - bounds[0], &mut rng);
            let mut changes = tutti_graph::TransportChanges::NONE;
            for w in bounds[1..].windows(2) {
                let to = script.block(w[1] - w[0], &mut rng);
                let at = tutti_graph::Offset::new(w[0], tutti_types::Samples(n)).expect("inside");
                changes.push(at, to).expect("ordered, distinct, few");
            }
            if rng.chance(30) {
                for _ in 0..1 + rng.below(3) {
                    let Some(to) = rng.pick(&ports) else { break };
                    let at = match rng.below(6) {
                        0 => tutti_types::At::NextBlock,
                        1 => tutti_types::At::Frame(tutti_types::Frame(frame + rng.below(2 * n as u64))),
                        // Near a piece's start beat, before or after it.
                        _ => {
                            let pieces: Vec<f64> = std::iter::once(start.beat().get())
                                .chain(changes.as_slice().iter().map(|c| c.to.beat().get()))
                                .collect();
                            let b = pieces[rng.below(pieces.len() as u64) as usize];
                            tutti_types::At::Beat(tutti_types::Beat(
                                (b + (rng.below(600) as f64 - 100.0) / 10_000.0).max(0.0),
                            ))
                        }
                    };
                    let kind = tutti_graph::EventKind::Midi(tutti_graph::Ump([rng.below(1000) as u32, 0, 0, 0]));
                    match pair.editor.schedule(at, to, kind) {
                        Ok(_) => {
                            pair.reference.schedule(at, to, kind);
                        }
                        Err(tutti_graph::ScheduleError::Backpressure) => {}
                        Err(e) => panic!("{e}"),
                    }
                }
            }
            let input = input_signal(frame, n);
            let (a, b) = pair.block_with_changes(n, &input, &start, &changes);
            prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {}", frame);
            prop_assert_eq!(pair.exec.late_commands(), pair.reference.late_commands(), "late, frame {}", frame);
            frame += n as u64;
        }
        prop_assert_eq!(pair.exec.unrouted_commands(), pair.reference.unrouted_commands());
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

/// The executor and the reference built from one random graph through an
/// `Editor` (not `package`), so `Editor::reprepare` has a spec to recompile.
struct EditorPair {
    editor: tutti_graph::Editor,
    exec: tutti_graph::Executor,
    reference: tutti_graph::Reference,
    inputs: usize,
    outputs: usize,
}

impl EditorPair {
    fn new(desc: &Desc, prepare: tutti_graph::Prepare) -> Self {
        let (mut editor, mut exec) =
            tutti_graph::Editor::with_event_capacity(prepare, common::EVENT_CAPACITY);
        for (&k, kind) in &desc.kinds {
            editor.insert(k, "test", common::TestNode::new(kind.clone()));
        }
        let spec = editor.spec_mut();
        spec.topology.edges = desc.spec.topology.edges.clone();
        spec.topology.outputs = desc.spec.topology.outputs.clone();
        spec.topology.inputs = desc.spec.topology.inputs;
        spec.events = desc.spec.events.clone();
        editor.commit().expect("a generated graph commits");
        exec.apply_pending();
        editor.collect();
        let mut reference = tutti_graph::Reference::new(prepare);
        reference.set_graph(
            &editor.spec().validate().expect("valid"),
            common::units_for(&desc.kinds, desc.kinds.keys().copied()),
        );
        Self {
            editor,
            exec,
            reference,
            inputs: desc.spec.topology.inputs.count() as usize,
            outputs: desc.spec.topology.outputs.len(),
        }
    }

    /// Re-prepare both, rendering `suspended` blocks between the two
    /// halves: silence in both, the clock counting in both.
    fn reprepare(&mut self, prepare: tutti_graph::Prepare, suspended: &[usize], frame: &mut u64) {
        self.editor.reprepare(prepare).expect("reprepares");
        self.reference.suspend(prepare);
        if suspended.is_empty() {
            self.exec.apply_pending();
        }
        self.run(suspended, frame);
        self.editor.collect(); // the resume
        self.reference.resume();
    }

    /// Schedule `kind` into `to` at `at` on both.
    fn schedule(&mut self, at: tutti_types::At, to: EventIn, kind: tutti_graph::EventKind) {
        self.editor.schedule(at, to, kind).expect("room");
        self.reference.schedule(at, to, kind);
    }

    fn run(&mut self, blocks: &[usize], frame: &mut u64) {
        let transport = tutti_graph::Transport::new(
            true,
            tutti_types::Bpm(120.0),
            tutti_types::Beat(0.0),
            None,
        );
        for &n in blocks {
            let input = input_signal(*frame, n);
            let chans: Vec<Vec<f32>> = (0..self.inputs.max(1))
                .map(|c| input.iter().map(|x| x * 0.5f32.powi(c as i32)).collect())
                .collect();
            let ins: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
            let mut a = vec![vec![0.0f32; n]; self.outputs];
            let mut b = vec![vec![0.0f32; n]; self.outputs];
            {
                let mut outs: Vec<&mut [f32]> = a.iter_mut().map(Vec::as_mut_slice).collect();
                self.exec.process(n, &transport, &ins, &mut outs);
            }
            {
                let mut outs: Vec<&mut [f32]> = b.iter_mut().map(Vec::as_mut_slice).collect();
                self.reference.process(n, &transport, &ins, &mut outs);
            }
            assert_eq!(self.exec.dropped_events(), 0, "the executor dropped events");
            assert_eq!(
                bits(&a),
                bits(&b),
                "executor and reference diverge in the block at frame {frame} ({n} frames)"
            );
            *frame += n as u64;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Across a re-prepare — a new sample rate, a new `MaxBlock`, or both —
    /// the executor (two commits through the editor, units re-prepared on the
    /// control side) and the reference (one step, with the reset rule written
    /// out independently) still agree bit for bit, block schedules ragged on
    /// both sides of the change — with zero to two blocks rendered while
    /// suspended, and frame-timed commands pending across it (rescaled to
    /// their wall-clock time on a rate change).
    ///
    /// Mutation: in `Executor::rebuild`, carry state across a rate change
    /// (`carry = true`) → the first graph with a PDC ring or feedback
    /// diverges after a rate change. Mutation: in `Reference::set_graph`,
    /// keep the audio lines on a reset → diverges the same way from the
    /// other side. Mutation: in `Executor::apply`, drop the `reset_time`
    /// flag on suspend → diverges. Mutation: stop the executor's clock while
    /// suspended, or skip rescaling it (or its pending commands) on a rate
    /// change → diverges.
    #[test]
    fn reprepare_matches_the_reference(seed in any::<u64>(), which in 0usize..5, to in 0usize..6) {
        let desc = random_graph(seed);
        let mut pair = EditorPair::new(&desc, common::prepare(MAX_BLOCK));
        let mut frame = 0;
        let blocks = schedule(which, seed, 400);
        let (first, rest) = blocks.split_at(blocks.len() / 2);
        pair.run(first, &mut frame);
        // Rates and blocks: same rate, new block; new rate, same block;
        // both. Every block stays ≤ the generator's feedback delays.
        let (rate, max) = [
            (48_000.0, 64),
            (48_000.0, 100),
            (96_000.0, MAX_BLOCK),
            (44_100.0, 32),
            (96_000.0, 64),
            (48_000.0, MAX_BLOCK),
        ][to];
        let p = tutti_graph::Prepare::new(tutti_types::SampleRate(rate), Samples(max));
        let ports: Vec<EventIn> = desc
            .kinds
            .keys()
            .flat_map(|&k| (0..shape(&desc.kinds[&k]).event_in).map(move |port| EventIn { node: k, port }))
            .collect();
        let mut rng = Rng::new(seed ^ 0xFA11);
        for _ in 0..rng.below(12) {
            let Some(to) = rng.pick(&ports) else { break };
            let at = tutti_types::At::Frame(tutti_types::Frame(frame + rng.below(300)));
            let kind = tutti_graph::EventKind::Midi(tutti_graph::Ump([rng.below(1000) as u32, 0, 0, 0]));
            pair.schedule(at, to, kind);
        }
        let rest: Vec<usize> = rest
            .iter()
            .flat_map(|&n| {
                // Re-cut the remaining blocks to fit the new maximum.
                (0..n.div_ceil(max)).map(move |i| (n - i * max).min(max))
            })
            .collect();
        let k = (rng.below(3) as usize).min(rest.len());
        let (suspended, rest) = rest.split_at(k);
        pair.reprepare(p, suspended, &mut frame);
        pair.run(rest, &mut frame);
        prop_assert_eq!(pair.exec.late_commands(), pair.reference.late_commands());
    }
}

/// Whether a unit of `b` may crossfade from one of `a`: the same shape in
/// everything but the tail (`Editor::replace`'s rule, spelled out a third
/// time so the generator does not borrow either implementation's).
fn fits(a: &Kind, b: &Kind) -> bool {
    let (a, b) = (shape(a), shape(b));
    (
        a.audio_in,
        a.audio_out,
        a.event_in,
        a.event_out,
        a.latency,
        a.in_place,
    ) == (
        b.audio_in,
        b.audio_out,
        b.event_in,
        b.event_out,
        b.latency,
        b.in_place,
    ) && a.event_resolution == b.event_resolution
}

fn random_fade(rng: &mut Rng) -> tutti_graph::Fade {
    let curve = if rng.chance(50) {
        tutti_graph::CrossfadeCurve::EqualPower
    } else {
        tutti_graph::CrossfadeCurve::EqualAmplitude
    };
    // Mostly shorter than the rounds between edits, often longer, so fades
    // end mid-block, run across edits, and queue behind each other.
    tutti_graph::Fade::new(Samples(1 + rng.below(250) as usize), curve)
}

/// An edit that replaces one to three running nodes with a crossfade — the
/// same kind with fresh state, or another kind that fits — sometimes on top
/// of an ordinary `mutate`, which can remove, rewire or regenerate (without a
/// fade, latency and all) a node that is fading.
fn mutate_with_fades(desc: &Desc, rng: &mut Rng) -> (Desc, BTreeMap<NodeKey, tutti_graph::Fade>) {
    let mut d = if rng.chance(40) {
        mutate(desc, rng)
    } else {
        desc.clone()
    };
    let mut fades = BTreeMap::new();
    for _ in 0..1 + rng.below(3) {
        // Still running as `desc` had it: not removed or regenerated above.
        let running: Vec<NodeKey> = desc
            .order
            .iter()
            .copied()
            .filter(|k| {
                d.kinds.contains_key(k)
                    && d.spec.generation(*k) == desc.spec.generation(*k)
                    && !fades.contains_key(k)
                    // A unit whose declared latency was set by hand cannot
                    // fade: a new unit reports its own (`FadeShape`).
                    && desc.spec.topology.nodes[k].latency
                        == shape(&desc.kinds[k]).latency.samples()
            })
            .collect();
        let recent: Vec<NodeKey> = running
            .iter()
            .copied()
            .filter(|k| desc.recent.contains(k))
            .collect();
        let among = if !recent.is_empty() && rng.chance(40) {
            &recent
        } else {
            &running
        };
        let Some(key) = rng.pick(among) else { break };
        let old = desc.kinds[&key].clone();
        let mut kind = old.clone();
        if rng.chance(50) {
            let other = random_kind(rng);
            if fits(&old, &other) {
                kind = other;
            }
        }
        let gen = d.spec.generation(key) + 1;
        d.spec.generations.insert(key, gen);
        d.spec.topology.nodes.insert(key, spec_for(&kind));
        d.kinds.insert(key, kind);
        fades.insert(key, random_fade(rng));
    }
    // `set_latency` on kept nodes — idle, fading, or with a fade waiting
    // (the proptest's schedule decides which): a new declared latency with
    // no new generation, which cuts a fade there.
    if rng.chance(35) {
        for _ in 0..1 + rng.below(2) {
            let kept: Vec<NodeKey> = desc
                .order
                .iter()
                .copied()
                .filter(|k| {
                    d.kinds.contains_key(k)
                        && d.spec.generation(*k) == desc.spec.generation(*k)
                        && !fades.contains_key(k)
                })
                .collect();
            // Mostly a key crossfaded last round, where a fade runs or
            // waits; now and then any kept key.
            let recent: Vec<NodeKey> = kept
                .iter()
                .copied()
                .filter(|k| desc.recent.contains(k))
                .collect();
            let among = if !recent.is_empty() && rng.chance(70) {
                &recent
            } else {
                &kept
            };
            let Some(key) = rng.pick(among) else { break };
            let node = d.spec.topology.nodes.get_mut(&key).expect("kept");
            node.latency = Samples(rng.below(41) as usize);
        }
    }
    d.recent = fades.keys().copied().collect();
    (d, fades)
}

/// Render until the editor has room for another commit — what a host does
/// on `Backpressure`. (A crossfade holds no commit, so this only waits for
/// the queue itself; `Pair::switch` already applies and collects each.)
fn wait_for_credit(pair: &mut Pair, frame: &mut u64) {
    loop {
        pair.editor.collect();
        if pair.editor.in_flight() < tutti_graph::QUEUE_CAPACITY {
            return;
        }
        run(pair, &[64], frame);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Replaces with crossfades, bit for bit against the reference, whose
    /// fade semantics are its own: eight rounds of edits a random 1–300
    /// frames apart, each crossfading one to three nodes (every kind, events
    /// included) over 1–250 frames, so fades end mid-block, run across other
    /// edits, queue behind a running fade and supersede a waiting one — and
    /// are cut by a removal, a hard regenerate or a `set_latency` (a new
    /// declared latency, no new generation) of their key — under every
    /// block schedule, ragged ones included. Afterwards every crossfade has
    /// come back on the fade-return ring.
    ///
    /// Mutation: in `Executor::apply`, supersede nothing (keep the first
    /// waiting fade) → diverges on the first case where two replaces queue.
    /// The same in `Reference::set_graph_with_fades` → diverges from the
    /// other side. Mutation: in the reference, blend with the fade's position
    /// at the block start for every sample (`gains(done, len)`) → diverges.
    /// Mutation: hand the outgoing unit the node's slot events in the
    /// executor → an event node's fade diverges. Mutation: drop the
    /// latency-change cut from `Reference::set_graph_with_fades` → diverges.
    /// Mutation: skip `Delta::cuts` in `Executor::apply` → diverges.
    #[test]
    fn crossfades_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        let mut desc = random_graph(seed);
        let mut pair = Pair::new(MAX_BLOCK);
        pair.switch(&desc.spec.validate().expect("valid"), &desc.kinds);
        let mut rng = Rng::new(seed ^ 0xFADE);
        let mut frame = 0;
        run(&mut pair, &schedule(which, seed, 100), &mut frame);
        for round in 0..8u64 {
            let (next, fades) = mutate_with_fades(&desc, &mut rng);
            wait_for_credit(&mut pair, &mut frame);
            pair.switch_with_fades(&next.spec.validate().expect("valid"), &next.kinds, &fades);
            let frames = 1 + rng.below(300) as usize;
            run(&mut pair, &schedule(which, seed.wrapping_add(round), frames), &mut frame);
            desc = next;
        }
        // Long enough for every fade, waiting ones included, to end.
        run(&mut pair, &schedule(which, seed, 8 * 260), &mut frame);
        pair.editor.collect();
        prop_assert_eq!(pair.editor.in_flight(), 0);
        prop_assert_eq!(pair.editor.fades_in_flight(), 0, "a crossfade never came back");
    }
}

/// The fade generator is not vacuous: over a fixed range of seeds, replaces
/// land while a fade still runs at their key (so they queue) and while one
/// already waits there (so they supersede it); `mutate` cuts running fades
/// (removing or hard-regenerating their key), and `set_latency` lands on
/// idle keys, on running fades and on queued ones; fades swap in another kind,
/// reach event nodes, and run over in-place kinds (whose outgoing unit must
/// run before the incoming one overwrites the shared slot). Counted by
/// replaying the generator's schedule against the fade rules, the way the
/// proptest drives it (without the credit waits, which only add frames).
///
/// Mutation: make `random_fade` 1–2 frames long → no replace lands on a
/// running fade → fails. Mutation: never swap the kind → fails. Mutation:
/// never call `mutate` in `mutate_with_fades` → no cuts → fails. Mutation:
/// never pick an in-place kind in `mutate_with_fades` → fails. Mutation:
/// drop the `set_latency` edits from `mutate_with_fades` → the three
/// latency counts are zero → fails.
#[test]
fn the_fade_generator_covers_what_the_suite_claims() {
    let (mut queued, mut superseded, mut cut) = (0, 0, 0);
    let (mut other_kind, mut on_events, mut in_place) = (0, 0, 0);
    let (mut latency_on_idle, mut latency_on_fading, mut latency_on_queued) = (0, 0, 0);
    for seed in 0..128u64 {
        let mut desc = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0xFADE);
        let mut frame = 100u64;
        // Per key: when its running fade ends, and a waiting one's length.
        let mut running: BTreeMap<NodeKey, u64> = BTreeMap::new();
        let mut waiting: BTreeMap<NodeKey, u64> = BTreeMap::new();
        for _ in 0..8 {
            let (next, fades) = mutate_with_fades(&desc, &mut rng);
            // Bring each key's fades up to `frame`: an ended one hands over
            // to the one waiting.
            for (k, end) in running.iter_mut() {
                while *end <= frame {
                    match waiting.remove(k) {
                        Some(len) => *end += len,
                        None => break,
                    }
                }
            }
            running.retain(|_, end| *end > frame);
            // A key `mutate` removed or regenerated without a fade.
            let gone: Vec<NodeKey> = running
                .keys()
                .copied()
                .filter(|k| {
                    !fades.contains_key(k)
                        && (!next.kinds.contains_key(k)
                            || next.spec.generation(*k) != desc.spec.generation(*k))
                })
                .collect();
            for k in gone {
                cut += 1;
                running.remove(&k);
                waiting.remove(&k);
            }
            // `set_latency` on a kept key: counted by what it lands on, and
            // it cuts a fade there.
            for (k, now) in &next.spec.topology.nodes {
                let Some(was) = desc.spec.topology.nodes.get(k) else {
                    continue;
                };
                if was.latency == now.latency
                    || next.spec.generation(*k) != desc.spec.generation(*k)
                {
                    continue;
                }
                match (running.remove(k), waiting.remove(k)) {
                    (Some(_), Some(_)) => latency_on_queued += 1,
                    (Some(_), None) => latency_on_fading += 1,
                    _ => latency_on_idle += 1,
                }
            }
            for (&k, f) in &fades {
                let len = f.duration.get() as u64;
                match running.entry(k) {
                    std::collections::btree_map::Entry::Occupied(_) => {
                        queued += 1;
                        if waiting.insert(k, len).is_some() {
                            superseded += 1;
                        }
                    }
                    std::collections::btree_map::Entry::Vacant(v) => {
                        v.insert(frame + len);
                    }
                }
                if next.kinds[&k] != desc.kinds[&k] {
                    other_kind += 1;
                }
                let s = shape(&next.kinds[&k]);
                if s.event_in + s.event_out > 0 {
                    on_events += 1;
                }
                if s.in_place {
                    in_place += 1;
                }
            }
            frame += 1 + rng.below(300);
            desc = next;
        }
    }
    eprintln!(
        "queued {queued}, superseded {superseded}, cut {cut}, other kind {other_kind}, \
         on event nodes {on_events}, in place {in_place}"
    );
    assert!(queued > 50, "only {queued} replaces over a running fade");
    assert!(
        superseded > 10,
        "only {superseded} supersedes of a waiting fade"
    );
    assert!(cut > 10, "only {cut} fades cut by `mutate`");
    assert!(other_kind > 25, "only {other_kind} kind swaps");
    assert!(on_events > 50, "only {on_events} fades on event nodes");
    assert!(in_place > 50, "only {in_place} fades on in-place kinds");
    eprintln!(
        "set_latency on idle {latency_on_idle}, fading {latency_on_fading}, \
         queued {latency_on_queued}"
    );
    assert!(
        latency_on_idle > 25,
        "only {latency_on_idle} latency changes on idle keys"
    );
    assert!(
        latency_on_fading > 10,
        "only {latency_on_fading} latency changes cutting a running fade"
    );
    assert!(
        latency_on_queued > 5,
        "only {latency_on_queued} latency changes cutting a queued fade"
    );
}
