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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Event, EventKind, Executor, Fade, Io, Node, Plan, Prepare, Reference, Shape,
    Shapes, Status, Transport, TransportChanges, ValidGraph, compile,
};
use tutti_types::{ChannelLayout, Frame, Latency, SampleRate, Samples, Tail};
use tutti_ump::Ump;

pub const RATE: SampleRate = SampleRate(48_000.0);

/// How many times an `EventLag` forwards one event.
pub const MAX_HOPS: u32 = 2;

/// Events per slot in the differential harness: far above what a generated
/// graph produces, so an overflow is a finding (asserted) rather than a
/// divergence.
pub const EVENT_CAPACITY: usize = 16_384;

/// What a [`Kind::GainDriver`] steps its gain to at frame `f`: a figure
/// that changes at every step and is exact in `f32`.
pub fn driver_value(f: u64, period: u64) -> f32 {
    ((f / period) % 7) as f32 * 0.25 - 0.5
}

pub fn prepare(max_block: usize) -> Prepare {
    Prepare::new(RATE, Samples(max_block))
}

fn lat(frames: usize) -> Latency {
    Latency::new(Samples(frames))
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::try_from(n).expect("a test width is small")
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
    /// A lane for an edge gain (doc 013, "Edge gain"): on every frame that is
    /// a multiple of `period` it steps the gain it drives to
    /// [`driver_value`] (a zero-length ramp addressed to `EdgeGain::ADDR`).
    /// It declares `latency` frames of processing latency it does not
    /// realise, so its events leave late for the PDC tests.
    GainDriver { period: u64, latency: usize },
    /// [`Kind::GainDriver`]'s twin for a node's mix: the same steps doubled
    /// (`-1.0` to `2.0`, off `0..=1` on both sides, for the clamp), as a `Mix`
    /// addressed to `NodeMix::ADDR`.
    MixDriver { period: u64, latency: usize },
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
    /// A spec-driven node for the fixed compile shapes: `dc`/`gain`/`sum`/`fan`
    /// behaviour with a *declared* latency and tail it does not realise.
    Spec {
        behaviour: SpecBehaviour,
        ins: usize,
        outs: usize,
        latency: usize,
        tail: Tail,
    },
    /// `inner`, declaring these buses on its shape (doc 013, "Buses and
    /// coercion between declared layouts"): the node behaves as `inner` and
    /// only its `Shape::buses_in` / `buses_out` differ.
    Bused {
        inner: Box<Kind>,
        buses_in: Box<tutti_graph::Buses>,
        buses_out: Box<tutti_graph::Buses>,
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
    /// What its shape declares to the cost model (`Cost::Heavy`, the
    /// default, unless set with `with_cost`).
    cost: tutti_graph::Cost,
    /// The buses a `Kind::Bused` declares.
    buses: Option<(tutti_graph::Buses, tutti_graph::Buses)>,
}

impl TestNode {
    pub fn new(kind: Kind) -> Self {
        Self::counted(kind, Arc::new(AtomicUsize::new(0)))
    }

    /// A node that counts its `process` calls into `calls`.
    pub fn counted(kind: Kind, calls: Arc<AtomicUsize>) -> Self {
        let (kind, buses) = match kind {
            Kind::Bused {
                inner,
                buses_in,
                buses_out,
            } => (*inner, Some((*buses_in, *buses_out))),
            kind => (kind, None),
        };
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
            cost: tutti_graph::Cost::default(),
            buses,
        }
    }

    /// This node, declaring `cost` (`Shape::cost`).
    pub fn with_cost(mut self, cost: tutti_graph::Cost) -> Self {
        self.cost = cost;
        self
    }
}

impl TestNode {
    fn kind_shape(&self) -> Shape {
        match self.kind {
            Kind::Bused { .. } => unreachable!("`counted` unwraps the buses"),
            Kind::Const { width, .. } => Shape::source_discrete(ch(width)),
            Kind::Gain { width, .. } => Shape::through_discrete(ch(width)).with_in_place(),
            // Any number of inputs, which are not a bus: ports.
            Kind::Sum { inputs } => Shape::EMPTY.with_ports(ch(inputs), ch(1)),
            Kind::Smooth => Shape::through_discrete(ch(1))
                .with_tail(Tail::Finite(Samples(1)))
                .with_in_place(),
            // A pure delay declared as latency: its tail, which excludes
            // latency, is none.
            Kind::Lag { latency } => Shape::through_discrete(ch(1))
                .with_latency(lat(latency))
                .with_tail(Tail::None),
            Kind::Emitter { .. } => Shape::source_discrete(ch(1)).with_events(0, 1),
            Kind::GainDriver { latency, .. } | Kind::MixDriver { latency, .. } => Shape::EMPTY
                .with_events(0, 1)
                .with_latency(lat(latency))
                .with_tail(Tail::None),
            Kind::Burst { cap, .. } => Shape::source_discrete(ch(1))
                .with_events(0, 1)
                .with_event_capacity(cap),
            Kind::Consumer { inputs } => Shape::source_discrete(ch(1))
                .with_events(inputs, 0)
                .with_tail(Tail::Unbounded),
            Kind::EventLag { latency } => Shape::EMPTY
                .with_events(1, 1)
                .with_latency(lat(latency))
                .with_tail(Tail::None),
            Kind::EnvProbe => Shape::source_discrete(ch(1)),
            Kind::Thru { width } => Shape::through_discrete(ch(width)),
            Kind::ThruInPlace { width } => Shape::through_discrete(ch(width)).with_in_place(),
            Kind::Mixed {
                width,
                events_in,
                events_out,
            } => Shape::through_discrete(ch(width))
                .with_events(events_in, events_out)
                .with_tail(Tail::Unbounded)
                .with_in_place(),
            Kind::Spec {
                ins,
                outs,
                latency,
                tail,
                ..
            } => Shape::buses(
                tutti_graph::Buses::discrete(ch(ins)),
                tutti_graph::Buses::discrete(ch(outs)),
            )
            .with_latency(lat(latency))
            .with_tail(tail),
        }
    }
}

/// `shape`'s figures over these buses, its ports kept.
#[allow(dead_code)]
pub fn rebused(shape: Shape, buses_in: tutti_graph::Buses, buses_out: tutti_graph::Buses) -> Shape {
    let mut bused =
        Shape::buses(buses_in, buses_out).with_ports(shape.audio_in(), shape.audio_out());
    bused.event_in = shape.event_in;
    bused.event_out = shape.event_out;
    bused.event_capacity = shape.event_capacity;
    bused.latency = shape.latency;
    bused.tail = shape.tail;
    bused.in_place = shape.in_place;
    bused.event_resolution = shape.event_resolution;
    bused.params = shape.params;
    bused.cost = shape.cost;
    bused.note_events = shape.note_events;
    bused
}

impl Node for TestNode {
    fn shape(&self) -> Shape {
        let shape = self.kind_shape().with_cost(self.cost);
        match self.buses {
            // The kind's figures over the buses the test declared.
            Some((buses_in, buses_out)) => rebused(shape, buses_in, buses_out),
            None => shape,
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
            Kind::Bused { .. } => unreachable!("`counted` unwraps the buses"),
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
                        let _ = io
                            .event_out(0)
                            .push(Event::midi(at, Ump([f as u32, 0, 0, 0])));
                        self.count += 1.0;
                    }
                }
                io.output(0).fill(self.count);
                Status::Modified
            }
            Kind::GainDriver { period, .. } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if f.is_multiple_of(period) {
                        let ramp = tutti_graph::ParamRamp::new(
                            tutti_types::ParamKey::<tutti_types::Amplitude>::VOLUME,
                            tutti_types::Amplitude::new(driver_value(f, period)),
                            Samples(0),
                        );
                        let _ = io.event_out(0).push(Event::ramp(at, ramp));
                    }
                }
                Status::Modified
            }
            Kind::MixDriver { period, .. } => {
                for at in cx.env.offsets() {
                    let f = cx.env.frame_at(at).get();
                    if f.is_multiple_of(period) {
                        let ramp = tutti_graph::ParamRamp::new(
                            tutti_types::ParamKey::<tutti_types::Mix>::WET.at(u8::MAX),
                            tutti_types::Mix::new(driver_value(f, period) * 2.0),
                            Samples(0),
                        );
                        let _ = io.event_out(0).push(Event::ramp(at, ramp));
                    }
                }
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
                            let e = Event::midi(at, Ump([f as u32 + i, 0, 0, 0]));
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
                    let fwd = Event::midi(e.offset, Ump(w));
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
    shapes_costed(kinds, &BTreeMap::new())
}

/// Shapes for every node in `kinds`, those in `costs` declaring that cost.
pub fn shapes_costed(
    kinds: &BTreeMap<NodeKey, Kind>,
    costs: &BTreeMap<NodeKey, tutti_graph::Cost>,
) -> Shapes {
    kinds
        .iter()
        .map(|(&k, kind)| (k, costed(k, kind, costs).shape()))
        .collect()
}

fn costed(k: NodeKey, kind: &Kind, costs: &BTreeMap<NodeKey, tutti_graph::Cost>) -> TestNode {
    let node = TestNode::new(kind.clone());
    match costs.get(&k) {
        Some(&c) => node.with_cost(c),
        None => node,
    }
}

/// A fresh unit for each key in `keys`.
pub fn units_for(
    kinds: &BTreeMap<NodeKey, Kind>,
    keys: impl IntoIterator<Item = NodeKey>,
) -> BTreeMap<NodeKey, Box<dyn Node>> {
    units_costed(kinds, &BTreeMap::new(), keys)
}

/// `units_for`, those in `costs` declaring that cost.
pub fn units_costed(
    kinds: &BTreeMap<NodeKey, Kind>,
    costs: &BTreeMap<NodeKey, tutti_graph::Cost>,
    keys: impl IntoIterator<Item = NodeKey>,
) -> BTreeMap<NodeKey, Box<dyn Node>> {
    keys.into_iter()
        .map(|k| (k, Box::new(costed(k, &kinds[&k], costs)) as Box<dyn Node>))
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
    /// Declared costs (`Shape::cost`) by key; every other node declares
    /// none (`Cost::Heavy`).
    pub costs: BTreeMap<NodeKey, tutti_graph::Cost>,
    /// The graph last switched to, for the editor's spec (`set_mode`).
    graph: Option<ValidGraph>,
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
            costs: BTreeMap::new(),
            graph: None,
        }
    }

    /// As [`new`](Self::new), with the executor running blocks on a shared
    /// [`TestPool`] of `workers` participants (`1`: serially, no pool).
    pub fn with_workers(max_block: usize, workers: usize) -> Self {
        let mut pair = Self::new(max_block);
        if workers > 1 {
            pair.exec.set_pool(Some(test_pool(workers)));
        }
        pair
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
        let mut shapes = shapes_costed(kinds, &self.costs);
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
                        was.generation == now.generation
                            && was.idx == now.idx
                            && was.shape.latency != now.shape.latency
                    })
            })
            .map(|u| tutti_graph::Placement {
                key: u.key,
                generation: u.generation,
                idx: u.idx,
            })
            .collect();
        plan.verify().expect("verifies");
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
        self.graph = Some(graph.clone());
        self.plan = Some(plan.clone());
        // The editor sends each weighted lane's cell from its own spec, and
        // `set_edge_gain` checks the edge against it: with edge gains in
        // play, the editor's spec is the graph's (a harness that compiles
        // itself has otherwise left it empty).
        if !graph.gains().is_empty() || !self.editor.spec_mut().gains.is_empty() {
            let live = self.editor.spec_mut();
            live.topology = graph.topology().clone();
            live.gains = graph.gains().clone();
        }
        // The ranges a unit's cells start with (a node's mix range) are the
        // editor's spec's.
        if !graph.params().is_empty() || !self.editor.spec_mut().params.is_empty() {
            self.editor.spec_mut().params = graph.params().clone();
        }
        // And `set_mode` checks the node against it, and a unit placed now
        // starts in the spec's mode.
        if !graph.modes().is_empty()
            || !self.editor.spec_mut().modes.is_empty()
            || !graph.mixes().is_empty()
            || !self.editor.spec_mut().mixes.is_empty()
        {
            let live = self.editor.spec_mut();
            live.topology = graph.topology().clone();
            live.modes = graph.modes().clone();
            live.mixes = graph.mixes().clone();
        }
        // Back on the control side, where the box and what it retired are
        // freed.
        self.editor
            .package(plan, delta, units_costed(kinds, &self.costs, placed))
            .expect("the harness collects every box");
        self.exec.apply_pending();
        self.editor.collect();

        // The reference decides for itself what is new, from generations; give
        // it a fresh unit for every key it might need.
        let all = units_costed(kinds, &self.costs, graph.topology().nodes.keys().copied());
        self.reference.set_graph_with_fades(graph, all, fades);
    }

    /// Move edge `id`'s weight to `base` in both, as a host moving a fader:
    /// the editor's `set_edge_gain` (a store, no commit) and the reference's.
    pub fn set_edge_gain(&mut self, id: tutti_graph::EdgeId, base: tutti_types::Amplitude) {
        self.editor
            .set_edge_gain(id, base)
            .expect("the edge has a gain entry");
        self.reference.set_edge_gain(id, base);
    }

    /// Move `key`'s mode in both, as a host clicking bypass: the editor's
    /// `set_mode` (a store, no commit) and the reference's.
    pub fn set_mode(&mut self, key: NodeKey, mode: tutti_graph::NodeMode) {
        // `set_mode` checks the node against the editor's spec, which a
        // harness that compiles itself has left empty.
        if !self.editor.spec().topology.nodes.contains_key(&key) {
            let graph = self.graph.as_ref().expect("a graph was switched to");
            self.editor.spec_mut().topology = graph.topology().clone();
        }
        self.editor
            .set_mode(key, mode)
            .expect("the harness's editor holds the graph's nodes");
        self.reference.set_mode(key, mode);
    }

    /// Move `key`'s mix in both, as a host moving a send knob: the editor's
    /// `set_mix` (a store, no commit) and the reference's.
    pub fn set_mix(&mut self, key: NodeKey, mix: tutti_types::Mix) {
        if !self.editor.spec().topology.nodes.contains_key(&key) {
            let graph = self.graph.as_ref().expect("a graph was switched to");
            self.editor.spec_mut().topology = graph.topology().clone();
        }
        self.editor
            .set_mix(key, mix)
            .expect("the harness's editor holds the graph's nodes");
        self.reference.set_mix(key, mix);
    }

    /// Move `key`'s mix range in both, as a host narrowing a send's knob: the
    /// editor's `set_param_range` at the mix's address (a store, no commit) and
    /// the reference's.
    pub fn set_mix_range(&mut self, key: NodeKey, range: tutti_graph::ParamRange) {
        if !self.editor.spec().topology.nodes.contains_key(&key) {
            let graph = self.graph.as_ref().expect("a graph was switched to");
            self.editor.spec_mut().topology = graph.topology().clone();
        }
        let at = tutti_graph::ParamIn {
            node: key,
            param: tutti_graph::NodeMix::ADDR,
        };
        self.editor
            .set_param_range(at, range)
            .expect("the harness's editor holds the graph's nodes");
        self.reference.set_param_range(at, range);
    }

    /// Move edge `id`'s range in both: the editor's `set_edge_gain_range` (a
    /// store, no commit) and the reference's.
    pub fn set_edge_gain_range(&mut self, id: tutti_graph::EdgeId, range: tutti_graph::ParamRange) {
        self.editor
            .set_edge_gain_range(id, range)
            .expect("the edge has a gain entry");
        self.reference.set_edge_gain_range(id, range);
    }

    /// Render one block through both; return (executor, reference) outputs.
    pub fn block(&mut self, frames: usize, input: &[f32]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let transport = Transport::new(
            true,
            tutti_types::Bpm::of(120.0),
            tutti_types::Beat(0.0),
            None,
        );
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
            self.exec.dropped_event_count(),
            self.reference.dropped_event_count(),
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

/// A pool of `participants - 1` helper threads over `tutti_rt::JobGate`,
/// for tests: helpers spin (yielding) for work and never sleep, so every
/// job gets its helpers promptly and the parallel paths are exercised hard.
/// `WorkerPool` is the production pool; this one never parks.
///
/// With `no_alloc`, every participant runs its share of the job inside
/// `assert_no_alloc::assert_no_alloc` — the helpers included, which a gate
/// around the caller alone would not see (`assert_no_alloc` is per thread).
pub struct TestPool {
    gate: Arc<tutti_rt::JobGate>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    helpers: Vec<std::thread::JoinHandle<()>>,
    no_alloc: bool,
}

impl TestPool {
    pub fn new(participants: usize) -> Arc<Self> {
        Self::build(participants, false)
    }

    /// See the type docs.
    pub fn no_alloc(participants: usize) -> Arc<Self> {
        Self::build(participants, true)
    }

    /// A pool whose gate holds `waits`, which it reports to the executor.
    pub fn with_waits(participants: usize, waits: tutti_graph::BlockWaits) -> Arc<Self> {
        Self::build_with(participants, false, waits)
    }

    fn build(participants: usize, no_alloc: bool) -> Arc<Self> {
        Self::build_with(participants, no_alloc, tutti_graph::BlockWaits::DEFAULT)
    }

    fn build_with(
        participants: usize,
        no_alloc: bool,
        waits: tutti_graph::BlockWaits,
    ) -> Arc<Self> {
        let gate = Arc::new(tutti_rt::JobGate::with_waits(participants, waits));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let helpers = (1..participants)
            .map(|_| {
                let (gate, stop) = (Arc::clone(&gate), Arc::clone(&stop));
                std::thread::spawn(move || {
                    let mut served = 0;
                    while !stop.load(Ordering::Relaxed) {
                        if !gate.help(&mut served) {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        Arc::new(Self {
            gate,
            stop,
            helpers,
            no_alloc,
        })
    }
}

impl tutti_graph::Pool for TestPool {
    fn participants(&self) -> usize {
        self.gate.participants()
    }

    fn run(&self, _wake: usize, job: &(dyn Fn(usize) + Sync)) {
        if self.no_alloc {
            self.gate
                .run(&|i| assert_no_alloc::assert_no_alloc(|| job(i)), || {});
        } else {
            self.gate.run(job, || {});
        }
    }

    fn waits(&self) -> tutti_graph::BlockWaits {
        self.gate.waits()
    }
}

impl Drop for TestPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.helpers.drain(..) {
            let _ = h.join();
        }
    }
}

/// A shared [`TestPool`] per participant count, for a whole test binary.
pub fn test_pool(participants: usize) -> Arc<dyn tutti_graph::Pool> {
    use std::sync::{Mutex, OnceLock};
    static POOLS: OnceLock<Mutex<BTreeMap<usize, Arc<TestPool>>>> = OnceLock::new();
    let pools = POOLS.get_or_init(|| Mutex::new(BTreeMap::new()));

    (Arc::clone(
        pools
            .lock()
            .expect("pool map")
            .entry(participants)
            .or_insert_with(|| TestPool::new(participants)),
    )) as _
}

/// The worker counts the parallel suites cover: two, three (odd, so a
/// block's tasks rarely split evenly), four and eight (more participants
/// than a small graph has tasks).
pub const WORKERS: [usize; 4] = [2, 3, 4, 8];

/// A participant count from `WORKERS` for a proptest `seed`.
pub fn workers_for(seed: u64) -> usize {
    WORKERS[(seed.rotate_left(17) % WORKERS.len() as u64) as usize]
}

/// A source that is `1.0` for its first `len` frames, then silent for good
/// (and says so, `Status::Silent`).
pub struct Pulse {
    pub len: usize,
    pub at: usize,
}

impl Pulse {
    pub fn new(len: usize) -> Self {
        Self { len, at: 0 }
    }
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::source_discrete(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.output(0);
        let mut loud = false;
        for o in out.iter_mut() {
            *o = if self.at < self.len { 1.0 } else { 0.0 };
            loud |= *o != 0.0;
            self.at += 1;
        }
        if loud {
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

/// What a [`Ringer`] reports each block.
pub const NO_REPORT: u64 = u64::MAX;
/// A [`Ringer`] report of `Tail::Unbounded`.
pub const REPORT_UNBOUNDED: u64 = u64::MAX - 1;
/// A [`Ringer`] report of `Tail::Unknown`.
pub const REPORT_UNKNOWN: u64 = u64::MAX - 2;

/// Passes its input and adds `0.25` for `ring - 1` frames after the last
/// non-silent input frame (that frame itself is the first of the `ring`), so
/// its last non-zero output frame is `ring - 1` past the input's last. It
/// rings `ring` frames, which is the honest report. Declares `ceiling` and
/// reports what `report` holds: a frame count, or one of the `REPORT_*`
/// words.
pub struct Ringer {
    pub ceiling: Tail,
    pub ring: usize,
    pub report: Arc<std::sync::atomic::AtomicU64>,
    pub calls: Arc<AtomicUsize>,
    pub latency: Samples,
    /// Audio channels in and out; every channel rings together.
    pub width: usize,
    /// Declares an (unused) event output port: the general form.
    pub event_out: bool,
    left: usize,
}

impl Ringer {
    pub fn new(ceiling: Tail, ring: usize, report: u64) -> Self {
        Self {
            ceiling,
            ring,
            report: Arc::new(std::sync::atomic::AtomicU64::new(report)),
            calls: Arc::new(AtomicUsize::new(0)),
            latency: Samples::ZERO,
            width: 1,
            event_out: false,
            left: 0,
        }
    }

    /// This node, declaring `latency`.
    pub fn with_latency(mut self, latency: Samples) -> Self {
        self.latency = latency;
        self
    }

    /// This node, `width` channels wide (2: the `Direct` form).
    pub fn wide(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// This node, with an event output port (the general form).
    pub fn with_event_out(mut self) -> Self {
        self.event_out = true;
        self
    }

    /// An honest reporter: the whole ring-out, under `ceiling`.
    pub fn honest(ceiling: Tail, ring: usize) -> Self {
        Self::new(ceiling, ring, ring as u64)
    }
}

impl Node for Ringer {
    fn shape(&self) -> Shape {
        let ch = ChannelLayout::try_from(self.width).expect("a width");
        let shape = Shape::through_discrete(ch)
            .with_latency(Latency::new(self.latency))
            .with_tail(self.ceiling);
        if self.event_out {
            shape.with_events(0, 1)
        } else {
            shape
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut loud = false;
        {
            let (ins, mut outs) = io.split();
            for i in 0..ins.get(0).len() {
                if (0..self.width).any(|c| ins.get(c)[i] != 0.0) {
                    self.left = self.ring;
                }
                let ringing = if self.left > 0 {
                    self.left -= 1;
                    0.25
                } else {
                    0.0
                };
                for c in 0..self.width {
                    let o = ins.get(c)[i] + ringing;
                    outs.get(c)[i] = o;
                    loud |= o != 0.0;
                }
            }
        }
        match self.report.load(Ordering::Relaxed) {
            NO_REPORT => {}
            REPORT_UNBOUNDED => io.report_tail(Tail::Unbounded),
            REPORT_UNKNOWN => io.report_tail(Tail::Unknown),
            0 => io.report_tail(Tail::None),
            n => io.report_tail(Tail::Finite(Samples(n as usize))),
        }
        if loud {
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {
        self.left = 0;
    }
}
