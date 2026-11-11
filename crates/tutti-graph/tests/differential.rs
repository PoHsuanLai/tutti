//! The differential suite: the serial plan executor against the reference
//! interpreter, bit for bit, on random graphs.
//!
//! The graphs mix every test node in `common`: generators (`Const` returning
//! `Status::Constant`, `EnvProbe` reading `Env` and `Cx::arrival`), in-place
//! gains, sums, one-sample memory, latency-declaring delays (so PDC inserts
//! rings), bypass nodes, event emitters, event-latency nodes and
//! order-sensitive event consumers with fan-in — and feedback edges of both
//! kinds. Block sizes include 1, non-powers of two, and a schedule that varies
//! every block.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{Kind, Pair, bits, input_signal};
use proptest::prelude::*;
use tutti_graph::Topology;
use tutti_graph::{Bus, BusRef, BusRole, BusSource, Buses, NodeKey, TapAlign, TapId, TapSpec};
use tutti_graph::{Edge, EdgeGain, EdgeId, FeedbackFrom, InPort, NodeSpec, OutPort, Sink, Source};
use tutti_graph::{EventEdge, EventIn, EventOut, GraphSpec, Unforkable};
use tutti_types::{Amplitude, BusTopology, ChannelLayout, ChannelTopology, Samples};

use tutti_graph::Node;

const MAX_BLOCK: usize = 128;

/// xorshift64* — the graph is a pure function of the seed, so a failing
/// proptest case replays exactly.
#[derive(Clone)]
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
    /// Its own stream for the audio fan-in choices, so adding fan-in left
    /// the seeds' other choices (and the coverage the generators are held
    /// to) where they were.
    fan: Rng,
}

fn random_kind(rng: &mut Rng) -> Kind {
    // Latency-bearing nodes and multi-input sinks are weighted up: PDC only
    // happens where paths of different latency meet.
    match rng.below(17) {
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
        16 => burst_kind(rng),
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

/// An emitter declaring its event capacity, often below its burst, so the
/// writer refuses events every burst. Its period is at least `MAX_BLOCK`, so
/// it keeps its declared rate (see `Kind::Burst`).
fn burst_kind(rng: &mut Rng) -> Kind {
    Kind::Burst {
        period: MAX_BLOCK as u64 + rng.below(200),
        phase: rng.below(MAX_BLOCK as u64),
        burst: 1 + rng.below(6) as u32,
        cap: 1 + rng.below(4) as u32,
    }
}

fn shape(kind: &Kind) -> tutti_graph::Shape {
    common::TestNode::new(kind.clone()).shape()
}

fn audio_outs(desc: &Desc, among: &[NodeKey]) -> Vec<OutPort> {
    among
        .iter()
        .flat_map(|&k| {
            (0..shape(&desc.kinds[&k]).audio_out().count())
                .map(move |port| OutPort { node: k, port })
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
    for port in 0..s.audio_in().count() {
        let at = InPort { node: key, port };
        let roll = rng.below(100);
        let edge = if roll < 64 {
            rng.pick(&direct).map(|p| Edge::Direct(Source::Node(p)))
        } else if roll < 76 {
            let width = u64::from(desc.spec.topology.inputs.count());
            Some(Edge::Direct(Source::Global(rng.below(width) as u16)))
        } else if roll < 80 {
            // Silence is no edge.
            None
        } else if roll < 90 {
            // Feedback delays of at least a block, sometimes longer.
            let delay = Samples(MAX_BLOCK + 17 * rng.below(3) as usize);
            rng.pick(&any)
                .map(|from| Edge::Feedback(FeedbackFrom::new(from, delay)))
        } else {
            None
        };
        // Audio fan-in (doc 013, "Audio fan-in sums"): a port now and then
        // takes several distinct sources (direct, global or feedback), which
        // the compiled op sums in `Edge` order, silent addends skipped, and
        // the reference folds from `+0.0`. A hub is wider than `MAX_PORTS`
        // when enough earlier outputs exist (a chain of sum ops).
        let mut fan = std::mem::replace(&mut desc.fan, Rng(1));
        let mut set: Vec<Edge> = edge.into_iter().collect();
        if fan.chance(35) {
            let hub = fan.chance(15);
            let tries = if hub {
                direct.len() as u64 + 3
            } else {
                1 + fan.below(3)
            };
            for _ in 0..tries {
                let extra = match fan.below(10) {
                    0 | 1 => {
                        let width = u64::from(desc.spec.topology.inputs.count());
                        Some(Edge::Direct(Source::Global(fan.below(width) as u16)))
                    }
                    2 => {
                        let delay = Samples(MAX_BLOCK + 17 * fan.below(3) as usize);
                        fan.pick(&any)
                            .map(|from| Edge::Feedback(FeedbackFrom::new(from, delay)))
                    }
                    _ => fan.pick(&direct).map(|p| Edge::Direct(Source::Node(p))),
                };
                if let Some(e) = extra
                    && !set.contains(&e)
                {
                    set.push(e);
                }
            }
        }
        desc.fan = fan;
        set.sort();
        if set.is_empty() {
            desc.spec.topology.edges.remove(&at);
        } else {
            desc.spec.topology.edges.insert(at, set);
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
            if let Some(e) = e
                && seen.insert(e.from())
            {
                sources.push(e);
            }
        }
        desc.spec.events.insert(at, sources);
    }
}

fn spec_for(kind: &Kind) -> NodeSpec {
    let s = shape(kind);
    NodeSpec::new("test", s.audio_in(), s.audio_out())
        .with_latency(s.latency.samples())
        .with_tail(s.tail)
}

fn wire_outputs(desc: &mut Desc, rng: &mut Rng) {
    let all = desc.order.clone();
    let outs = audio_outs(desc, &all);
    let width = u64::from(desc.spec.topology.inputs.count());
    desc.spec
        .topology
        .set_outputs((0..1 + rng.below(4)).map(|_| match rng.below(10) {
            0 => Source::Zero,
            1 => Source::Global(rng.below(width) as u16),
            _ => rng.pick(&outs).map_or(Source::Zero, Source::Node),
        }));
    // A channel now and then sums several sources, aligned to the output time
    // each on its own.
    let mut fan = std::mem::replace(&mut desc.fan, Rng(1));
    for ch in 0..desc.spec.topology.outputs.len() {
        if desc.spec.topology.outputs[ch].is_empty() || !fan.chance(35) {
            continue;
        }
        for _ in 0..1 + fan.below(3) {
            let extra = match fan.below(8) {
                0 => Source::Global(fan.below(width) as u16),
                _ => fan.pick(&outs).map_or(Source::Zero, Source::Node),
            };
            let set = &mut desc.spec.topology.outputs[ch];
            if extra != Source::Zero && !set.contains(&extra) {
                set.push(extra);
            }
        }
        desc.spec.topology.outputs[ch].sort();
    }
    desc.fan = fan;
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
        fan: Rng::new(seed ^ 0xFA41_1A5E),
        spec: GraphSpec::new(Topology {
            inputs: ChannelLayout::from_count(1 + u16::try_from(rng.below(3)).expect("below 3")),
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
            match rng.below(5) {
                4 => burst_kind(&mut rng),
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
        t.edges.retain(|at, es| {
            es.retain(|e| {
                let from = match *e {
                    Edge::Direct(Source::Node(p))
                    | Edge::Feedback(FeedbackFrom { from: p, .. }) => Some(p.node),
                    Edge::Direct(_) => None,
                };
                from != Some(victim)
            });
            at.node != victim && !es.is_empty()
        });
        for s in &mut t.outputs {
            s.retain(|src| !matches!(src, Source::Node(p) if p.node == victim));
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
        let generation = d.spec.generation(key) + 1;
        d.spec.generations.insert(key, generation);
        if rng.chance(50) {
            let old = shape(&d.kinds[&key]);
            let kind = random_kind(rng);
            let new = shape(&kind);
            // Keep the port shape so edges stay valid; only swap when it
            // matches (a latency change, typically).
            if (old.audio_in(), old.audio_out(), old.event_in, old.event_out)
                == (new.audio_in(), new.audio_out(), new.event_in, new.event_out)
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
            tutti_types::Bpm::of(self.tempo),
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

/// The executors each case runs: serial, and parallel on a participant
/// count the seed picks from `common::WORKERS` — so every generated graph is
/// held to the reference on both, and the parallel render is held to the
/// serial one through it, sample for sample (doc 013, "The parallel executor").
fn modes(seed: u64) -> [usize; 2] {
    [1, common::workers_for(seed)]
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
        for workers in modes(seed) {
            let desc = random_graph(seed);
            let valid = desc.spec.validate().expect("generated graphs are valid");
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&valid, &desc.kinds);
            let mut frame = 0;
            run(&mut pair, &schedule(which, seed, 700), &mut frame);
        }
    }

    /// A tap changes nothing and reads exactly what it names: with an aligned
    /// tap of every output's source and an outputs tap open, the executor
    /// still renders the reference's graph bit for bit, and the lanes of each
    /// are the output channels, sample for sample, at every block schedule,
    /// serial and parallel. The outputs tap reads **every** channel, summed
    /// ones included (post-sum and aligned); the ports tap names the source of
    /// a one-source channel only (a tap of ports names no sum), so it is
    /// compared on those. The reference knows no taps; this holds the tap ops
    /// to the output they must copy, on random graphs with feedback, PDC,
    /// fan-in and events.
    #[test]
    fn an_aligned_tap_of_the_outputs_is_the_output(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let desc = random_graph(seed);
            let outputs: Vec<Source> = desc
                .spec
                .topology
                .outputs
                .iter()
                .map(|s| match s[..] {
                    [one] => one,
                    // A ports tap names no sum: its lane reads silence and is
                    // not compared.
                    _ => Source::Zero,
                })
                .collect();
            let summed: Vec<bool> = desc.spec.topology.outputs.iter().map(|s| s.len() > 1).collect();
            let channels = outputs.len();
            let mut spec = desc.spec.clone();
            spec.topology.taps.insert(TapId(1), TapSpec::aligned(outputs.clone()));
            spec.topology.taps.insert(TapId(2), TapSpec::Outputs { width: channels });
            let valid = spec.validate().expect("generated graphs are valid");
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            // The harness compiles the spec itself; the readers' lanes ride the
            // same commit.
            // `open_tap` refuses a source the editor's spec lacks, and the
            // harness compiles `spec` itself: give the editor the same nodes.
            pair.editor.spec_mut().topology.nodes = spec.topology.nodes.clone();
            pair.editor.spec_mut().topology.inputs = spec.topology.inputs;
            let mut tap = pair.editor.open_tap(TapId(1), outputs, TapAlign::ToOutput, Samples(1 << 16)).expect("the sources exist");
            let mut heard = pair.editor.open_output_tap(TapId(2), channels, Samples(1 << 16)).expect("the sources exist");
            pair.switch(&valid, &desc.kinds);
            let width = tap.width();
            prop_assert_eq!(heard.width(), channels);
            let mut frame = 0;
            for n in schedule(which, seed, 700) {
                let input = input_signal(frame, n);
                let (exec, reference) = pair.block(n, &input);
                prop_assert_eq!(bits(&exec), bits(&reference), "a tap changed the render at frame {}", frame);
                let mut got = vec![0.0f32; n * width];
                prop_assert_eq!(tap.pop_into(&mut got).get(), n);
                for (c, channel) in exec.iter().enumerate().filter(|(c, _)| !summed[*c]) {
                    let lane: Vec<f32> = got.iter().skip(c).step_by(width).copied().collect();
                    prop_assert_eq!(bits(&[lane]), bits(std::slice::from_ref(channel)), "tap lane {} at frame {}", c, frame);
                }
                let mut got = vec![0.0f32; n * channels];
                prop_assert_eq!(heard.pop_into(&mut got).get(), n);
                for (c, channel) in exec.iter().enumerate() {
                    let lane: Vec<f32> = got.iter().skip(c).step_by(channels).copied().collect();
                    prop_assert_eq!(bits(&[lane]), bits(std::slice::from_ref(channel)), "outputs tap lane {} at frame {}", c, frame);
                }
                prop_assert_eq!(tap.take_lost().get(), 0);
                prop_assert_eq!(heard.take_lost().get(), 0);
                frame += n as u64;
            }
        }
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
    #[test]
    fn scheduled_commands_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let desc = random_graph(seed);
            let valid = desc.spec.validate().expect("generated graphs are valid");
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
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
                        let kind = tutti_graph::EventKind::Midi(tutti_ump::Ump([rng.below(1000) as u32, 0, 0, 0]));
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
                // Only writers past a declared capacity refuse, alike in both.
                prop_assert_eq!(pair.exec.dropped_event_count(), pair.reference.dropped_event_count());
                prop_assert_eq!(pair.exec.late_command_count(), pair.reference.late_command_count(), "late, frame {}", frame);
                frame += n as u64;
            }
            prop_assert_eq!(pair.exec.unrouted_command_count(), pair.reference.unrouted_command_count());
        }
    }

    /// Scheduled commands and `Env`, with the transport changing **inside**
    /// blocks (the engine's timestamped transport commands):
    /// both interpreters see the same changes, resolve a beat against the
    /// transport in force where playback reaches it (a beat just after a
    /// mid-block start or seek lands in that block), and agree on late.
    #[test]
    fn transport_changes_inside_blocks_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        transport_case(seed, which)?;
    }

    /// The same across a recompile: nodes, rings and feedback slots that
    /// survive by key carry their state, in both interpreters, identically.
    #[test]
    fn recompiles_preserve_state_identically(seed in any::<u64>(), which in 0usize..5) {
        recompile_case(seed, which);
    }
}

/// [`transport_changes_inside_blocks_are_bit_identical`]'s body, for one
/// `(seed, which)`.
fn transport_case(seed: u64, which: usize) -> Result<(), TestCaseError> {
    for workers in modes(seed) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("generated graphs are valid");
        let mut pair = Pair::with_workers(MAX_BLOCK, workers);
        pair.switch(&valid, &desc.kinds);
        let ports: Vec<EventIn> = desc
            .kinds
            .keys()
            .flat_map(|&k| {
                (0..shape(&desc.kinds[&k]).event_in).map(move |port| EventIn { node: k, port })
            })
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
                        1 => tutti_types::At::Frame(tutti_types::Frame(
                            frame + rng.below(2 * n as u64),
                        )),
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
                    let kind = tutti_graph::EventKind::Midi(tutti_ump::Ump([
                        rng.below(1000) as u32,
                        0,
                        0,
                        0,
                    ]));
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
            prop_assert_eq!(
                pair.exec.late_command_count(),
                pair.reference.late_command_count(),
                "late, frame {}",
                frame
            );
            frame += n as u64;
        }
        prop_assert_eq!(
            pair.exec.unrouted_command_count(),
            pair.reference.unrouted_command_count()
        );
    }
    Ok(())
}

/// [`recompiles_preserve_state_identically`]'s body, for one `(seed, which)`.
fn recompile_case(seed: u64, which: usize) {
    let first = random_graph(seed);
    let mut rng = Rng::new(seed ^ 0x5EED);
    let second = mutate(&first, &mut rng);
    let third = mutate(&second, &mut rng);

    for workers in modes(seed) {
        let mut pair = Pair::with_workers(MAX_BLOCK, workers);
        let mut frame = 0;
        for (i, desc) in [&first, &second, &third].into_iter().enumerate() {
            let valid = desc.spec.validate().expect("mutations stay valid");
            pair.switch(&valid, &desc.kinds);
            run(
                &mut pair,
                &schedule(which, seed.wrapping_add(i as u64), 300),
                &mut frame,
            );
        }
    }
}

/// A case `recompiles_preserve_state_identically` found on main (seed
/// 3280887136571273968, which = 3), kept as a plain test: the regressions
/// file persists proptest's RNG seeds, not generated values, so a case found
/// by another run of the generator cannot be written into it by hand.
/// A seek back by under a frame continues the run (the slack for a host's
/// `f32` beat jitter), so a beat the run passed, which the playhead is again
/// just short of, is ahead of it and not late. Here playback stops at beat
/// 0.84953, the transport seeks to 0.849 (0.0005 beat, 0.7 of a frame back),
/// then a one-frame block starts there with a command at beat 0.8493 (0.4 of
/// a frame ahead, so it lands in the next block). The executor counts it
/// pending; the reference once counted it late because its history still
/// held the interval past the playhead.
#[test]
fn transport_regression_11926813388736633892() {
    transport_case(11_926_813_388_736_633_892, 0).expect("agrees");
}

#[test]
fn recompile_regression_3280887136571273968() {
    recompile_case(3_280_887_136_571_273_968, 3);
}

/// The generator is not vacuous: over a fixed range of seeds it produces the
/// features the suite claims to cover.
#[test]
fn the_generator_covers_what_the_suite_claims() {
    let (mut delays, mut event_delays, mut feedback, mut event_fb, mut merges, mut in_place) =
        (0, 0, 0, 0, 0, 0);
    let (mut wide, mut global_delays, mut bypass_in_place, mut multi_io) = (0, 0, 0, 0);
    let mut overflowing_wired = 0;
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
            // A port past its declared capacity every burst, whose events
            // someone reads: the writer's refusals are then visible in both
            // interpreters' outputs and drop counts.
            if matches!(kind, Kind::Burst { burst, cap, .. } if burst > cap)
                && desc
                    .spec
                    .events
                    .values()
                    .flatten()
                    .any(|e| e.from().node == *k)
            {
                overflowing_wired += 1;
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
        (
            "wired ports past their declared capacity",
            overflowing_wired,
        ),
    ];
    for (what, n) in all {
        eprintln!("{what}: {n}");
    }
    // The last four are rarer by construction (each needs
    // two features to meet); ten over 512 graphs still exercises each.
    for (i, (what, n)) in all.iter().enumerate() {
        let min = if i < 6 { 25 } else { 10 };
        assert!(*n > min, "only {n} {what} over 512 graphs");
    }
}

/// Declared costs for `desc`'s nodes, a pure function of the seed: about
/// two in three declare `Cost::Light`, a few `Cost::Moderate`, the rest
/// nothing (`Cost::Heavy`) — so cheap siblings pack (doc 013, "The cost
/// model and coarsening") beside heavy tasks that keep the plan parallel.
fn random_costs(desc: &Desc, seed: u64) -> BTreeMap<NodeKey, tutti_graph::Cost> {
    let mut rng = Rng::new(seed ^ 0xC057);
    desc.kinds
        .keys()
        .filter_map(|&k| match rng.below(6) {
            0..=3 => Some((k, tutti_graph::Cost::Light)),
            4 => Some((k, tutti_graph::Cost::Moderate)),
            _ => None,
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// With random declared costs the compiler packs cheap tasks together
    /// (and `Plan::schedule` picks serial or parallel per plan, and per
    /// block length); the executor, serial and on a pool, still renders
    /// every graph exactly as the reference does. `compile` runs the
    /// verifier on every packed plan (debug), and `Pair::switch` runs it
    /// again.
    #[test]
    fn coarsened_plans_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let desc = random_graph(seed);
            let valid = desc.spec.validate().expect("generated graphs are valid");
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.costs = random_costs(&desc, seed);
            pair.switch(&valid, &desc.kinds);
            let mut frame = 0;
            run(&mut pair, &schedule(which, seed, 700), &mut frame);
        }
    }
}

/// The generator reaches what the fan-in op is held to: across the seeds,
/// plans with a sum on an input port, a sum on an output channel and a sum
/// with a feedback addend, and a sum op in a plan exactly where a sink has
/// several sources. (A chain past `MAX_PORTS` needs more nodes than a generated
/// graph has; `tests/fan_in.rs` renders it against the reference and the
/// parallel executor.)
#[test]
fn the_fan_in_generator_covers_the_sum_op() {
    let (mut ports, mut outputs, mut feedback) = (0, 0, 0);
    for seed in 0..600u64 {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) = tutti_graph::compile(
            &valid,
            &common::shapes_of(&desc.kinds),
            &common::prepare(MAX_BLOCK),
            None,
        )
        .expect("compiles");
        let t = &desc.spec.topology;
        ports += t.edges.values().filter(|es| es.len() > 1).count().min(1);
        outputs += t.outputs.iter().filter(|s| s.len() > 1).count().min(1);
        feedback += t
            .edges
            .values()
            .filter(|es| es.len() > 1 && es.iter().any(|e| matches!(e, Edge::Feedback(_))))
            .count()
            .min(1);
        let has_sum = plan
            .ops()
            .iter()
            .any(|o| matches!(o, tutti_graph::Op::Sum { .. }));
        assert_eq!(
            has_sum,
            t.edges.values().any(|es| es.len() > 1) || t.outputs.iter().any(|s| s.len() > 1),
            "seed {seed}: a sum op exactly where a sink has several sources"
        );
    }
    assert!(ports > 60, "only {ports} graphs sum at a port");
    assert!(outputs > 30, "only {outputs} graphs sum at an output");
    assert!(
        feedback > 10,
        "only {feedback} graphs sum a feedback addend"
    );
}

/// `coarsened_plans_are_bit_identical` exercises what it claims: over 512
/// seeds, declared costs pack plans further than the same graphs
/// undeclared, and many of those packed plans still spread on a pool of
/// four (so packed tasks run in parallel), while others go serial.
#[test]
fn the_cost_generator_packs_and_spreads() {
    let (mut packed, mut packed_parallel, mut serial) = (0, 0, 0);
    // Undeclared graphs, for the record (doc 013, the cost model's
    // decision 2):
    // multi-task plans the model keeps serial at four participants.
    let (mut plain_multi, mut plain_serial) = (0, 0);
    for seed in 0..512u64 {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let compile = |costs: &BTreeMap<NodeKey, tutti_graph::Cost>| {
            tutti_graph::compile(
                &valid,
                &common::shapes_costed(&desc.kinds, costs),
                &common::prepare(MAX_BLOCK),
                None,
            )
            .expect("compiles")
            .0
        };
        let plain = compile(&BTreeMap::new());
        if plain.tasks().len() > 1 {
            plain_multi += 1;
            if plain.schedule(4) == tutti_graph::Schedule::Serial {
                plain_serial += 1;
            }
        }
        let costed = compile(&random_costs(&desc, seed));
        if costed.tasks().len() < plain.tasks().len() {
            packed += 1;
            if matches!(costed.schedule(4), tutti_graph::Schedule::Parallel { .. }) {
                packed_parallel += 1;
            }
        }
        if plain.tasks().len() > 1 && costed.schedule(4) == tutti_graph::Schedule::Serial {
            serial += 1;
        }
    }
    eprintln!("packed further: {packed}, of them parallel: {packed_parallel}, serial: {serial}");
    eprintln!("undeclared multi-task plans: {plain_multi}, serial at 4: {plain_serial}");
    assert!(packed > 50, "only {packed} plans packed further");
    assert!(
        packed_parallel > 25,
        "only {packed_parallel} packed plans spread"
    );
    assert!(serial > 25, "only {serial} multi-task plans went serial");
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
                for (at, e) in d
                    .spec
                    .topology
                    .edges
                    .iter()
                    .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
                {
                    if let Edge::Direct(Source::Node(p)) = e
                        && p.node == k
                    {
                        stack.push(at.node);
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
        if se.audio_in().count() > 0 && sl.audio_out().count() > 0 {
            desc.spec.topology.edges.insert(
                InPort {
                    node: early,
                    port: 0,
                },
                vec![Edge::Direct(Source::Node(OutPort {
                    node: late,
                    port: 0,
                }))],
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
    #[test]
    fn unbroken_cycles_are_rejected(seed in any::<u64>()) {
        let mut desc = random_graph(seed);
        let mut rng = Rng::new(seed ^ 0xC1C1E);
        prop_assume!(close_a_cycle(&mut desc, &mut rng));
        let rejected = match desc.spec.validate() {
            Err(errs) => errs.iter().any(|e| matches!(
                e,
                tutti_graph::GraphInvalid::Topology(tutti_graph::Invalid::Cycle { .. })
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
    #[test]
    fn colouring_is_safe_under_any_schedule(seed in any::<u64>()) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("valid");
        let (plan, _) = tutti_graph::compile(&valid, &common::shapes_of(&desc.kinds), &common::prepare(MAX_BLOCK), None)
            .expect("compiles");
        plan.verify().expect("the verifier accepts it");
        brute_force_interference(&plan);
    }
}

/// State continuity, stated directly: recompiling to the same graph plus an
/// unrelated node renders exactly what never recompiling renders. The PDC
/// ring, the feedback slot, and the `Smooth`/`Lag` unit state all have to
/// carry for this to hold.
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
    t.edges.insert(InPort { node: b, port: 0 }, vec![src(a)]);
    t.edges.insert(InPort { node: c, port: 0 }, vec![src(a)]);
    t.edges.insert(InPort { node: mix, port: 0 }, vec![src(b)]);
    t.edges.insert(InPort { node: mix, port: 1 }, vec![src(c)]);
    t.edges.insert(
        InPort { node: mix, port: 2 },
        vec![Edge::Feedback(FeedbackFrom::one_block(
            OutPort { node: c, port: 0 },
            Samples(MAX_BLOCK),
        ))],
    );
    t.set_outputs(vec![Source::Node(OutPort { node: mix, port: 0 })]);
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
        t.edges
            .insert(InPort { node, port }, vec![Edge::Direct(from)]);
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
        t.edges
            .insert(InPort { node, port }, vec![Edge::Direct(from)]);
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
    t.set_outputs(vec![
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
    ]);
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
    fn new(desc: &Desc, prepare: tutti_graph::Prepare, workers: usize) -> Self {
        let (mut editor, mut exec) =
            tutti_graph::Editor::with_event_capacity(prepare, common::EVENT_CAPACITY);
        if workers > 1 {
            exec.set_pool(Some(common::test_pool(workers)));
        }
        for (&k, kind) in &desc.kinds {
            editor.insert(k, "test", Unforkable(common::TestNode::new(kind.clone())));
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
            tutti_types::Bpm::of(120.0),
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
            // Only writers past a declared capacity (`Kind::Burst`) refuse,
            // alike in both; see `Pair::block_with_changes`.
            assert_eq!(
                self.exec.dropped_event_count(),
                self.reference.dropped_event_count(),
                "the executor dropped events the reference did not"
            );
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
    #[test]
    fn reprepare_matches_the_reference(seed in any::<u64>(), which in 0usize..5, to in 0usize..6) {
        for workers in modes(seed) {
            let desc = random_graph(seed);
            let mut pair = EditorPair::new(&desc, common::prepare(MAX_BLOCK), workers);
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
                let kind = tutti_graph::EventKind::Midi(tutti_ump::Ump([rng.below(1000) as u32, 0, 0, 0]));
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
            prop_assert_eq!(pair.exec.late_command_count(), pair.reference.late_command_count());
        }
    }
}

/// Whether a unit of `b` may crossfade from one of `a`: the same shape in
/// everything but the tail (`Editor::replace`'s rule, spelled out a third
/// time so the generator does not borrow either implementation's).
fn fits(a: &Kind, b: &Kind) -> bool {
    let (a, b) = (shape(a), shape(b));
    (
        a.audio_in(),
        a.audio_out(),
        a.event_in,
        a.event_out,
        a.latency,
        a.in_place,
    ) == (
        b.audio_in(),
        b.audio_out(),
        b.event_in,
        b.event_out,
        b.latency,
        b.in_place,
    ) && a.event_resolution == b.event_resolution
        && a.event_capacity == b.event_capacity
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
        let generation = d.spec.generation(key) + 1;
        d.spec.generations.insert(key, generation);
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
    #[test]
    fn crossfades_are_bit_identical(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let mut desc = random_graph(seed);
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
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

// ---------------------------------------------------------------------------
// Edge gain (doc 013, "Edge gain"): weighted lanes on the generated graphs.
// ---------------------------------------------------------------------------

/// The weights the generator picks from: unity (the plain add), zero (never
/// skipped by value), negative (polarity), and fractions that do not round
/// exactly.
const WEIGHTS: [f32; 9] = [1.0, 0.5, -0.75, 0.0, 1.5, 0.3, 0.1, -1.0, 2.0];

/// Whether `id` names an edge of `t`.
fn has_edge(t: &Topology, id: EdgeId) -> bool {
    match id.sink {
        Sink::Port(at) => t.edges.get(&at).is_some_and(|e| e.contains(&id.from)),
        Sink::Output(ch) => match id.from {
            Edge::Direct(s) => t
                .outputs
                .get(usize::from(ch))
                .is_some_and(|v| v.contains(&s)),
            Edge::Feedback(_) => false,
        },
    }
}

/// Drops the entries of edges `mutate` removed or rewired.
fn prune_gains(desc: &mut Desc) {
    let t = &desc.spec.topology;
    desc.spec.gains.retain(|id, _| has_edge(t, *id));
}

/// Gives about a third of the edges without an entry one, from `rng`: a
/// stream of its own, so the graphs the other properties draw are untouched.
fn weigh(desc: &mut Desc, rng: &mut Rng) {
    let t = &desc.spec.topology;
    let mut ids: Vec<EdgeId> = t
        .edges
        .iter()
        .flat_map(|(&at, es)| {
            es.iter().map(move |&from| EdgeId {
                sink: Sink::Port(at),
                from,
            })
        })
        .collect();
    for (ch, sources) in t.outputs.iter().enumerate() {
        ids.extend(sources.iter().map(|&s| EdgeId {
            sink: Sink::Output(ch as u16),
            from: Edge::Direct(s),
        }));
    }
    for id in ids {
        if rng.chance(33) && !desc.spec.gains.contains_key(&id) {
            let w = WEIGHTS[rng.below(WEIGHTS.len() as u64) as usize];
            desc.spec
                .set_edge_gain(id, EdgeGain::new(Amplitude::new(w)));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Weighted lanes on the random graphs, serial and parallel, are the
    /// reference's bit for bit: with weights on a third of the edges (feedback
    /// ones, fan-ins, one-edge sinks, output channels, and chains behind PDC
    /// included), a fader moved between blocks (`set_edge_gain`, so ramps of
    /// every block length in the schedule), and an edit halfway that rewires,
    /// removes and regenerates nodes (lane state carried by edge, entries of
    /// vanished edges dropped, new ones added). The parallel render is held to
    /// the serial one through the reference.
    #[test]
    fn weighted_graphs_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let mut gains = Rng::new(seed ^ 0x6A14_ED6E);
            let mut rng = Rng::new(seed ^ 0xED17);
            let mut desc = random_graph(seed);
            weigh(&mut desc, &mut gains);
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&desc.spec.validate().expect("generated graphs are valid"), &desc.kinds);
            let blocks = schedule(which, seed, 900);
            let half = blocks.len() / 2;
            let mut frame = 0u64;
            for (i, &n) in blocks.iter().enumerate() {
                if i == half {
                    let mut edited = mutate(&desc, &mut rng);
                    prune_gains(&mut edited);
                    weigh(&mut edited, &mut gains);
                    pair.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
                    desc = edited;
                }
                if gains.chance(40)
                    && let Some(id) = gains.pick(&desc.spec.gains.keys().copied().collect::<Vec<_>>())
                {
                    let w = Amplitude::new(WEIGHTS[gains.below(WEIGHTS.len() as u64) as usize]);
                    pair.set_edge_gain(id, w);
                    desc.spec.gains.get_mut(&id).expect("picked").base = w;
                }
                let input = input_signal(frame, n);
                let (a, b) = pair.block(n, &input);
                prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {} ({} frames)", frame, n);
                frame += n as u64;
            }
        }
    }
}

/// The weighted property is not vacuous: over the seeds a good share of the
/// graphs carry entries, and the plans hold weighted sums of one addend, of
/// several, and in chains' company, so the edge cases are drawn.
#[test]
fn the_generator_draws_weighted_lanes() {
    let (mut with_gains, mut one_addend, mut many, mut negative, mut zero) = (0, 0, 0, 0, 0);
    for seed in 0..300u64 {
        let mut gains = Rng::new(seed ^ 0x6A14_ED6E);
        let mut desc = random_graph(seed);
        weigh(&mut desc, &mut gains);
        if desc.spec.gains.is_empty() {
            continue;
        }
        with_gains += 1;
        negative += usize::from(desc.spec.gains.values().any(|g| g.base.get() < 0.0));
        zero += usize::from(desc.spec.gains.values().any(|g| g.base.get() == 0.0));
        let valid = desc.spec.validate().expect("valid");
        let shapes: BTreeMap<NodeKey, tutti_graph::Shape> = desc
            .kinds
            .iter()
            .map(|(&k, kind)| (k, shape(kind)))
            .collect();
        let (plan, _) = tutti_graph::compile(&valid, &shapes, &common::prepare(MAX_BLOCK), None)
            .expect("compiles");
        for op in plan.ops() {
            if let tutti_graph::Op::WeightedSum { srcs, .. } = op {
                if srcs.len == 1 {
                    one_addend += 1;
                } else {
                    many += 1;
                }
            }
        }
    }
    eprintln!(
        "graphs {with_gains}, one-addend {one_addend}, many {many}, neg {negative}, zero {zero}"
    );
    assert!(with_gains > 200, "only {with_gains} graphs with entries");
    assert!(
        one_addend > 100,
        "only {one_addend} one-addend weighted sums"
    );
    assert!(many > 100, "only {many} weighted sums of several");
    assert!(
        negative > 100 && zero > 40,
        "{negative} negative, {zero} zero"
    );
}

// ---------------------------------------------------------------------------
// Driven edge gains (doc 013, "Edge gain", G2): lanes behind the entries.
// ---------------------------------------------------------------------------

/// The first key of the lane nodes: `fresh_key` draws below 1000, and
/// `mutate` touches only the nodes of `order`, so a lane node is never
/// wired, removed or regenerated by it.
const LANE_KEYS: u64 = 1_000_000;

/// The latencies the lane nodes declare: none, a few frames, and past a
/// block (an early lane gets an event ring, a late one makes the sink wait).
const LANE_LATENCIES: [usize; 4] = [0, 16, 40, 150];

/// The ranges a driven lane is clamped to.
const LANE_RANGES: [(f32, f32); 4] = [(-8.0, 8.0), (-1.0, 1.0), (0.0, 0.5), (0.5, -0.25)];

/// Adds lane nodes (once) and, for about half the entries, a source among
/// them (or none: a source that goes, declicked) and a range, from `rng`: a
/// stream of its own, so no other property's graphs move.
fn drive(desc: &mut Desc, rng: &mut Rng) {
    if !desc.kinds.contains_key(&NodeKey(LANE_KEYS)) {
        for (i, &latency) in LANE_LATENCIES.iter().enumerate() {
            let key = NodeKey(LANE_KEYS + i as u64);
            let kind = Kind::GainDriver {
                period: 8 + 9 * i as u64,
                latency,
            };
            desc.spec.topology.nodes.insert(key, spec_for(&kind));
            desc.kinds.insert(key, kind);
            desc.used.insert(key);
        }
    }
    let ids: Vec<EdgeId> = desc.spec.gains.keys().copied().collect();
    for id in ids {
        if !rng.chance(50) {
            continue;
        }
        let g = desc.spec.gains.get_mut(&id).expect("listed");
        g.source = rng.chance(80).then(|| EventOut {
            node: NodeKey(LANE_KEYS + rng.below(LANE_LATENCIES.len() as u64)),
            port: 0,
        });
        let (min, max) = LANE_RANGES[rng.below(LANE_RANGES.len() as u64) as usize];
        g.range = tutti_graph::ParamRange::new(min, max);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Driven lanes on the random graphs, serial and parallel, are the
    /// reference's bit for bit: lanes of several periods and latencies drive
    /// about half the weighted edges (at ports and output channels, behind
    /// PDC, in fan-ins and one-edge sinks), with ranges that clamp, a range
    /// moved between blocks (`set_edge_gain_range`), a fader moved under a
    /// driven lane, and an edit halfway that rewires, removes and
    /// regenerates nodes and re-draws every source (a source that changes
    /// or goes is declicked, state carried by edge).
    #[test]
    fn driven_gains_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let mut gains = Rng::new(seed ^ 0x6A14_ED6E);
            let mut lanes = Rng::new(seed ^ 0xD21_7E4);
            let mut rng = Rng::new(seed ^ 0xED17);
            let mut desc = random_graph(seed);
            weigh(&mut desc, &mut gains);
            drive(&mut desc, &mut lanes);
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&desc.spec.validate().expect("generated graphs are valid"), &desc.kinds);
            let blocks = schedule(which, seed, 900);
            let half = blocks.len() / 2;
            let mut frame = 0u64;
            for (i, &n) in blocks.iter().enumerate() {
                if i == half {
                    let mut edited = mutate(&desc, &mut rng);
                    prune_gains(&mut edited);
                    weigh(&mut edited, &mut gains);
                    drive(&mut edited, &mut lanes);
                    pair.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
                    desc = edited;
                }
                let ids: Vec<EdgeId> = desc.spec.gains.keys().copied().collect();
                if lanes.chance(30)
                    && let Some(id) = lanes.pick(&ids)
                {
                    let (min, max) = LANE_RANGES[lanes.below(LANE_RANGES.len() as u64) as usize];
                    let range = tutti_graph::ParamRange::new(min, max);
                    pair.set_edge_gain_range(id, range);
                    desc.spec.gains.get_mut(&id).expect("picked").range = range;
                }
                if lanes.chance(30)
                    && let Some(id) = lanes.pick(&ids)
                {
                    let w = Amplitude::new(WEIGHTS[lanes.below(WEIGHTS.len() as u64) as usize]);
                    pair.set_edge_gain(id, w);
                    desc.spec.gains.get_mut(&id).expect("picked").base = w;
                }
                let input = input_signal(frame, n);
                let (a, b) = pair.block(n, &input);
                prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {} ({} frames)", frame, n);
                frame += n as u64;
            }
        }
    }
}

/// The driven property is not vacuous: over the seeds a good share of the
/// graphs have driven lanes, and the plans hold driven lanes, lane event
/// rings (a lane earlier than its sink) and sums that wait for a late lane.
#[test]
fn the_generator_draws_driven_lanes() {
    let (mut driven, mut rings, mut ports) = (0, 0, 0);
    for seed in 0..300u64 {
        let mut gains = Rng::new(seed ^ 0x6A14_ED6E);
        let mut lanes = Rng::new(seed ^ 0xD21_7E4);
        let mut desc = random_graph(seed);
        weigh(&mut desc, &mut gains);
        drive(&mut desc, &mut lanes);
        if desc.spec.gains.values().all(|g| g.source.is_none()) {
            continue;
        }
        let valid = desc.spec.validate().expect("valid");
        let shapes: BTreeMap<NodeKey, tutti_graph::Shape> = desc
            .kinds
            .iter()
            .map(|(&k, kind)| (k, shape(kind)))
            .collect();
        let (plan, _) = tutti_graph::compile(&valid, &shapes, &common::prepare(MAX_BLOCK), None)
            .expect("compiles");
        driven += 1;
        ports += plan.gain_ports().len();
        rings += plan
            .delays()
            .iter()
            .filter(|d| matches!(d.key, tutti_graph::DelayKey::EdgeGain { .. }))
            .count();
    }
    eprintln!("graphs {driven}, driven lanes {ports}, lane rings {rings}");
    assert!(driven > 150, "only {driven} graphs with driven lanes");
    assert!(ports > 300, "only {ports} driven lanes");
    assert!(rings > 20, "only {rings} lane event rings");
}

// ---------------------------------------------------------------------------
// Bypass and mute (doc 013, "Bypass and mute are graph state"): modes drawn
// on the generated graphs.
// ---------------------------------------------------------------------------

/// A node and a mode for it, from `rng`: a stream of its own, so the graphs
/// the other properties draw (and the seeds they replay) are untouched. A
/// third of the draws are `Active`, so a toggle back is as common as a toggle
/// away.
fn pick_mode(desc: &Desc, rng: &mut Rng) -> Option<(NodeKey, tutti_graph::NodeMode)> {
    use tutti_graph::NodeMode;
    let key = rng.pick(&desc.order)?;
    let mode = match rng.below(3) {
        0 => NodeMode::Active,
        1 => NodeMode::Bypassed,
        _ => NodeMode::Muted,
    };
    Some((key, mode))
}

/// Moves `desc`'s spec to `mode` for `key`, as `Pair::set_mode` moves both
/// interpreters.
fn note_mode(desc: &mut Desc, key: NodeKey, mode: tutti_graph::NodeMode) {
    desc.spec.set_mode(key, mode);
}

/// Drops the modes of nodes `mutate` removed.
fn prune_modes(desc: &mut Desc) {
    let nodes = &desc.spec.topology.nodes;
    desc.spec.modes.retain(|k, _| nodes.contains_key(k));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Modes on the random graphs, serial and parallel, are the reference's
    /// bit for bit: about one block in five a node (any: latent ones, event
    /// ones, sources, sinks, in-place ones, sums of several inputs, nodes
    /// behind PDC) is set to a random mode, so toggles land mid-fade, during
    /// the warm-up, while parked and in blocks of every length in the
    /// schedule; an edit halfway rewires, removes and regenerates nodes (a
    /// kept node's mode carries, a regenerated one starts steady in the
    /// spec's mode). The parallel render is held to the serial one through
    /// the reference.
    #[test]
    fn modes_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let mut rng = Rng::new(seed ^ 0xED17);
            let mut modes = Rng::new(seed ^ 0x40DE_5EED);
            let mut desc = random_graph(seed);
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&desc.spec.validate().expect("generated graphs are valid"), &desc.kinds);
            let blocks = schedule(which, seed, 1400);
            let half = blocks.len() / 2;
            let mut frame = 0u64;
            for (i, &n) in blocks.iter().enumerate() {
                if i == half {
                    let mut edited = mutate(&desc, &mut rng);
                    prune_modes(&mut edited);
                    pair.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
                    desc = edited;
                }
                if modes.chance(20)
                    && let Some((key, mode)) = pick_mode(&desc, &mut modes)
                {
                    pair.set_mode(key, mode);
                    note_mode(&mut desc, key, mode);
                }
                let input = input_signal(frame, n);
                let (a, b) = pair.block(n, &input);
                prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {} ({} frames)", frame, n);
                frame += n as u64;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mix (doc 013, "Bypass and mute are graph state", M4): mixes drawn on the
// generated graphs, beside modes.
// ---------------------------------------------------------------------------

/// A node and a mix for it, from `rng`: a stream of its own, so the graphs and
/// the modes the other properties draw (and the seeds they replay) are
/// untouched. Half the draws are an end (steady WET or DRY, the cases that
/// copy or leave), the rest are between.
fn pick_mix(desc: &Desc, rng: &mut Rng) -> Option<(NodeKey, tutti_types::Mix)> {
    let key = rng.pick(&desc.order)?;
    let mix = match rng.below(8) {
        0 | 1 => 1.0,
        2 | 3 => 0.0,
        4 => 0.5,
        5 => 0.25,
        6 => 0.875,
        _ => rng.below(1000) as f32 / 999.0,
    };
    Some((key, tutti_types::Mix::new(mix)))
}

/// Drops the mixes of nodes `mutate` removed.
fn prune_mixes(desc: &mut Desc) {
    let nodes = &desc.spec.topology.nodes;
    desc.spec.mixes.retain(|k, _| nodes.contains_key(k));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Mixes on the random graphs, serial and parallel, are the reference's bit
    /// for bit: about one block in four a node (any: latent ones, event ones,
    /// sources, sinks, in-place ones, sums, nodes behind PDC) gets a random mix,
    /// and one in eight of those blocks also a random mode, so a mix meets a
    /// steady Active node, a ramp, a fade, the warm-up and a parked node, in
    /// blocks of every length in the schedule; an edit halfway rewires, removes
    /// and regenerates nodes (a kept node's mix carries, a regenerated one
    /// starts at the spec's).
    #[test]
    fn mixes_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let mut rng = Rng::new(seed ^ 0xED17);
            let mut draws = Rng::new(seed ^ 0x31D5_0F17);
            let mut desc = random_graph(seed);
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&desc.spec.validate().expect("generated graphs are valid"), &desc.kinds);
            let blocks = schedule(which, seed, 1400);
            let half = blocks.len() / 2;
            let mut frame = 0u64;
            for (i, &n) in blocks.iter().enumerate() {
                if i == half {
                    let mut edited = mutate(&desc, &mut rng);
                    prune_modes(&mut edited);
                    prune_mixes(&mut edited);
                    pair.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
                    desc = edited;
                }
                if draws.chance(25)
                    && let Some((key, mix)) = pick_mix(&desc, &mut draws)
                {
                    pair.set_mix(key, mix);
                    desc.spec.set_mix(key, mix);
                    if draws.chance(8)
                        && let Some((key, mode)) = pick_mode(&desc, &mut draws)
                    {
                        pair.set_mode(key, mode);
                        note_mode(&mut desc, key, mode);
                    }
                }
                let input = input_signal(frame, n);
                let (a, b) = pair.block(n, &input);
                prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {} ({} frames)", frame, n);
                frame += n as u64;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Driven mixes (doc 013, M4b): a control lane drives a node's mix.
// ---------------------------------------------------------------------------

/// The keys of the mix lane nodes (`Kind::MixDriver`), clear of every other
/// property's.
const MIX_LANE_KEYS: u64 = 2_000_000;

/// The address of `key`'s mix.
fn mix_port(key: NodeKey) -> tutti_graph::ParamIn {
    tutti_graph::ParamIn {
        node: key,
        param: tutti_graph::NodeMix::ADDR,
    }
}

/// Adds the mix lane nodes (once) and, for about a third of the nodes, a
/// source among them (or none: a source that goes, declicked) and a range,
/// from `rng`: a stream of its own, so no other property's graphs move.
fn drive_mixes(desc: &mut Desc, rng: &mut Rng) {
    if !desc.kinds.contains_key(&NodeKey(MIX_LANE_KEYS)) {
        for (i, &latency) in LANE_LATENCIES.iter().enumerate() {
            let key = NodeKey(MIX_LANE_KEYS + i as u64);
            let kind = Kind::MixDriver {
                period: 8 + 9 * i as u64,
                latency,
            };
            desc.spec.topology.nodes.insert(key, spec_for(&kind));
            desc.kinds.insert(key, kind);
            desc.used.insert(key);
        }
    }
    for key in desc.order.clone() {
        if !rng.chance(33) {
            continue;
        }
        let at = mix_port(key);
        if rng.chance(80) {
            let from = EventOut {
                node: NodeKey(MIX_LANE_KEYS + rng.below(LANE_LATENCIES.len() as u64)),
                port: 0,
            };
            desc.spec.connect_param(at, from);
        } else if let Some(m) = desc.spec.params.get_mut(&at) {
            m.source = None;
        }
        if rng.chance(50) {
            let (min, max) = LANE_RANGES[rng.below(LANE_RANGES.len() as u64) as usize];
            desc.spec
                .set_param_range(at, tutti_graph::ParamRange::new(min, max));
        }
    }
}

/// Drops the lanes of nodes `mutate` removed.
fn prune_mix_lanes(desc: &mut Desc) {
    let nodes = &desc.spec.topology.nodes;
    desc.spec
        .params
        .retain(|at, _| nodes.contains_key(&at.node));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 192, ..ProptestConfig::default() })]

    /// Driven mixes on the random graphs, serial and parallel, are the
    /// reference's bit for bit: lanes of several periods and latencies drive
    /// the mix of about a third of the nodes (latent ones, event ones, sources,
    /// sinks, in-place ones, nodes behind PDC), with ranges that clamp, a mix
    /// moved under a driven lane, a range moved between blocks, and modes
    /// (a driven mix meets a fade, the warm-up and a parked node); an edit
    /// halfway rewires, removes and regenerates nodes and re-draws every source
    /// (a source that changes or goes is declicked, a regenerated node's lane
    /// starts at its value). Own RNGs, so no other property's seeds moved.
    #[test]
    fn driven_mixes_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let mut rng = Rng::new(seed ^ 0xED17);
            let mut draws = Rng::new(seed ^ 0x31D5_0F17);
            let mut lanes = Rng::new(seed ^ 0x6D12_17E5);
            let mut desc = random_graph(seed);
            drive_mixes(&mut desc, &mut lanes);
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&desc.spec.validate().expect("generated graphs are valid"), &desc.kinds);
            let blocks = schedule(which, seed, 1400);
            let half = blocks.len() / 2;
            let mut frame = 0u64;
            for (i, &n) in blocks.iter().enumerate() {
                if i == half {
                    let mut edited = mutate(&desc, &mut rng);
                    prune_modes(&mut edited);
                    prune_mixes(&mut edited);
                    prune_mix_lanes(&mut edited);
                    drive_mixes(&mut edited, &mut lanes);
                    pair.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
                    desc = edited;
                }
                if draws.chance(20)
                    && let Some((key, mix)) = pick_mix(&desc, &mut draws)
                {
                    pair.set_mix(key, mix);
                    desc.spec.set_mix(key, mix);
                }
                if draws.chance(15)
                    && let Some(key) = draws.pick(&desc.order)
                {
                    let (min, max) = LANE_RANGES[draws.below(LANE_RANGES.len() as u64) as usize];
                    let range = tutti_graph::ParamRange::new(min, max);
                    pair.set_mix_range(key, range);
                    desc.spec.set_param_range(mix_port(key), range);
                }
                if draws.chance(8)
                    && let Some((key, mode)) = pick_mode(&desc, &mut draws)
                {
                    pair.set_mode(key, mode);
                    note_mode(&mut desc, key, mode);
                }
                let input = input_signal(frame, n);
                let (a, b) = pair.block(n, &input);
                prop_assert_eq!(bits(&a), bits(&b), "diverged at frame {} ({} frames)", frame, n);
                frame += n as u64;
            }
        }
    }
}

/// The driven-mix property is not vacuous: over the seeds a good share of the
/// graphs have driven mixes, and the plans hold mix ports, lane event rings (a
/// lane earlier than its node) and nodes that wait for a late lane.
#[test]
fn the_generator_draws_driven_mixes() {
    let (mut driven, mut ports, mut rings) = (0, 0, 0);
    for seed in 0..300u64 {
        let mut lanes = Rng::new(seed ^ 0x6D12_17E5);
        let mut desc = random_graph(seed);
        drive_mixes(&mut desc, &mut lanes);
        if desc.spec.params.values().all(|m| m.source.is_none()) {
            continue;
        }
        let valid = desc.spec.validate().expect("valid");
        let shapes: BTreeMap<NodeKey, tutti_graph::Shape> = desc
            .kinds
            .iter()
            .map(|(&k, kind)| (k, shape(kind)))
            .collect();
        let (plan, _) = tutti_graph::compile(&valid, &shapes, &common::prepare(MAX_BLOCK), None)
            .expect("compiles");
        driven += 1;
        ports += plan.param_ports().iter().filter(|p| p.is_mix()).count();
        rings += plan
            .delays()
            .iter()
            .filter(|d| {
                matches!(d.key, tutti_graph::DelayKey::ParamEvent { at, .. }
                    if at.param == tutti_graph::NodeMix::ADDR)
            })
            .count();
    }
    eprintln!("graphs {driven}, mix lanes {ports}, lane rings {rings}");
    assert!(driven > 200, "only {driven} graphs with driven mixes");
    assert!(ports > 400, "only {ports} driven mixes");
    assert!(rings > 20, "only {rings} lane event rings");
}

/// The mix property is not vacuous: over the seeds the draws put a mix between
/// the ends on latent nodes, on nodes behind PDC-free chains, and on nodes
/// that a mode draw also reaches.
#[test]
fn the_generator_draws_mixes_on_latent_and_plain_nodes() {
    let (mut latent_between, mut plain_between, mut dry, mut wet) = (0, 0, 0, 0);
    for seed in 0..300u64 {
        let mut draws = Rng::new(seed ^ 0x31D5_0F17);
        let desc = random_graph(seed);
        for _ in 0..80 {
            if !draws.chance(25) {
                continue;
            }
            let Some((key, mix)) = pick_mix(&desc, &mut draws) else {
                continue;
            };
            let latent = shape(&desc.kinds[&key]).latency.samples().get() > 0;
            match mix.get() {
                0.0 => dry += 1,
                1.0 => wet += 1,
                _ => {
                    latent_between += usize::from(latent);
                    plain_between += usize::from(!latent);
                }
            }
        }
    }
    eprintln!("latent {latent_between}, plain {plain_between}, dry {dry}, wet {wet}");
    assert!(latent_between > 100 && plain_between > 100);
    assert!(dry > 100 && wet > 100);
}

/// The modes property is not vacuous: over the seeds the draws put every mode
/// on latent nodes, on event nodes, on nodes with no audio input and on wide
/// ones, and toggle a node away and back.
#[test]
fn the_generator_draws_modes_on_every_kind_of_node() {
    use tutti_graph::NodeMode;
    let (mut latent_bypass, mut event_mute, mut event_bypass, mut source_bypass) = (0, 0, 0, 0);
    let (mut wide_mute, mut round_trips, mut unlatent_bypass) = (0, 0, 0);
    for seed in 0..300u64 {
        let mut modes = Rng::new(seed ^ 0x40DE_5EED);
        let desc = random_graph(seed);
        let mut last: BTreeMap<NodeKey, NodeMode> = BTreeMap::new();
        for _ in 0..80 {
            if !modes.chance(20) {
                continue;
            }
            let Some((key, mode)) = pick_mode(&desc, &mut modes) else {
                continue;
            };
            let s = shape(&desc.kinds[&key]);
            let latent = s.latency.samples().get() > 0;
            let events = s.event_in + s.event_out > 0;
            let silent_in = s.audio_in().count() == 0;
            match mode {
                NodeMode::Bypassed => {
                    latent_bypass += usize::from(latent);
                    unlatent_bypass += usize::from(!latent && !silent_in);
                    event_bypass += usize::from(events);
                    source_bypass += usize::from(silent_in);
                }
                NodeMode::Muted => {
                    event_mute += usize::from(events);
                    wide_mute += usize::from(s.audio_out().count() > 1);
                }
                NodeMode::Active => {}
            }
            if mode == NodeMode::Active && last.get(&key).is_some_and(|&m| m != NodeMode::Active) {
                round_trips += 1;
            }
            last.insert(key, mode);
        }
    }
    eprintln!(
        "latent bypass {latent_bypass}, plain bypass {unlatent_bypass}, event bypass {event_bypass}, \
         source bypass {source_bypass}, event mute {event_mute}, wide mute {wide_mute}, \
         round trips {round_trips}"
    );
    assert!(
        latent_bypass > 100 && unlatent_bypass > 100,
        "{latent_bypass} {unlatent_bypass}"
    );
    assert!(
        event_bypass > 100 && event_mute > 100,
        "{event_bypass} {event_mute}"
    );
    assert!(
        source_bypass > 100 && wide_mute > 50,
        "{source_bypass} {wide_mute}"
    );
    assert!(round_trips > 300, "{round_trips}");
}

// ---------------------------------------------------------------------------
// Bus edges (doc 013, "Buses and coercion between declared layouts", B2).
// ---------------------------------------------------------------------------

/// A graph of nodes declaring buses, wired by bus edges (and some plain
/// ones), drawn from its **own** stream (`seed ^ 0x0B05_ED6E`) so no other
/// generator's seed moves. Every bus edge kept lowers, so the value the
/// interpreters take is the lowered spec's.
struct BusGraph {
    spec: GraphSpec,
    kinds: BTreeMap<NodeKey, Kind>,
}

fn bus_of(width: usize, named: bool) -> Bus {
    let layout = ChannelLayout::from_count(width as u16);
    let topology = match ChannelTopology::smpte(layout).filter(|_| named) {
        Some(t) => BusTopology::try_from(&t).expect("fits a bus"),
        None => BusTopology::unassigned(layout).expect("fits a bus"),
    };
    Bus {
        topology,
        role: BusRole::Main,
    }
}

fn bus_graph(seed: u64) -> BusGraph {
    let mut rng = Rng::new(seed ^ 0x0B05_ED6E);
    let globals = 1 + rng.below(6) as usize;
    let mut spec = GraphSpec::default();
    spec.topology.inputs = ChannelLayout::from_count(globals as u16);
    let mut kinds: BTreeMap<NodeKey, Kind> = BTreeMap::new();
    let mut widths: Vec<(NodeKey, usize)> = Vec::new();
    let nodes = 3 + rng.below(6) as usize;
    for j in 1..=nodes as u64 {
        let node = NodeKey(j);
        let width = rng.pick(&[1usize, 1, 2, 2, 6]).expect("a width");
        let named = rng.chance(75);
        let source = j == 1 || rng.chance(15);
        let inner = if width == 1 && !source && rng.chance(30) {
            Kind::Lag {
                latency: 1 + rng.below(20) as usize,
            }
        } else {
            Kind::Gain {
                gain: rng.pick(&[0.5f32, 0.75, 1.0, 0.3]).expect("a gain"),
                width,
            }
        };
        let kind = Kind::Bused {
            inner: Box::new(inner),
            buses_in: Box::new(Buses::main(bus_of(width, named).topology)),
            buses_out: Box::new(Buses::main(bus_of(width, named).topology)),
        };
        spec.topology.nodes.insert(node, spec_for(&kind));
        kinds.insert(node, kind);
        if source {
            for c in 0..width as u16 {
                spec.topology.connect(
                    InPort { node, port: c },
                    Edge::Direct(Source::Global(c % globals as u16)),
                );
            }
        } else {
            // One or two bus edges from earlier nodes, kept when they lower.
            for _ in 0..1 + rng.below(2) {
                let Some((from, _)) = rng.pick(&widths) else {
                    continue;
                };
                let mut trial = spec.clone();
                trial.connect_bus(
                    BusSource::Node(BusRef { node: from, bus: 0 }),
                    BusRef { node, bus: 0 },
                );
                if trial.lowered(&common::shapes_of(&kinds)).is_ok() {
                    spec = trial;
                }
            }
            // And now and then an ordinary edge into the same sink.
            if rng.chance(30)
                && let Some((from, w)) = rng.pick(&widths)
            {
                let mut trial = spec.clone();
                trial.topology.connect(
                    InPort {
                        node,
                        port: rng.below(width as u64) as u16,
                    },
                    Edge::Direct(Source::Node(OutPort {
                        node: from,
                        port: rng.below(w as u64) as u16,
                    })),
                );
                if trial.lowered(&common::shapes_of(&kinds)).is_ok() {
                    spec = trial;
                }
            }
        }
        widths.push((node, width));
    }
    let &(last, width) = widths.last().expect("a node");
    spec.topology.set_outputs(
        (0..width as u16)
            .map(|port| Source::Node(OutPort { node: last, port }))
            .collect::<Vec<_>>(),
    );
    BusGraph { spec, kinds }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// A bus edge is nothing the interpreters know: the lowered spec renders
    /// the same on the executor, serial and parallel, as on the reference,
    /// bit for bit, at every block schedule, over random graphs of folds
    /// (weighted lanes of every width and unity copies) beside ordinary
    /// edges, with PDC rings where a latent node sits on a lane.
    #[test]
    fn bus_edges_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let graph = bus_graph(seed);
            let lowered = graph
                .spec
                .lowered(&common::shapes_of(&graph.kinds))
                .expect("every kept bus edge lowers");
            let valid = lowered.spec.validate().expect("generated graphs are valid");
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&valid, &graph.kinds);
            let mut frame = 0;
            run(&mut pair, &schedule(which, seed, 500), &mut frame);
        }
    }
}

/// The bus generator is not vacuous: over a fixed range of seeds it makes
/// graphs with bus edges, weighted lanes (a fold with `-3 dB` or `1/2`
/// coefficients), unity copies, and a bus edge beside an ordinary edge on one
/// sink port.
#[test]
fn the_bus_generator_reaches_folds_copies_and_mixes() {
    let (mut with_edges, mut weighted, mut unity, mut mixed) = (0, 0, 0, 0);
    for seed in 0..400u64 {
        let graph = bus_graph(seed);
        let lowering = graph
            .spec
            .lowered(&common::shapes_of(&graph.kinds))
            .expect("lowers");
        with_edges += usize::from(!graph.spec.bus_edges.is_empty());
        weighted += usize::from(!lowering.weights().is_empty());
        unity += usize::from(lowering.lanes.values().any(|w| w.get() == 1.0));
        mixed += usize::from(lowering.lanes.keys().any(|id| {
            match id.sink {
                Sink::Port(at) => graph
                    .spec
                    .topology
                    .edges
                    .get(&at)
                    .is_some_and(|edges| !edges.is_empty()),
                Sink::Output(_) => false,
            }
        }));
    }
    eprintln!("bus edges {with_edges}, weighted {weighted}, unity {unity}, mixed {mixed}");
    assert!(with_edges > 300 && weighted > 100 && unity > 100 && mixed > 20);
}

// ---------------------------------------------------------------------------
// Bus edges into the root (doc 013, "Buses", B4).
// ---------------------------------------------------------------------------

/// `bus_graph(seed)` with a declared root and bus edges into it, drawn from a
/// stream of their own (`seed ^ 0x0B05_4007`), after the graph, so neither
/// `bus_graph`'s draws nor any other generator's move. The root is 1, 2 or 6
/// channels, named or unassigned; its ordinary output sources past its width
/// are dropped, and a root edge is kept when the spec still lowers (an
/// unfoldable pair, or a lane on an ordinary output source, is not).
fn root_bus_graph(seed: u64) -> BusGraph {
    let mut graph = bus_graph(seed);
    let mut rng = Rng::new(seed ^ 0x0B05_4007);
    let width = rng.pick(&[1usize, 2, 2, 6]).expect("a width");
    let root = bus_of(width, rng.chance(75)).topology;
    graph.spec.root = Some(root);
    graph.spec.topology.outputs.truncate(width);
    let nodes: Vec<NodeKey> = graph.kinds.keys().copied().collect();
    for _ in 0..1 + rng.below(3) {
        let Some(node) = rng.pick(&nodes) else {
            continue;
        };
        let mut trial = graph.spec.clone();
        trial.connect_bus_to_root(BusSource::Node(BusRef { node, bus: 0 }));
        if trial.lowered(&common::shapes_of(&graph.kinds)).is_ok() {
            graph.spec = trial;
        }
    }
    graph
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// A bus edge into the root is lanes on the output channels and nothing
    /// the interpreters know: the lowered spec renders the same on the
    /// executor, serial and parallel, as on the reference, bit for bit, with
    /// PDC aligning each lane to the outputs' time.
    #[test]
    fn root_bus_edges_are_bit_identical_to_the_reference(seed in any::<u64>(), which in 0usize..5) {
        for workers in modes(seed) {
            let graph = root_bus_graph(seed);
            let lowered = graph
                .spec
                .lowered(&common::shapes_of(&graph.kinds))
                .expect("every kept root edge lowers");
            let valid = lowered.spec.validate().expect("generated graphs are valid");
            let mut pair = Pair::with_workers(MAX_BLOCK, workers);
            pair.switch(&valid, &graph.kinds);
            let mut frame = 0;
            run(&mut pair, &schedule(which, seed, 500), &mut frame);
        }
    }
}

/// The root generator is not vacuous: over a fixed range of seeds it makes
/// root edges, weighted root lanes, unity root lanes, and a root lane beside
/// an ordinary output source on one channel.
#[test]
fn the_root_generator_reaches_folds_copies_and_mixes() {
    let (mut with_edges, mut weighted, mut unity, mut mixed) = (0, 0, 0, 0);
    for seed in 0..400u64 {
        let graph = root_bus_graph(seed);
        let lowering = graph
            .spec
            .lowered(&common::shapes_of(&graph.kinds))
            .expect("lowers");
        with_edges += usize::from(!graph.spec.root_sources.is_empty());
        let roots = || {
            lowering
                .lanes
                .iter()
                .filter(|(id, _)| matches!(id.sink, Sink::Output(_)))
        };
        weighted += usize::from(roots().any(|(_, w)| w.get() != 1.0));
        unity += usize::from(roots().any(|(_, w)| w.get() == 1.0));
        mixed += usize::from(roots().any(|(id, _)| {
            match id.sink {
                Sink::Output(c) => graph
                    .spec
                    .topology
                    .outputs
                    .get(usize::from(c))
                    .is_some_and(|o| !o.is_empty()),
                Sink::Port(_) => false,
            }
        }));
    }
    eprintln!("root edges {with_edges}, weighted {weighted}, unity {unity}, mixed {mixed}");
    assert!(with_edges > 200 && weighted > 50 && unity > 50 && mixed > 10);
}
