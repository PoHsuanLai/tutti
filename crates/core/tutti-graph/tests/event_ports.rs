//! Events as ports (doc 013, "Rewrite order", item 5): the graph-side
//! properties a MIDI or automation node will rely on once it is ported, each
//! checked on the executor **and** the reference interpreter, so the two
//! cannot share one wrong decision unseen.
//!
//! - **Same-block delivery**: an event edge joins the one topological order,
//!   so what a node emits reaches its downstream node in the block it was
//!   emitted in, on its own offset (arp → synth, zero latency).
//! - **Fan-in order**: an event input with several sources merges them by
//!   offset, ties going to the source port with the lower `(NodeKey, port)`,
//!   whatever order the spec lists the edges in.
//! - **PDC shifts events**: an event edge into a node whose arrival a latent
//!   sibling raised is delayed by exactly the gap, across block boundaries
//!   and under ragged blocks, and nothing is dropped.
//! - **Feedback latency**: an event feedback edge delays by exactly its
//!   declared delay, and only a feedback edge adds latency.
//! - **Declared capacity** (`Shape::event_capacity`): a port past its
//!   declaration drops the newest events and counts them; everything
//!   downstream (merges, PDC FIFOs) is sized from the declarations, so a node
//!   that keeps to its own loses nothing — even when the executor's default
//!   is far smaller.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tutti_graph::{
    compile, Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, EventSlotCapacity,
    GraphSpec, Io, Node, Op, Prepare, Reference, Shape, Shapes, Status, Transport, Ump, ValidGraph,
};
use tutti_types::graph::NodeSpec;
use tutti_types::{ChannelLayout, Frame, Latency, NodeKey, SampleRate, Samples, Tail, Topology};

/// What a sink saw: `(block start frame, offset, port, tag)`.
type Seen = Arc<Mutex<Vec<(u64, u32, u16, u32)>>>;

/// Emits each planned `(frame, port, tag)` on its frame, on `ports` event
/// outputs, declaring `cap` (or the default). Counts the pushes its writers
/// refused.
struct Burst {
    ports: u16,
    cap: Option<u32>,
    plan: Vec<(u64, u16, u32)>,
    refused: Arc<AtomicUsize>,
}

impl Node for Burst {
    fn shape(&self) -> Shape {
        let s = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, self.ports);
        match self.cap {
            Some(c) => s.with_event_capacity(c),
            None => s,
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for &(f, port, tag) in &self.plan {
            if let Some(at) = cx.env.offset_of(Frame(f)) {
                if io
                    .event_out(port as usize)
                    .push(Event::midi(at, [tag, 0, 0, 0]))
                    .is_err()
                {
                    self.refused.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Forwards every input event on its own offset, tag + 1000 — an
/// arpeggiator's shape, with no latency of its own.
struct Relay;

impl Node for Relay {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for e in io.events(0) {
            let EventKind::Midi(Ump(mut w)) = e.kind else {
                continue;
            };
            w[0] += 1000;
            io.event_out(0)
                .push(Event::midi(e.offset, w))
                .expect("forwards what it was handed");
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Declares `latency` and an event output it never writes: wired into a
/// sink, it raises the sink's arrival, so the sink's other event edges get
/// PDC delays.
struct Late {
    latency: usize,
}

impl Node for Late {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_latency(Latency::new(Samples(self.latency)))
            .with_tail(Tail::Finite(Samples(self.latency)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Logs every event on each of its event inputs.
struct Sink {
    ports: u16,
    seen: Seen,
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for p in 0..io.event_input_count() {
            for e in io.events(p) {
                let EventKind::Midi(Ump(w)) = e.kind else {
                    panic!("only MIDI is sent")
                };
                seen.push((cx.env.frame.get(), e.offset.get(), p as u16, w[0]));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

type Make = Box<dyn Fn() -> Box<dyn Node>>;

/// A graph for both interpreters: node constructors by key, and the event
/// edges exactly as the test lists them (not through `connect_events`, so a
/// test can list sources out of order).
struct Rig {
    nodes: Vec<(NodeKey, Make)>,
    events: BTreeMap<EventIn, Vec<EventEdge>>,
}

impl Rig {
    fn spec(&self) -> (ValidGraph, Shapes) {
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        for (k, make) in &self.nodes {
            let s = make().shape();
            t.nodes.insert(
                *k,
                NodeSpec::new("n", s.audio_in, s.audio_out)
                    .with_latency(s.latency.samples())
                    .with_tail(s.tail),
            );
            shapes.insert(*k, s);
        }
        let mut g = GraphSpec::new(t);
        g.events = self.events.clone();
        (g.validate().expect("valid"), shapes)
    }

    fn units(&self) -> BTreeMap<NodeKey, Box<dyn Node>> {
        self.nodes.iter().map(|(k, make)| (*k, make())).collect()
    }

    /// Render `blocks` through the executor (with `default_cap` as its
    /// default event capacity) or the reference. Returns what the sinks saw
    /// and how many events the interpreter refused.
    fn run(
        &self,
        executor: bool,
        max: usize,
        default_cap: usize,
        blocks: &[usize],
        seen: &Seen,
    ) -> (Vec<(u64, u32, u16, u32)>, u64) {
        seen.lock().unwrap().clear();
        let prep = Prepare::new(SampleRate(48_000.0), Samples(max));
        let (valid, shapes) = self.spec();
        let transport = Transport::default();
        let dropped = if executor {
            let (mut ed, mut exec) = Editor::with_event_capacity(prep, default_cap);
            let (plan, delta) = compile(&valid, &shapes, &prep, None).expect("compiles");
            ed.package(plan, delta, self.units()).expect("room");
            exec.apply_pending();
            ed.collect();
            for &n in blocks {
                exec.process(n, &transport, &[], &mut []);
            }
            exec.dropped_events()
        } else {
            let mut r = Reference::new(prep);
            r.set_graph(&valid, self.units());
            for &n in blocks {
                r.process(n, &transport, &[], &mut []);
            }
            r.dropped_events()
        };
        (seen.lock().unwrap().clone(), dropped)
    }
}

fn at(node: u64, port: u16) -> EventIn {
    EventIn {
        node: NodeKey(node),
        port,
    }
}

fn out(node: u64, port: u16) -> EventOut {
    EventOut {
        node: NodeKey(node),
        port,
    }
}

fn burst(
    ports: u16,
    cap: Option<u32>,
    plan: &[(u64, u16, u32)],
    refused: &Arc<AtomicUsize>,
) -> Make {
    let (plan, refused) = (plan.to_vec(), Arc::clone(refused));
    Box::new(move || {
        Box::new(Burst {
            ports,
            cap,
            plan: plan.clone(),
            refused: Arc::clone(&refused),
        })
    })
}

fn sink(ports: u16, seen: &Seen) -> Make {
    let seen = Arc::clone(seen);
    Box::new(move || {
        Box::new(Sink {
            ports,
            seen: Arc::clone(&seen),
        })
    })
}

/// `(block start, offset)` of absolute frame `f` under `blocks`.
fn place(blocks: &[usize], f: u64) -> (u64, u32) {
    let mut start = 0u64;
    for &n in blocks {
        if f < start + n as u64 {
            return (start, (f - start) as u32);
        }
        start += n as u64;
    }
    panic!("frame {f} is past the render");
}

/// Blocks of 1, 63, 64, 65 and a few odd lengths, repeated to cover `total`.
fn ragged(total: u64, max: usize) -> Vec<usize> {
    let pattern = [1usize, 63, 64, 65, 7, max, 100, 3];
    let mut v = Vec::new();
    let mut done = 0u64;
    let mut i = 0;
    while done < total {
        let n = pattern[i % pattern.len()].min(max);
        v.push(n);
        done += n as u64;
        i += 1;
    }
    v
}

/// Arp → synth: an event a node emits reaches the node downstream **in the
/// block it was emitted in**, on its own offset — the relay's output too, one
/// hop further. Keys run against the data flow (sink 1, relay 2, source 3),
/// so an order taken from the keys rather than the event edges cannot pass.
/// The plan has no event delay: nothing on the path is latent.
///
/// Mutation (run): in `compile`, leave event edges out of the ordering
/// dependencies (`preds`) → the key order runs the sink first and the
/// compile of a read before its write panics → fails. Mutation (run): in
/// the reference, leave event edges out of `direct_preds` → it runs the
/// sink before its source and panics on the missing output → fails.
#[test]
fn an_event_reaches_downstream_in_the_block_it_was_emitted() {
    let seen: Seen = Arc::default();
    let refused = Arc::new(AtomicUsize::new(0));
    let frames = [0u64, 5, 63, 64 + 17, 200, 255];
    let plan: Vec<(u64, u16, u32)> = frames.iter().map(|&f| (f, 0, f as u32)).collect();
    let mut events = BTreeMap::new();
    events.insert(at(2, 0), vec![EventEdge::Direct(out(3, 0))]);
    events.insert(at(1, 0), vec![EventEdge::Direct(out(2, 0))]);
    let rig = Rig {
        nodes: vec![
            (NodeKey(1), sink(1, &seen)),
            (NodeKey(2), Box::new(|| Box::new(Relay) as Box<dyn Node>)),
            (NodeKey(3), burst(1, None, &plan, &refused)),
        ],
        events,
    };
    let (valid, shapes) = rig.spec();
    let prep = Prepare::new(SampleRate(48_000.0), Samples(64));
    let (p, _) = compile(&valid, &shapes, &prep, None).expect("compiles");
    assert!(
        !p.ops().iter().any(|op| matches!(op, Op::EventDelay { .. })),
        "nothing latent, so no event delay"
    );
    assert_eq!(p.unit(NodeKey(1)).expect("sink").arrival, Latency::ZERO);
    let blocks = vec![64usize; 5];
    let want: Vec<(u64, u32, u16, u32)> = frames
        .iter()
        .map(|&f| {
            let (b, o) = place(&blocks, f);
            (b, o, 0, f as u32 + 1000)
        })
        .collect();
    for executor in [true, false] {
        let (got, dropped) = rig.run(executor, 64, 64, &blocks, &seen);
        assert_eq!(got, want, "executor: {executor}");
        assert_eq!(dropped, 0);
    }
}

/// Fan-in: several sources on one event input merge by offset, and a tie
/// goes to the source with the lower `(NodeKey, port)` — node 5's port 0,
/// then its port 1, then node 9 — although the spec lists them 9, 5:1, 5:0.
/// An earlier offset still comes first whatever its source. Every offset of
/// a 64-frame block is tried for the tie, so block-edge offsets are covered.
///
/// Mutation (run): drop the `sort_by_key(|e| e.from())` in `compile` → the
/// listed order (9 first) → fails on the executor. Mutation (run): in the
/// reference, iterate the listed edges instead of re-keying them by source
/// → fails on the reference. Mutation (run): in `merge_into`, `<` → `<=`
/// (ties to the later source) → fails.
#[test]
fn fan_in_ties_go_by_source_key_then_port() {
    for k in [0u64, 1, 31, 62, 63] {
        let seen: Seen = Arc::default();
        let refused = Arc::new(AtomicUsize::new(0));
        let tie = 64 + k;
        let five = [(tie, 1, 51), (tie, 0, 50)];
        let mut five = five.to_vec();
        five.sort();
        // Node 9 also sends an event a frame *before* the tie (when there is
        // one in the block): it must come first despite the higher key.
        let mut nine = vec![(tie, 0, 90)];
        if k > 0 {
            nine.insert(0, (tie - 1, 0, 89));
        }
        let mut events = BTreeMap::new();
        events.insert(
            at(1, 0),
            vec![
                EventEdge::Direct(out(9, 0)),
                EventEdge::Direct(out(5, 1)),
                EventEdge::Direct(out(5, 0)),
            ],
        );
        let rig = Rig {
            nodes: vec![
                (NodeKey(1), sink(1, &seen)),
                (NodeKey(5), burst(2, None, &five, &refused)),
                (NodeKey(9), burst(1, None, &nine, &refused)),
            ],
            events,
        };
        let mut want = Vec::new();
        if k > 0 {
            want.push((64, k as u32 - 1, 0, 89));
        }
        want.extend([
            (64, k as u32, 0, 50),
            (64, k as u32, 0, 51),
            (64, k as u32, 0, 90),
        ]);
        for executor in [true, false] {
            let (got, _) = rig.run(executor, 64, 64, &[64, 64, 64], &seen);
            assert_eq!(got, want, "tie at offset {k}, executor: {executor}");
        }
    }

    // The spec keeps sources in that order however they are connected, so
    // two specs with one wiring compare equal.
    let mut a = GraphSpec::default();
    let mut b = GraphSpec::default();
    for e in [out(9, 0), out(5, 1), out(5, 0)] {
        a.connect_events(at(1, 0), EventEdge::Direct(e));
    }
    for e in [out(5, 0), out(9, 0), out(5, 1)] {
        b.connect_events(at(1, 0), EventEdge::Direct(e));
    }
    assert_eq!(a, b);
    assert_eq!(
        a.events[&at(1, 0)],
        vec![
            EventEdge::Direct(out(5, 0)),
            EventEdge::Direct(out(5, 1)),
            EventEdge::Direct(out(9, 0)),
        ]
    );
}

/// PDC applies to events: a sink whose arrival a latent sibling raises to
/// 141 frames gets the direct source's events exactly 141 frames later —
/// across one or two block boundaries, under whole and ragged blocks —
/// every one of them, in order, and nothing dropped.
///
/// Mutation (run): in `compile`, emit no `EventDelay` for an event gap
/// (treat it as zero) → every event arrives 141 frames early → fails.
/// Mutation (run): in `EventFifo::pop_due`, deliver at `due - start + 1` →
/// one frame late → fails (and the one on a block's last frame lands
/// outside it, which `SortedEvents::trusted` catches in debug).
#[test]
fn pdc_shifts_events_across_blocks_and_drops_none() {
    const LATE: usize = 141;
    let seen: Seen = Arc::default();
    let refused = Arc::new(AtomicUsize::new(0));
    // A block's first and last frames, the 64-frame seam, and a chord.
    let frames = [0u64, 1, 60, 63, 64, 100, 127, 128, 300, 300, 300, 511];
    let plan: Vec<(u64, u16, u32)> = frames
        .iter()
        .enumerate()
        .map(|(i, &f)| (f, 0, i as u32))
        .collect();
    let mut events = BTreeMap::new();
    events.insert(
        at(1, 0),
        vec![EventEdge::Direct(out(2, 0)), EventEdge::Direct(out(3, 0))],
    );
    let rig = Rig {
        nodes: vec![
            (NodeKey(1), sink(1, &seen)),
            (NodeKey(2), burst(1, None, &plan, &refused)),
            (
                NodeKey(3),
                Box::new(|| Box::new(Late { latency: LATE }) as Box<dyn Node>),
            ),
        ],
        events,
    };
    let (valid, shapes) = rig.spec();
    let prep = Prepare::new(SampleRate(48_000.0), Samples(128));
    let (p, _) = compile(&valid, &shapes, &prep, None).expect("compiles");
    assert_eq!(
        p.unit(NodeKey(1)).expect("sink").arrival,
        Latency::new(Samples(LATE))
    );
    let total = 511 + LATE as u64 + 64;
    for blocks in [
        vec![64usize; total.div_ceil(64) as usize],
        ragged(total, 128),
    ] {
        let want: Vec<(u64, u32, u16, u32)> = frames
            .iter()
            .enumerate()
            .map(|(i, &f)| {
                let (b, o) = place(&blocks, f + LATE as u64);
                (b, o, 0, i as u32)
            })
            .collect();
        for executor in [true, false] {
            let (got, dropped) = rig.run(executor, 128, 64, &blocks, &seen);
            assert_eq!(got, want, "executor: {executor}, blocks {blocks:?}");
            assert_eq!(dropped, 0);
        }
    }
}

/// Only a feedback edge adds latency, and exactly its declared delay: the
/// same source into two ports of one sink, directly and through a 100-frame
/// feedback edge (at `MaxBlock` 64, so the delay crosses one or two block
/// boundaries), under whole and ragged blocks.
///
/// Mutation (run): in `Executor::rebuild`, size a new event feedback FIFO's
/// delay from `MaxBlock` rather than the edge (`EventFifo::sized(Samples(
/// max_block), ..)`) → the feedback copies arrive 64 frames after the direct
/// ones instead of 100 → fails.
#[test]
fn an_event_feedback_edge_delays_by_its_declared_delay() {
    const DELAY: usize = 100;
    let seen: Seen = Arc::default();
    let refused = Arc::new(AtomicUsize::new(0));
    let frames = [0u64, 27, 28, 63, 64, 90, 200];
    let plan: Vec<(u64, u16, u32)> = frames.iter().map(|&f| (f, 0, f as u32)).collect();
    let mut events = BTreeMap::new();
    events.insert(at(1, 0), vec![EventEdge::Direct(out(2, 0))]);
    events.insert(
        at(1, 1),
        vec![EventEdge::feedback(out(2, 0), Samples(DELAY))],
    );
    let rig = Rig {
        nodes: vec![
            (NodeKey(1), sink(2, &seen)),
            (NodeKey(2), burst(1, None, &plan, &refused)),
        ],
        events,
    };
    let total = 200 + DELAY as u64 + 64;
    for blocks in [
        vec![64usize; total.div_ceil(64) as usize],
        ragged(total, 64),
    ] {
        let mut want: Vec<(u64, u32, u16, u32)> = Vec::new();
        for &f in &frames {
            let (b, o) = place(&blocks, f);
            want.push((b, o, 0, f as u32));
            let (b, o) = place(&blocks, f + DELAY as u64);
            want.push((b, o, 1, f as u32));
        }
        // The sink logs port by port within a block; compare as sets of
        // deliveries, each at its (block, offset).
        want.sort();
        for executor in [true, false] {
            let (mut got, dropped) = rig.run(executor, 64, 64, &blocks, &seen);
            got.sort();
            assert_eq!(got, want, "executor: {executor}, blocks {blocks:?}");
            assert_eq!(dropped, 0);
        }
    }
}

/// Overflow is defined and observable: a port declaring 3 events per block
/// that is handed 5 in one block keeps the first 3 — the newest two are
/// refused at the push (the node is told) and counted by the interpreter.
/// A port that declares nothing takes the executor's default, and all 5 fit.
///
/// Mutation (run): in `node_op`, give every writer the executor's default
/// (`cap: h.cap`) → the declared port keeps all 5 → fails. Mutation (run):
/// in the reference, keep writers unbounded → the reference keeps 5 →
/// fails.
#[test]
fn a_port_past_its_declared_capacity_drops_the_newest_and_counts_them() {
    let seen: Seen = Arc::default();
    for (cap, kept) in [(Some(3u32), 3usize), (None, 5)] {
        let plan: Vec<(u64, u16, u32)> = (0..5).map(|i| (70 + i, 0, i as u32)).collect();
        let want: Vec<(u64, u32, u16, u32)> =
            (0..kept).map(|i| (64, 6 + i as u32, 0, i as u32)).collect();
        for executor in [true, false] {
            let refused = Arc::new(AtomicUsize::new(0));
            let mut events = BTreeMap::new();
            events.insert(at(1, 0), vec![EventEdge::Direct(out(2, 0))]);
            let rig = Rig {
                nodes: vec![
                    (NodeKey(1), sink(1, &seen)),
                    (NodeKey(2), burst(1, cap, &plan, &refused)),
                ],
                events,
            };
            let (got, dropped) = rig.run(executor, 64, 64, &[64, 64, 64], &seen);
            assert_eq!(got, want, "{cap:?}, executor: {executor}");
            let lost = 5 - kept;
            assert_eq!(dropped, lost as u64, "counted by the interpreter");
            assert_eq!(
                refused.load(Ordering::Relaxed),
                lost,
                "and refused to the node at the push"
            );
        }
    }
}

/// Everything downstream of a writer is sized from the **declarations**, so
/// a node that keeps to its own loses nothing even when the executor's
/// default capacity is 1: two sources declaring 8 each send 8 events a block
/// (a chord on one frame) into one port — a merge, with one of them behind a
/// 141-frame PDC delay on a `MaxBlock` of 64 — and all of them arrive, on
/// their frames. The plan prices the merge at 16 declared events.
///
/// Mutation (run): size event slots from the default alone
/// (`Vec::with_capacity(cap)` in `rebuild`) → the merge, sized for one
/// event per source, drops → the drop count fails. Mutation (run): size the
/// PDC FIFO from the default (`rate(from)` → `cap`) → its limit is 5
/// events, the chords overflow it → fails. Mutation (run): make `EventSlotCapacity::plus` a max
/// → the merge holds 8 and drops the other 8 → fails.
#[test]
fn declared_capacities_size_merges_and_delays_so_nothing_is_lost() {
    const LATE: usize = 141;
    let seen: Seen = Arc::default();
    let refused = Arc::new(AtomicUsize::new(0));
    // Every 64 frames, 8 events on one frame, for 10 blocks.
    let chord = |tag: u32| -> Vec<(u64, u16, u32)> {
        (0..10u64)
            .flat_map(|b| (0..8u32).map(move |i| (b * 64 + 13, 0, tag + b as u32 * 100 + i)))
            .collect()
    };
    let mut events = BTreeMap::new();
    events.insert(
        at(1, 0),
        vec![EventEdge::Direct(out(2, 0)), EventEdge::Direct(out(3, 0))],
    );
    // Node 3's events reach node 4 (a relay) behind node 5's latency, and
    // the relay forwards them into the sink's second port.
    events.insert(
        at(4, 0),
        vec![EventEdge::Direct(out(3, 0)), EventEdge::Direct(out(5, 0))],
    );
    events.insert(at(1, 1), vec![EventEdge::Direct(out(4, 0))]);
    let rig = Rig {
        nodes: vec![
            (NodeKey(1), sink(2, &seen)),
            (NodeKey(2), burst(1, Some(8), &chord(10_000), &refused)),
            (NodeKey(3), burst(1, Some(8), &chord(20_000), &refused)),
            (
                NodeKey(4),
                Box::new(|| Box::new(CappedRelay) as Box<dyn Node>),
            ),
            (
                NodeKey(5),
                Box::new(|| Box::new(Late { latency: LATE }) as Box<dyn Node>),
            ),
        ],
        events,
    };
    let (valid, shapes) = rig.spec();
    let prep = Prepare::new(SampleRate(48_000.0), Samples(64));
    let (p, _) = compile(&valid, &shapes, &prep, None).expect("compiles");
    // The sink's merge: both chord sources, 8 declared each.
    let sink_merge = p
        .ops()
        .iter()
        .filter_map(|op| match *op {
            Op::EventMerge { dst, .. } => Some(p.event_slot_capacity()[dst as usize]),
            _ => None,
        })
        .max_by_key(|c| c.declared)
        .expect("a merge");
    assert!(
        sink_merge.holds(EventSlotCapacity {
            declared: 16,
            defaults: 0
        }),
        "{sink_merge:?}"
    );
    let blocks = vec![64usize; 14];
    for executor in [true, false] {
        let (got, dropped) = rig.run(executor, 64, 1, &blocks, &seen);
        assert_eq!(dropped, 0, "executor: {executor}");
        assert_eq!(refused.load(Ordering::Relaxed), 0);
        let direct = got.iter().filter(|g| g.2 == 0).count();
        let delayed: Vec<_> = got.iter().filter(|g| g.2 == 1).collect();
        assert_eq!(
            direct, 160,
            "both chords, every block, executor: {executor}"
        );
        assert_eq!(delayed.len(), 80, "executor: {executor}");
        for d in delayed {
            let f = d.0 + u64::from(d.1);
            assert_eq!((f - LATE as u64) % 64, 13, "on its frame, 141 late");
        }
    }
}

/// A relay declaring 8 per block (it forwards one chord at a time).
struct CappedRelay;

impl Node for CappedRelay {
    fn shape(&self) -> Shape {
        Relay.shape().with_event_capacity(8)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        Relay.process(cx, io)
    }
    fn reset(&mut self) {}
}
