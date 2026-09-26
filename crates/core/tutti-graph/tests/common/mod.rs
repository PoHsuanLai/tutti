//! Small test nodes and a harness that drives the executor and the reference
//! interpreter side by side.
//!
//! Every node here is deterministic, declares an **honest** tail (the
//! executor's silence skip trusts it), and never turns silence into `-0.0`
//! (all gains are positive): a skipped node writes `+0.0`, so a node that
//! produced `-0.0` from silence would differ in the sign bit only, which the
//! bit-exact comparisons below would report as a failure of the skip rather
//! than of the node.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tutti_graph::{
    compile, Cx, Editor, Event, EventKind, Executor, Fade, Io, Node, Plan, Prepare, Reference,
    Shape, Shapes, Status, Transport, TransportChanges, Ump, ValidGraph,
};
use tutti_types::{ChannelLayout, Frame, Latency, NodeKey, SampleRate, Samples, Tail};

pub const RATE: SampleRate = SampleRate(48_000.0);

/// How many times an `EventLag` forwards one event.
pub const MAX_HOPS: u32 = 2;

/// Events per slot in the differential harness: far above what a generated
/// graph produces, so an overflow is a finding (asserted) rather than a
/// divergence.
pub const EVENT_CAPACITY: usize = 16_384;

pub fn prepare(max_block: usize) -> Prepare {
    Prepare::new(RATE, Samples(max_block))
}

fn lat(frames: usize) -> Latency {
    Latency::new(Samples(frames))
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::from_count(n as u16)
}

/// What a test node does. `Clone` so the harness can build two identical
/// instances — one for each interpreter.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Constant `value` on `width` outputs; writes sample 0 and returns
    /// `Status::Constant`.
    Const { value: f32, width: usize },
    /// `width` channels scaled by `gain`; accepts in-place channels.
    Gain { gain: f32, width: usize },
    /// Sum of `inputs` channels onto one output.
    Sum { inputs: usize },
    /// `y[n] = 0.5 (x[n] + x[n-1])`: one sample of memory, in place.
    Smooth,
    /// A pure delay of `latency` frames, declared as processing latency.
    Lag { latency: usize },
    /// Emits a MIDI event every `period` frames of absolute time, offset by
    /// `phase`; also outputs how many it has emitted, as audio.
    Emitter { period: u64, phase: u64 },
    /// Emits `burst` MIDI events on every frame where `(frame + phase) %
    /// period == 0`, on a port declaring `cap` events per block
    /// (`Shape::event_capacity`); also outputs how many its writer
    /// accepted, as audio. With `cap < burst` the writer refuses the newest
    /// of each burst — the same ones in both interpreters. `period` is at
    /// least `MAX_BLOCK` in the generator, so the port keeps its declared
    /// *rate* (at most `cap` per `MaxBlock` frames) and nothing downstream
    /// may drop.
    Burst {
        period: u64,
        phase: u64,
        burst: u32,
        cap: u32,
    },
    /// Folds incoming events into a running value: `s = 0.5 s + (w0 % 97)`.
    /// Order-sensitive, so a merge that reorders ties is caught.
    Consumer { inputs: u16 },
    /// Re-emits its input events `latency` frames later.
    EventLag { latency: usize },
    /// Reads the environment: frame, arrival latency, tempo.
    EnvProbe,
    /// `Status::Bypass` over `width` channels.
    Thru { width: usize },
    /// `Status::Bypass` over `width` channels, accepting in-place channels —
    /// the executor must then leave the aliased output as it is.
    ThruInPlace { width: usize },
    /// `width` in-place audio channels scaled by a gain its event inputs
    /// steer, and every output event port forwarding one input port. Audio
    /// and events in one node, at any width: it reaches the executor's
    /// general borrow path in every bucket, with or without in-place
    /// channels.
    Mixed {
        width: usize,
        events_in: u16,
        events_out: u16,
    },
    /// A spec-driven node for the ported shapes: `dc`/`gain`/`sum`/`fan`
    /// behaviour with a *declared* latency and tail it does not realise.
    Spec {
        behaviour: SpecBehaviour,
        ins: usize,
        outs: usize,
        latency: usize,
        tail: Tail,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpecBehaviour {
    Dc(f32),
    Gain(f32),
    Sum,
    Fan,
}

pub struct TestNode {
    kind: Kind,
    ring: VecDeque<f32>,
    prev: f32,
    count: f32,
    state: f32,
    pending: VecDeque<(Frame, Event)>,
    /// The rate last prepared for.
    rate: Option<f64>,
    calls: Arc<AtomicUsize>,
}

impl TestNode {
    pub fn new(kind: Kind) -> Self {
        Self::counted(kind, Arc::new(AtomicUsize::new(0)))
    }

    /// A node that counts its `process` calls into `calls`.
    pub fn counted(kind: Kind, calls: Arc<AtomicUsize>) -> Self {
        let ring = match kind {
            Kind::Lag { latency } => VecDeque::from(vec![0.0; latency]),
            _ => VecDeque::new(),
        };
        Self {
            kind,
            ring,
            prev: 0.0,
            count: 0.0,
            state: 0.0,
            pending: VecDeque::with_capacity(1024),
            rate: None,
            calls,
        }
    }
}

impl Node for TestNode {
    fn shape(&self) -> Shape {
        match self.kind {
            Kind::Const { width, .. } => Shape::audio(ch(0), ch(width)),
            Kind::Gain { width, .. } => Shape::audio(ch(width), ch(width)).with_in_place(),
            Kind::Sum { inputs } => Shape::audio(ch(inputs), ch(1)),
            Kind::Smooth => Shape::audio(ch(1), ch(1))
                .with_tail(Tail::Finite(Samples(1)))
                .with_in_place(),
            Kind::Lag { latency } => Shape::audio(ch(1), ch(1))
                .with_latency(lat(latency))
                .with_tail(if latency == 0 {
                    Tail::None
                } else {
                    Tail::Finite(Samples(latency))
                }),
            Kind::Emitter { .. } => Shape::audio(ch(0), ch(1)).with_events(0, 1),
            Kind::Burst { cap, .. } => Shape::audio(ch(0), ch(1))
                .with_events(0, 1)
                .with_event_capacity(cap),
            Kind::Consumer { inputs } => Shape::audio(ch(0), ch(1))
                .with_events(inputs, 0)
                .with_tail(Tail::Unbounded),
            Kind::EventLag { latency } => Shape::audio(ch(0), ch(0))
                .with_events(1, 1)
                .with_latency(lat(latency))
                .with_tail(Tail::Finite(Samples(latency))),
            Kind::EnvProbe => Shape::audio(ch(0), ch(1)),
            Kind::Thru { width } => Shape::audio(ch(width), ch(width)),
            Kind::ThruInPlace { width } => Shape::audio(ch(width), ch(width)).with_in_place(),
            Kind::Mixed {
                width,
                events_in,
                events_out,
            } => Shape::audio(ch(width), ch(width))
                .with_events(events_in, events_out)
                .with_tail(Tail::Unbounded)
                .with_in_place(),
            Kind::Spec {
                ins,
                outs,
                latency,
                tail,
                ..
            } => Shape::audio(ch(ins), ch(outs))
                .with_latency(lat(latency))
                .with_tail(tail),
        }
    }

    fn prepare(&mut self, p: &Prepare) {
        // `Frame` means samples at the current rate: an `EventLag` holding
        // absolute due frames moves them to the same wall-clock time when the
        // rate changes, as the executor moves its own clock.
        let rate = p.sample_rate().get();
        if let Some(old) = self.rate.filter(|&old| old != rate) {
            for (due, _) in &mut self.pending {
                *due = Frame((due.get() as f64 * rate / old).round() as u64);
            }
        }
        self.rate = Some(rate);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let n = io.frames();
        match self.kind {
            Kind::Const { value, .. } => {
                for c in 0..io.output_count() {
                    io.output(c)[0] = value;
                }
                Status::Constant
            }
            Kind::Gain { gain, width } => {
                for c in 0..width {
                    io.channel(c).map(|x| x * gain);
                }
                Status::Modified
            }
            Kind::Sum { inputs } => {
                let (ins, mut outs) = io.split();
                let out = outs.get(0);
                out.fill(0.0);
                for c in 0..inputs {
                    for (o, &i) in out.iter_mut().zip(ins.get(c)) {
                        *o += i;
                    }
                }
                Status::Modified
            }
            Kind::Smooth => {
                let prev = &mut self.prev;
                io.channel(0).map(|now| {
                    let y = 0.5 * (now + *prev);
                    *prev = now;
                    y
                });
                Status::Modified
            }
            Kind::Lag { latency } => {
                let (ins, mut outs) = io.split();
                for (o, &i) in outs.get(0).iter_mut().zip(ins.get(0)) {
                    if latency == 0 {
                        *o = i;
                    } else {
                        // Pop before push, so the ring never grows past the
                        // capacity it was built with (the no-alloc test runs
                        // this node).
                        *o = self.ring.pop_front().expect("ring");
                        self.ring.push_back(i);
                    }
                }
                Status::Modified
            }
            Kind::Emitter { period, phase } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if (f + phase).is_multiple_of(period) {
                        let _ = io.event_out(0).push(Event::midi(at, [f as u32, 0, 0, 0]));
                        self.count += 1.0;
                    }
                }
                io.output(0).fill(self.count);
                Status::Modified
            }
            Kind::Burst {
                period,
                phase,
                burst,
                ..
            } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if (f + phase).is_multiple_of(period) {
                        for i in 0..burst {
                            let e = Event::midi(at, [f as u32 + i, 0, 0, 0]);
                            if io.event_out(0).push(e).is_ok() {
                                self.count += 1.0;
                            }
                        }
                    }
                }
                io.output(0).fill(self.count);
                Status::Modified
            }
            Kind::Consumer { inputs } => {
                // A merge of this node's own ports, in port order, so several
                // event inputs are consumed deterministically too.
                let mut heads = [0usize; 8];
                for i in 0..n {
                    for (p, h) in heads.iter_mut().enumerate().take(inputs as usize) {
                        let evs = io.events(p);
                        while *h < evs.len() && evs[*h].offset.index() == i {
                            if let EventKind::Midi(Ump(w)) = evs[*h].kind {
                                self.state = 0.5 * self.state + (w[0] % 97) as f32 + p as f32;
                            }
                            *h += 1;
                        }
                    }
                    io.output(0)[i] = self.state;
                }
                Status::Modified
            }
            Kind::EventLag { latency } => {
                for e in io.events(0) {
                    // Word 1 counts hops. Forwarding at most `MAX_HOPS`
                    // times bounds how far an event feedback loop can
                    // multiply events -- without it a random graph can
                    // amplify exponentially until the executor's
                    // preallocated slots overflow (which it reports, see
                    // `Pair::block`) and the unbounded reference runs out of
                    // memory.
                    let EventKind::Midi(Ump(mut w)) = e.kind else {
                        continue;
                    };
                    if w[1] >= MAX_HOPS {
                        continue;
                    }
                    w[1] += 1;
                    let fwd = Event::midi(e.offset, w);
                    // Kept sorted by due frame (stably): after a rate change
                    // rescaled the older entries, new ones can fall due
                    // before them. `insert` stays within the reserved
                    // capacity, so it does not allocate.
                    let due = cx.env.frame_at(e.offset) + Samples(latency);
                    let at = self.pending.partition_point(|&(d, _)| d <= due);
                    self.pending.insert(at, (due, fwd));
                }
                while let Some(&(due, e)) = self.pending.front() {
                    // Queued in due order. One behind the block — the clock
                    // jumped: a re-prepare's suspension, or a rate change
                    // rescaling it — goes out at once rather than never.
                    let offset = match cx.env.offset_of(due) {
                        Some(o) => o,
                        None if due < cx.env.frame => tutti_graph::Offset::ZERO,
                        None => break,
                    };
                    self.pending.pop_front();
                    let _ = io.event_out(0).push(Event { offset, ..e });
                }
                Status::Modified
            }
            Kind::EnvProbe => {
                let base =
                    cx.arrival.samples().get() as f32 + cx.env.transport.tempo.get() as f32 * 1e-3;
                // The transport per frame, changes included, so both
                // interpreters must hand every node the same `Env`.
                let env = *cx.env;
                for (k, o) in env.offsets().zip(io.output(0).iter_mut()) {
                    let t = env.transport_at(k);
                    let moving = if t.playing { 0.5 } else { 0.0 };
                    *o = ((env.frame.get() + k.get() as u64) % 1000) as f32 * 1e-3
                        + base
                        + moving
                        + t.beat().get().fract() as f32;
                }
                Status::Modified
            }
            Kind::Thru { .. } | Kind::ThruInPlace { .. } => Status::Bypass,
            Kind::Mixed {
                width,
                events_in,
                events_out,
            } => {
                // The gain follows every event on every port, in port order
                // (order-sensitive, like `Consumer`), and stays positive so
                // silence stays `+0.0`.
                for p in 0..events_in as usize {
                    for e in io.events(p) {
                        if let EventKind::Midi(Ump(w)) = e.kind {
                            self.state = 0.5 * self.state + ((w[0] % 7) as f32 + p as f32) * 0.125;
                        }
                    }
                }
                let gain = 1.0 + self.state;
                for c in 0..width {
                    let g = gain * (c + 1) as f32;
                    io.channel(c).map(|x| x * g);
                }
                if events_in > 0 {
                    for q in 0..events_out as usize {
                        let from = io.events(q % events_in as usize);
                        for &e in from {
                            let _ = io.event_out(q).push(e);
                        }
                    }
                }
                Status::Modified
            }
            Kind::Spec {
                behaviour,
                outs,
                ins: width,
                ..
            } => {
                let (ins, mut o) = io.split();
                for c in 0..outs {
                    for i in 0..n {
                        let v = match behaviour {
                            SpecBehaviour::Dc(v) => v,
                            SpecBehaviour::Gain(g) => {
                                if c < width {
                                    ins.get(c)[i] * g
                                } else {
                                    0.0
                                }
                            }
                            SpecBehaviour::Sum => (0..width).map(|k| ins.get(k)[i]).sum(),
                            SpecBehaviour::Fan => {
                                if width == 0 {
                                    0.0
                                } else {
                                    ins.get(c % width)[i]
                                }
                            }
                        };
                        o.get(c)[i] = v;
                    }
                }
                Status::Modified
            }
        }
    }

    fn reset(&mut self) {
        *self = Self::counted(self.kind.clone(), Arc::clone(&self.calls));
    }
}

/// Shapes for every node in `kinds`.
pub fn shapes_of(kinds: &BTreeMap<NodeKey, Kind>) -> Shapes {
    kinds
        .iter()
        .map(|(&k, kind)| (k, TestNode::new(kind.clone()).shape()))
        .collect()
}

/// A fresh unit for each key in `keys`.
pub fn units_for(
    kinds: &BTreeMap<NodeKey, Kind>,
    keys: impl IntoIterator<Item = NodeKey>,
) -> BTreeMap<NodeKey, Box<dyn Node>> {
    keys.into_iter()
        .map(|k| {
            (
                k,
                Box::new(TestNode::new(kinds[&k].clone())) as Box<dyn Node>,
            )
        })
        .collect()
}

/// The executor and the reference, driven together.
pub struct Pair {
    pub editor: Editor,
    pub exec: Executor,
    pub reference: Reference,
    pub plan: Option<Plan>,
    pub outputs: usize,
    pub inputs: usize,
    pub max_block: usize,
}

impl Pair {
    pub fn new(max_block: usize) -> Self {
        // The harness compiles itself, to hand the same spec to both
        // interpreters, and ships the plan through `Editor::package`.
        let (editor, exec) = Editor::with_event_capacity(prepare(max_block), EVENT_CAPACITY);
        Self {
            editor,
            exec,
            reference: Reference::new(prepare(max_block)),
            plan: None,
            outputs: 0,
            inputs: 0,
            max_block,
        }
    }

    /// Switch both to `graph`, building fresh units for whatever is new or
    /// regenerated.
    pub fn switch(&mut self, graph: &ValidGraph, kinds: &BTreeMap<NodeKey, Kind>) {
        self.switch_with_fades(graph, kinds, &BTreeMap::new());
    }

    /// `switch`, with the regenerated keys in `fades` crossfading
    /// (`Editor::replace`'s contract). The executor gets the fades for the
    /// keys its delta replaces, as `Editor::commit` attaches them; the
    /// reference gets them all and decides for itself.
    ///
    /// A node's **declared latency is the spec's**, as after
    /// `Editor::set_latency`: the shapes compiled against take it from
    /// `graph`, not from the kind. A kept unit whose declared latency
    /// changed has its crossfade cut (`Delta::cuts`, as `Editor::commit`
    /// attaches it, derived here from the two plans); the reference derives
    /// the same from the two specs.
    pub fn switch_with_fades(
        &mut self,
        graph: &ValidGraph,
        kinds: &BTreeMap<NodeKey, Kind>,
        fades: &BTreeMap<NodeKey, Fade>,
    ) {
        let mut shapes = shapes_of(kinds);
        for (k, s) in shapes.iter_mut() {
            if let Some(n) = graph.topology().nodes.get(k) {
                s.latency = Latency::new(n.latency);
            }
        }
        let (plan, mut delta) =
            compile(graph, &shapes, &prepare(self.max_block), self.plan.as_ref())
                .expect("compiles");
        delta.cuts = plan
            .units()
            .iter()
            .filter(|now| {
                self.plan
                    .as_ref()
                    .and_then(|p| p.unit(now.key))
                    .is_some_and(|was| {
                        was.gen == now.gen
                            && was.idx == now.idx
                            && was.shape.latency != now.shape.latency
                    })
            })
            .map(|u| tutti_graph::Placement {
                key: u.key,
                gen: u.gen,
                idx: u.idx,
            })
            .collect();
        tutti_graph::verify(&plan).expect("verifies");
        delta.fades = delta
            .replace
            .iter()
            .filter_map(|&(_, new)| fades.get(&new.key).map(|&f| (new.key, f)))
            .collect();
        let placed: Vec<NodeKey> = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, n)| n.key))
            .collect();
        self.outputs = graph.topology().outputs.len();
        self.inputs = graph.topology().inputs.count() as usize;
        self.plan = Some(plan.clone());
        // Back on the control side, where the box and what it retired are
        // freed.
        self.editor
            .package(plan, delta, units_for(kinds, placed))
            .expect("the harness collects every box");
        self.exec.apply_pending();
        self.editor.collect();

        // The reference decides for itself what is new, from generations; give
        // it a fresh unit for every key it might need.
        let all = units_for(kinds, graph.topology().nodes.keys().copied());
        self.reference.set_graph_with_fades(graph, all, fades);
    }

    /// Render one block through both; return (executor, reference) outputs.
    pub fn block(&mut self, frames: usize, input: &[f32]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let transport = Transport::new(true, tutti_types::Bpm(120.0), tutti_types::Beat(0.0), None);
        self.block_at(frames, input, &transport)
    }

    /// `block`, under a transport the caller moves.
    pub fn block_at(
        &mut self,
        frames: usize,
        input: &[f32],
        transport: &Transport,
    ) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        self.block_with_changes(frames, input, transport, &TransportChanges::NONE)
    }

    /// `block_at`, with the transport changing inside the block.
    pub fn block_with_changes(
        &mut self,
        frames: usize,
        input: &[f32],
        transport: &Transport,
        changes: &TransportChanges,
    ) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let transport = *transport;
        // Channel `c` of the graph input is the signal scaled by 2^-c: exact,
        // and distinct per channel.
        let chans: Vec<Vec<f32>> = (0..self.inputs.max(1))
            .map(|c| {
                input[..frames]
                    .iter()
                    .map(|x| x * 0.5f32.powi(c as i32))
                    .collect()
            })
            .collect();
        let ins: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        let mut a = vec![vec![0.0f32; frames]; self.outputs];
        let mut b = vec![vec![0.0f32; frames]; self.outputs];
        {
            let mut outs: Vec<&mut [f32]> = a.iter_mut().map(Vec::as_mut_slice).collect();
            self.exec
                .process_with_changes(frames, &transport, changes, &ins, &mut outs);
        }
        {
            let mut outs: Vec<&mut [f32]> = b.iter_mut().map(Vec::as_mut_slice).collect();
            self.reference
                .process_with_changes(frames, &transport, changes, &ins, &mut outs);
        }
        // The one place either interpreter may refuse an event is a writer
        // past its node's declared capacity (`Kind::Burst`), which both do
        // alike. Anything else the executor drops — a merge or a delay FIFO
        // short of what the declarations promise, or a slot past its
        // preallocated capacity — would surface as a divergence far from its
        // cause, so the two counts are asserted equal here, where it happens.
        assert_eq!(
            self.exec.dropped_events(),
            self.reference.dropped_events(),
            "the executor dropped events the reference did not"
        );
        (a, b)
    }
}

/// A deterministic global input signal.
pub fn input_signal(start: u64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| {
            let f = start + i as u64;
            // Silent stretches, so the skip path is exercised.
            if (f / 50).is_multiple_of(3) {
                0.0
            } else {
                ((f % 17) as f32 - 8.0) * 0.0625
            }
        })
        .collect()
}

/// Bits, so a comparison is exact (and NaN-safe).
pub fn bits(v: &[Vec<f32>]) -> Vec<Vec<u32>> {
    v.iter()
        .map(|c| c.iter().map(|x| x.to_bits()).collect())
        .collect()
}
