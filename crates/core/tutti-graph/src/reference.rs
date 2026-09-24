//! The reference interpreter: the oracle the plan executor is checked against.
//!
//! Doc 013 §3: "a naive reference interpreter (serial, copy every edge, no
//! colouring) and the optimized executor must be bit-identical on
//! proptest-generated topologies". Everything here is written to be *obviously*
//! right rather than fast, and — the point — **independently** of the compiler:
//! it does not call `compile`, read a `Plan`, or share the executor's kernels.
//! It re-derives the evaluation order (by repeated scanning), the latency solve
//! (by memoised recursion), the delays (with `VecDeque`s), the event fan-in
//! (concatenate, then a *stable* sort by offset) and feedback (a map of last
//! block's port values) from the spec alone. Where the two agree, it is
//! because two different programs computed the same thing.
//!
//! It never skips a node, never aliases a buffer, and allocates freely. It
//! passes nodes no silence or constant hints, so a node that behaves
//! differently on a hint than without one is caught too.
//!
//! # Semantics it pins
//!
//! - **Delay keys are (sink, source)** (see [`DelayKey`]). A PDC delay whose
//!   length changes across a recompile keeps its most recent `min(old, new)`
//!   inputs; a new one starts silent; an audio line whose key disappears is
//!   dropped — a rewired sink never hears its old source's past.
//! - An event delay keeps its pending events with their input times and
//!   reschedules them on a retune, delivering at offset 0 anything already
//!   past due. **When its key disappears its pending events are flushed** to
//!   the sink's next call if the sink survives (with the sink, they go):
//!   each flushed delay keeps its events' spacing, shifted so its earliest
//!   lands at offset 0, and anything that falls past the block is clamped to
//!   its last frame. They sort ahead of that block's own events on ties.
//!   Flushes into one sink queue in this order: undelivered earlier flushes,
//!   event delays in key order, event feedback in key order. A flushed event
//!   can reach a *replaced* unit at that sink — a note-off whose note-on the
//!   old unit saw — which a unit must tolerate.
//! - **A global input is delayed** to its node's arrival like any other
//!   merge-point source.
//! - **A feedback edge delays by exactly the `delay` its edge declares**,
//!   whatever the current block length (see [`FeedbackKey`]): audio keeps the
//!   last `delay` samples of its source port per (port, unit generation,
//!   delay); events keep a FIFO per (sink, source, generation, delay),
//!   flushed like a delay when the key disappears.
//! - Blocks run under a flush-to-zero guard, as the executor's do, so the two
//!   agree about denormals too.
//!
//! [`FeedbackKey`]: crate::FeedbackKey

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::latency::MAX_NODE_LATENCY;
use tutti_types::{At, Frame, Latency, NodeKey, Samples, ScopedNoDenormals};

use crate::event::{Event, EventKind, EventWriter, SortedEvents};
use crate::io::Io;
use crate::node::{
    ConstantMask, Cx, Env, InPlaceMask, Node, Prepare, SilenceMask, Status, Transport,
};
use crate::plan::DelayKey;
use crate::spec::{EventEdge, EventIn, EventOut, ValidGraph};
use crate::time::{Due, Offset};

struct RefFifo {
    pending: Vec<(u64, Event)>,
    len: u64,
    clock: u64,
}

/// The naive interpreter. See the `reference` module's docs (`src/reference.rs`).
pub struct Reference {
    prepare: Prepare,
    graph: Option<ValidGraph>,
    units: BTreeMap<NodeKey, (u32, Box<dyn Node>)>,
    arrival: BTreeMap<NodeKey, Latency>,
    delays: BTreeMap<DelayKey, Samples>,
    audio_lines: BTreeMap<DelayKey, VecDeque<f32>>,
    event_lines: BTreeMap<DelayKey, RefFifo>,
    fb_audio: BTreeMap<(OutPort, u32, Samples), VecDeque<f32>>,
    fb_event: BTreeMap<(EventIn, EventOut, u32, Samples), RefFifo>,
    inject: BTreeMap<EventIn, Vec<Event>>,
    /// Scheduled commands not yet due, in scheduling order.
    scheduled: Vec<(At, EventIn, EventKind)>,
    /// This block's scheduled deliveries, per port, in scheduling order.
    landing: BTreeMap<EventIn, Vec<Event>>,
    late: u64,
    unrouted: u64,
    frame: Frame,
}

impl Reference {
    /// An interpreter with no graph.
    pub fn new(prepare: Prepare) -> Self {
        Self {
            prepare,
            graph: None,
            units: BTreeMap::new(),
            arrival: BTreeMap::new(),
            delays: BTreeMap::new(),
            audio_lines: BTreeMap::new(),
            event_lines: BTreeMap::new(),
            fb_audio: BTreeMap::new(),
            fb_event: BTreeMap::new(),
            inject: BTreeMap::new(),
            scheduled: Vec::new(),
            landing: BTreeMap::new(),
            late: 0,
            unrouted: 0,
            frame: Frame::ZERO,
        }
    }

    /// Deliver `kind` into `to` at `at` — the same contract as
    /// `Editor::schedule`, without the queue: a time already past lands at
    /// offset 0 of the next block and is counted late; one whose port is gone
    /// when it falls due is counted unrouted.
    pub fn schedule(&mut self, at: At, to: EventIn, kind: EventKind) {
        self.scheduled.push((at, to, kind));
    }

    /// Scheduled commands that landed late (see `Executor::late_commands`).
    pub fn late_commands(&self) -> u64 {
        self.late
    }

    /// Scheduled commands with nowhere to land.
    pub fn unrouted_commands(&self) -> u64 {
        self.unrouted
    }

    /// The PDC delay the interpreter derived for `key`, or zero — so a test
    /// can compare its latency solve against the compiler's.
    pub fn delay(&self, key: DelayKey) -> Samples {
        self.delays.get(&key).copied().unwrap_or_default()
    }

    /// Switch to `graph`. `fresh` supplies a unit for every node that is new or
    /// whose generation changed; units whose key and generation survive keep
    /// their state.
    ///
    /// # Panics
    ///
    /// If `fresh` lacks a needed unit.
    pub fn set_graph(&mut self, graph: &ValidGraph, mut fresh: BTreeMap<NodeKey, Box<dyn Node>>) {
        let t = graph.topology();
        self.units
            .retain(|k, (gen, _)| t.nodes.contains_key(k) && *gen == graph.generation(*k));
        for &key in t.nodes.keys() {
            if !self.units.contains_key(&key) {
                let mut unit = fresh
                    .remove(&key)
                    .unwrap_or_else(|| panic!("no unit for node {}", key.0));
                unit.prepare(&self.prepare);
                self.units.insert(key, (graph.generation(key), unit));
            }
        }

        // Latency, by memoised recursion over direct predecessors.
        let lat = |k: NodeKey| {
            self.units[&k]
                .1
                .shape()
                .latency
                .min(Latency::new(MAX_NODE_LATENCY))
        };
        let mut arrival: BTreeMap<NodeKey, Latency> = BTreeMap::new();
        fn arrive(
            k: NodeKey,
            g: &ValidGraph,
            lat: &dyn Fn(NodeKey) -> Latency,
            memo: &mut BTreeMap<NodeKey, Latency>,
        ) -> Latency {
            if let Some(&a) = memo.get(&k) {
                return a;
            }
            let mut best = Latency::ZERO;
            for (at, e) in &g.topology().edges {
                if at.node == k {
                    if let Edge::Direct(Source::Node(p)) = e {
                        best = best.max(arrive(p.node, g, lat, memo) + lat(p.node));
                    }
                }
            }
            for (at, sources) in g.events() {
                if at.node == k {
                    for e in sources {
                        if let EventEdge::Direct(from) = e {
                            best = best.max(arrive(from.node, g, lat, memo) + lat(from.node));
                        }
                    }
                }
            }
            memo.insert(k, best);
            best
        }
        for &k in t.nodes.keys() {
            arrive(k, graph, &lat, &mut arrival);
        }
        let departure = |k: NodeKey| arrival[&k] + lat(k);

        let mut delays: BTreeMap<DelayKey, Samples> = BTreeMap::new();
        for (&at, e) in &t.edges {
            let dep = match *e {
                Edge::Direct(Source::Node(p)) => departure(p.node).samples(),
                Edge::Direct(Source::Global(_)) => Samples::ZERO,
                _ => continue,
            };
            let Edge::Direct(from) = *e else { continue };
            // Written as a subtraction on the raw counts, not with
            // `Latency::gap_to` the compiler uses: two spellings of one
            // rule, so a slip in either shows up as a divergence.
            let d = arrival[&at.node]
                .samples()
                .checked_sub(dep)
                .unwrap_or_default();
            if !d.is_zero() {
                delays.insert(DelayKey::Audio { at, from }, d);
            }
        }
        for (&at, sources) in graph.events() {
            for e in sources {
                if let EventEdge::Direct(from) = *e {
                    let d = arrival[&at.node]
                        .samples()
                        .checked_sub(departure(from.node).samples())
                        .unwrap_or_default();
                    if !d.is_zero() {
                        delays.insert(DelayKey::Event { at, from }, d);
                    }
                }
            }
        }
        let arrivals: Vec<Samples> = t
            .outputs
            .iter()
            .map(|s| match s {
                Source::Node(p) => departure(p.node).samples(),
                _ => Samples::ZERO,
            })
            .collect();
        let total = arrivals.iter().copied().max().unwrap_or_default();
        for (ch, (s, a)) in t.outputs.iter().zip(&arrivals).enumerate() {
            let d = total.checked_sub(*a).unwrap_or_default();
            if !d.is_zero() && *s != Source::Zero {
                delays.insert(
                    DelayKey::Output {
                        channel: ch as u16,
                        from: *s,
                    },
                    d,
                );
            }
        }

        // Flush first, while the old state is intact. A sink survives when
        // its node is still here with that event input.
        let survives = |at: &EventIn| {
            self.units
                .get(&at.node)
                .is_some_and(|(_, u)| at.port < u.shape().event_in)
        };
        let fb_event_keys: BTreeSet<(EventIn, EventOut, u32, Samples)> = graph
            .events()
            .iter()
            .flat_map(|(&at, v)| {
                v.iter().filter_map(move |e| match *e {
                    EventEdge::Feedback { from, delay } => {
                        Some((at, from, graph.generation(from.node), delay))
                    }
                    EventEdge::Direct(_) => None,
                })
            })
            .collect();
        let mut inject = std::mem::take(&mut self.inject);
        inject.retain(|at, _| survives(at));
        let mut flush = |at: EventIn, pending: Vec<Event>| {
            if survives(&at) {
                // Kept in spacing order (stable: flush order on ties) before
                // any clamping, which would erase it.
                let list = inject.entry(at).or_default();
                list.extend(pending);
                list.sort_by_key(|e| e.offset);
            }
        };
        for (k, f) in &self.event_lines {
            if let DelayKey::Event { at, .. } = *k {
                if !delays.contains_key(k) {
                    flush(at, relative(f));
                }
            }
        }
        for (k, f) in &self.fb_event {
            if !fb_event_keys.contains(k) {
                flush(k.0, relative(f));
            }
        }
        self.inject = inject;

        // Delay state: keep by key (retuned), drop the rest, start new ones
        // silent.
        self.audio_lines.retain(|k, _| delays.contains_key(k));
        self.event_lines.retain(|k, _| delays.contains_key(k));
        for (&key, &d) in &delays {
            if matches!(key, DelayKey::Event { .. }) {
                let f = self.event_lines.entry(key).or_insert(RefFifo {
                    pending: Vec::new(),
                    len: 0,
                    clock: 0,
                });
                f.len = d.get() as u64;
            } else {
                let line = self
                    .audio_lines
                    .entry(key)
                    .or_insert_with(|| VecDeque::from(vec![0.0; d.get()]));
                while line.len() > d.get() {
                    line.pop_front();
                }
                while line.len() < d.get() {
                    line.push_front(0.0);
                }
            }
        }

        let audio_fb: BTreeSet<(OutPort, u32, Samples)> = t
            .edges
            .values()
            .filter_map(|e| match e {
                Edge::Feedback(f) => Some((f.from, graph.generation(f.from.node), f.delay)),
                _ => None,
            })
            .collect();
        self.fb_audio.retain(|k, _| audio_fb.contains(k));
        for k in audio_fb {
            self.fb_audio
                .entry(k)
                .or_insert_with(|| VecDeque::from(vec![0.0; k.2.get()]));
        }
        self.fb_event.retain(|k, _| fb_event_keys.contains(k));
        for k in fb_event_keys {
            self.fb_event.entry(k).or_insert(RefFifo {
                pending: Vec::new(),
                len: k.3.get() as u64,
                clock: 0,
            });
        }

        self.arrival = arrival;
        self.delays = delays;
        self.graph = Some(graph.clone());
    }

    /// Render one block. Same contract as `Executor::process`.
    pub fn process(
        &mut self,
        frames: usize,
        transport: &Transport,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        let Some(graph) = self.graph.clone() else {
            for o in outputs.iter_mut() {
                o[..frames].fill(0.0);
            }
            self.frame += Samples(frames);
            return;
        };
        let t = graph.topology();
        let _ftz = ScopedNoDenormals::new();
        let env = Env {
            frame: self.frame,
            sample_rate: self.prepare.sample_rate(),
            block_len: Samples(frames),
            transport: *transport,
        };

        // Scheduled commands: which land this block, and where. Kept in
        // scheduling order per port; the gather below appends them after the
        // port's own events.
        self.landing.clear();
        let mut waiting = Vec::new();
        for (at, to, kind) in std::mem::take(&mut self.scheduled) {
            let offset = match env.due(at) {
                Due::NotYet => {
                    waiting.push((at, to, kind));
                    continue;
                }
                Due::In(o) => o,
                Due::Late => {
                    self.late += 1;
                    Offset::ZERO
                }
            };
            let has_port = self
                .units
                .get(&to.node)
                .is_some_and(|(_, u)| to.port < u.shape().event_in);
            if has_port {
                self.landing
                    .entry(to)
                    .or_default()
                    .push(Event { offset, kind });
            } else {
                self.unrouted += 1;
            }
        }
        self.scheduled = waiting;

        let mut audio: BTreeMap<OutPort, Vec<f32>> = BTreeMap::new();
        let mut events: BTreeMap<EventOut, Vec<Event>> = BTreeMap::new();
        let mut done: BTreeSet<NodeKey> = BTreeSet::new();

        let direct_preds = |k: NodeKey| -> Vec<NodeKey> {
            let mut v: Vec<NodeKey> = t
                .edges
                .iter()
                .filter(|(at, _)| at.node == k)
                .filter_map(|(_, e)| match e {
                    Edge::Direct(Source::Node(p)) => Some(p.node),
                    _ => None,
                })
                .collect();
            for (at, sources) in graph.events() {
                if at.node == k {
                    v.extend(sources.iter().filter_map(|e| match e {
                        EventEdge::Direct(f) => Some(f.node),
                        EventEdge::Feedback { .. } => None,
                    }));
                }
            }
            v
        };

        while done.len() < t.nodes.len() {
            let before = done.len();
            for &key in t.nodes.keys() {
                if done.contains(&key) || !direct_preds(key).iter().all(|p| done.contains(p)) {
                    continue;
                }
                self.run_node(key, &graph, frames, &env, inputs, &mut audio, &mut events);
                done.insert(key);
            }
            assert!(
                done.len() > before,
                "reference interpreter: graph has a cycle"
            );
        }

        for (ch, source) in t.outputs.iter().enumerate() {
            let mut buf = match source {
                Source::Node(p) => audio[p].clone(),
                Source::Global(g) => inputs[*g as usize][..frames].to_vec(),
                Source::Zero => vec![0.0; frames],
            };
            let key = DelayKey::Output {
                channel: ch as u16,
                from: *source,
            };
            if let Some(line) = self.audio_lines.get_mut(&key) {
                delay_line(line, &mut buf);
            }
            outputs[ch][..frames].copy_from_slice(&buf);
        }

        // Feed the feedback delays with this block, after every read of them.
        for ((from, _, _), line) in self.fb_audio.iter_mut() {
            for &x in &audio[from] {
                line.push_back(x);
                line.pop_front();
            }
        }
        for ((_, from, _, _), f) in self.fb_event.iter_mut() {
            let start = f.clock;
            f.pending.extend(
                events[from]
                    .iter()
                    .map(|e| (start + u64::from(e.offset.get()), *e)),
            );
            f.clock += frames as u64;
        }
        self.frame += Samples(frames);
    }

    #[allow(clippy::too_many_arguments)]
    fn run_node(
        &mut self,
        key: NodeKey,
        graph: &ValidGraph,
        frames: usize,
        env: &Env,
        inputs: &[&[f32]],
        audio: &mut BTreeMap<OutPort, Vec<f32>>,
        events: &mut BTreeMap<EventOut, Vec<Event>>,
    ) {
        let t = graph.topology();
        let shape = self.units[&key].1.shape();

        let mut ins: Vec<Vec<f32>> = Vec::new();
        for port in 0..shape.audio_in.count() {
            let at = InPort { node: key, port };
            let mut buf = match t.edges.get(&at) {
                None | Some(Edge::Direct(Source::Zero)) => vec![0.0; frames],
                Some(Edge::Direct(Source::Global(g))) => inputs[*g as usize][..frames].to_vec(),
                Some(Edge::Direct(Source::Node(p))) => audio[p].clone(),
                Some(Edge::Feedback(f)) => {
                    let line = &self.fb_audio[&(f.from, graph.generation(f.from.node), f.delay)];
                    line.iter().take(frames).copied().collect()
                }
            };
            if let Some(&Edge::Direct(from)) = t.edges.get(&at) {
                if let Some(line) = self.audio_lines.get_mut(&DelayKey::Audio { at, from }) {
                    delay_line(line, &mut buf);
                }
            }
            ins.push(buf);
        }

        let mut ev_ins: Vec<Vec<Event>> = Vec::new();
        for port in 0..shape.event_in {
            let at = EventIn { node: key, port };
            // Flushed events first: they are older than anything this block.
            let mut all: Vec<Event> = self.inject.remove(&at).unwrap_or_default();
            for e in &mut all {
                e.offset = e.offset.clamp_to(frames);
            }
            for e in graph.events().get(&at).map(Vec::as_slice).unwrap_or(&[]) {
                match *e {
                    EventEdge::Direct(from) => {
                        let src = events[&from].clone();
                        match self.event_lines.get_mut(&DelayKey::Event { at, from }) {
                            Some(f) => all.extend(fifo_run(f, &src, frames)),
                            None => all.extend(src),
                        }
                    }
                    EventEdge::Feedback { from, delay } => {
                        let f = self
                            .fb_event
                            .get_mut(&(at, from, graph.generation(from.node), delay))
                            .expect("built by set_graph");
                        all.extend(fifo_due(f, frames));
                    }
                }
            }
            // Stable: equal offsets keep source order.
            // Scheduled commands: one more source, after the edges.
            all.extend(self.landing.remove(&at).unwrap_or_default());
            all.sort_by_key(|e| e.offset);
            ev_ins.push(all);
        }

        let n_out = shape.audio_out.count() as usize;
        let mut outs: Vec<Vec<f32>> = vec![vec![0.0; frames]; n_out];
        let mut ev_bufs: Vec<Vec<Event>> = vec![Vec::new(); shape.event_out as usize];
        let status = {
            let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
            let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
            let ev_refs: Vec<SortedEvents<'_>> = ev_ins
                .iter()
                .map(|v| SortedEvents::new(v, frames).expect("the reference sorted them"))
                .collect();
            let drops = Cell::new(0);
            let mut writers: Vec<EventWriter<'_>> = ev_bufs
                .iter_mut()
                .map(|b| EventWriter::new(b, usize::MAX, frames as u32, &drops))
                .collect();
            let cx = Cx {
                env,
                arrival: self.arrival[&key],
            };
            let io = Io::new(
                self.prepare.max_block(),
                frames,
                &in_refs,
                &mut out_refs,
                SilenceMask::NONE,
                ConstantMask::NONE,
                InPlaceMask::NONE,
                &ev_refs,
                &mut writers,
            );
            self.units
                .get_mut(&key)
                .expect("unit present")
                .1
                .process(&cx, io)
        };
        match status {
            Status::Modified | Status::Masked { .. } => {}
            Status::Silent | Status::Idle => outs.iter_mut().for_each(|o| o.fill(0.0)),
            Status::Constant => outs.iter_mut().for_each(|o| {
                let v = o[0];
                o.fill(v);
            }),
            Status::Bypass => {
                for (c, o) in outs.iter_mut().enumerate() {
                    match ins.get(c) {
                        Some(i) => o.copy_from_slice(i),
                        None => o.fill(0.0),
                    }
                }
            }
        }
        for (port, o) in outs.into_iter().enumerate() {
            audio.insert(
                OutPort {
                    node: key,
                    port: port as u16,
                },
                o,
            );
        }
        for (port, e) in ev_bufs.into_iter().enumerate() {
            events.insert(
                EventOut {
                    node: key,
                    port: port as u16,
                },
                e,
            );
        }
    }
}

/// Push every sample through the line: in at the back, out at the front.
fn delay_line(line: &mut VecDeque<f32>, buf: &mut [f32]) {
    for x in buf.iter_mut() {
        line.push_back(*x);
        *x = line.pop_front().expect("line is non-empty");
    }
}

/// A vanished FIFO's events for a flush: due times relative to the
/// earliest, so their spacing survives.
fn relative(f: &RefFifo) -> Vec<Event> {
    let Some(&(first, _)) = f.pending.first() else {
        return Vec::new();
    };
    f.pending
        .iter()
        .map(|&(t, e)| Event {
            offset: Offset::raw((t - first).min(u64::from(u32::MAX)) as u32),
            ..e
        })
        .collect()
}

/// Take what falls due in the block starting at the FIFO's clock, without
/// queueing or advancing (a feedback read).
fn fifo_due(f: &mut RefFifo, frames: usize) -> Vec<Event> {
    let start = f.clock;
    let end = start + frames as u64;
    let due = f
        .pending
        .iter()
        .take_while(|(t, _)| t + f.len < end)
        .count();
    f.pending
        .drain(..due)
        .map(|(t, e)| Event {
            offset: Offset::raw((t + f.len).saturating_sub(start) as u32),
            ..e
        })
        .collect()
}

fn fifo_run(f: &mut RefFifo, input: &[Event], frames: usize) -> Vec<Event> {
    let start = f.clock;
    let end = start + frames as u64;
    f.pending.extend(
        input
            .iter()
            .map(|e| (start + u64::from(e.offset.get()), *e)),
    );
    let due = f
        .pending
        .iter()
        .take_while(|(t, _)| t + f.len < end)
        .count();
    let out = f
        .pending
        .drain(..due)
        .map(|(t, e)| Event {
            offset: Offset::raw((t + f.len).saturating_sub(start) as u32),
            ..e
        })
        .collect();
    f.clock = end;
    out
}
