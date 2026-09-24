//! The compiler: `compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)`.
//!
//! Doc 013 §3. Pure: no I/O, no audio, no units — only the value, the shapes
//! the units declared, and the previous plan (for placements and nothing
//! else). The passes, in order:
//!
//! 1. **Check** every node's shape against its spec — widths, latency and
//!    tail must all agree — and every event edge's ports against the shapes.
//!    The shape is what the unit says about itself and the spec is what the
//!    value says; if they disagree, one of them is stale, and compiling
//!    either would make the latency folds over the value
//!    (`tutti_types::latency::plan`) disagree with the plan.
//! 2. **SCC** ([`order::scc`]) over direct audio + event dependencies. A cycle
//!    not broken by a feedback edge is [`CompileError::Cycle`], naming the
//!    edges. A feedback edge becomes a read of a slot the executor fills from
//!    the edge's own delay state before the block, and a `Capture` op
//!    that feeds the delay, so the op DAG stays acyclic.
//! 3. **Order** ([`order::kahn`]) — one deterministic topological sort.
//! 4. **Latency solve** — `arrival = max(departures)`, `departure = arrival +
//!    own`, `tutti_types::latency::plan`'s forward pass, with two deliberate
//!    differences until `Net` goes (doc 013 Phase 5) and the two solves are
//!    unified:
//!    - **event edges count toward arrival** here; `latency::plan` walks audio
//!      only, because `Net` has no event ports;
//!    - **a `Source::Global` input is a merge-point source** here, delayed to
//!      the node's arrival like any other; `latency::plan` treats it as
//!      outside the graph and never delays it.
//!
//!    Emits a `Delay` op per mismatched audio port and per mismatched event
//!    *source*, plus per-output alignment rings, with state keyed by
//!    [`DelayKey`]. An event fan-in wider than [`MAX_PORTS`] becomes a tree
//!    of merges over contiguous source ranges, which keeps the
//!    `(offset, source order)` rule exactly.
//! 5. **Emit ops** in the serial order, recording every value's writer and
//!    readers, and the op DAG.
//! 6. **Colour** ([`colour`]) — slot sharing that is correct under any
//!    parallel schedule, with in-place aliasing where legal.
//! 7. **Coarsen** — fuse single-successor/single-predecessor chains into
//!    tasks. The serial executor ignores tasks; the plan carries them so the
//!    phase-2 parallel executor needs no format change.
//! 8. **Verify** ([`verify`]) in debug builds.
//! 9. **Place** units: `NodeKey` → dense [`UnitIdx`], diffed against `prev`
//!    into a [`Delta`].
//!
//! Doc 013's step 6 (a serial-vs-parallel cost model) is phase 6 work; there
//! is only a serial executor to pick.

mod colour;
mod order;
pub(crate) mod verify;

use std::collections::{BTreeMap, BTreeSet};

use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::latency::MAX_NODE_LATENCY;
use tutti_types::{ChannelLayout, Latency, NodeKey, Samples, Tail};

use crate::node::{InPlaceMask, Prepare, Shape, MAX_PORTS};
use crate::plan::{
    Csr, DelayKey, DelaySpec, Delta, FeedbackKey, FeedbackSpec, NodeTables, Op, Placement, Plan,
    PlanUnit, Span, UnitIdx, Value, EMPTY_SLOT, ZERO_SLOT,
};
use crate::spec::{EventEdge, EventIn, EventOut, ValidGraph};

pub use verify::VerifyError;

/// The shape each node declared, by key — what `compile` checks the value
/// against and folds latency over.
pub type Shapes = BTreeMap<NodeKey, Shape>;

/// One edge of a cycle, named by its sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CycleEdge {
    /// An audio edge into this port.
    Audio(InPort),
    /// An event edge from `from` into `at`.
    Event {
        /// The sink port.
        at: EventIn,
        /// The source port.
        from: EventOut,
    },
}

/// Why a [`ValidGraph`] could not be compiled against these shapes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// A feedback edge's delay is shorter than the `MaxBlock` being compiled
    /// for: a block cannot read samples it has not produced yet. For an
    /// offline render, prepare with a smaller maximum block — never shorten
    /// the loop, or the bounce would not sound like playback.
    FeedbackTooShort {
        /// The edge, named by its sink.
        edge: CycleEdge,
        /// Its declared delay.
        delay: Samples,
        /// The maximum block being compiled for.
        max_block: Samples,
    },
    /// No shape was supplied for a node.
    MissingShape {
        /// The node.
        node: NodeKey,
    },
    /// The node's shape disagrees with the widths its spec declared.
    WidthMismatch {
        /// The node.
        node: NodeKey,
        /// `(inputs, outputs)` as the spec declared them.
        declared: (ChannelLayout, ChannelLayout),
        /// `(inputs, outputs)` as the shape reports them.
        shape: (ChannelLayout, ChannelLayout),
    },
    /// An event edge names a port past a node's declared event ports.
    EventPortOutOfRange {
        /// The sink port of the bad edge.
        at: EventIn,
        /// The source, when it is the source that is out of range.
        from: Option<EventOut>,
    },
    /// A node has more than [`MAX_PORTS`] channels or event ports on a side.
    TooManyPorts {
        /// The node.
        node: NodeKey,
        /// The offending count.
        count: usize,
    },
    /// The shape's latency disagrees with the spec's (see the module docs,
    /// step 1).
    LatencyMismatch {
        /// The node.
        node: NodeKey,
        /// What the spec declared.
        declared: Samples,
        /// What the shape reports.
        shape: Latency,
    },
    /// The shape's tail disagrees with the spec's.
    TailMismatch {
        /// The node.
        node: NodeKey,
        /// What the spec declared.
        declared: Tail,
        /// What the shape reports.
        shape: Tail,
    },
    /// A cycle that no feedback edge breaks. Every direct edge inside the
    /// cycle's strongly connected component is listed, in key order.
    Cycle {
        /// The edges.
        edges: Vec<CycleEdge>,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FeedbackTooShort {
                edge,
                delay,
                max_block,
            } => write!(
                f,
                "feedback edge {edge:?} delays {delay} frames, less than the maximum block {max_block}"
            ),
            Self::MissingShape { node } => write!(f, "no shape for node {}", node.0),
            Self::WidthMismatch {
                node,
                declared,
                shape,
            } => write!(
                f,
                "node {} declares {}in/{}out but its unit is {}in/{}out",
                node.0,
                declared.0.count(),
                declared.1.count(),
                shape.0.count(),
                shape.1.count()
            ),
            Self::EventPortOutOfRange { at, from } => match from {
                Some(from) => write!(
                    f,
                    "event edge into node {} port {}: source node {} has no event output {}",
                    at.node.0, at.port, from.node.0, from.port
                ),
                None => write!(f, "node {} has no event input {}", at.node.0, at.port),
            },
            Self::TooManyPorts { node, count } => write!(
                f,
                "node {} has {count} ports on one side; the limit is {MAX_PORTS}",
                node.0
            ),
            Self::LatencyMismatch {
                node,
                declared,
                shape,
            } => write!(
                f,
                "node {} declares {declared} frames of latency but its unit reports {shape}",
                node.0
            ),
            Self::TailMismatch {
                node,
                declared,
                shape,
            } => write!(
                f,
                "node {} declares tail {declared:?} but its unit reports {shape:?}",
                node.0
            ),
            Self::Cycle { edges } => write!(f, "unbroken cycle through {} edges", edges.len()),
        }
    }
}

impl std::error::Error for CompileError {}

/// Where an audio input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ARef {
    Val(u32),
    Zero,
    Fb(u32),
}

/// Where an event input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ERef {
    Val(u32),
    Empty,
    Fb(u32),
}

/// An op before colouring: the same shape as [`Op`] with value ids where
/// slots will go.
enum Pre {
    GlobalIn {
        channel: u16,
        dst: u32,
    },
    Delay {
        delay: u32,
        src: u32,
        dst: u32,
    },
    EventDelay {
        delay: u32,
        src: u32,
        dst: u32,
    },
    EventMerge {
        srcs: Vec<ERef>,
        dst: u32,
    },
    Node {
        unit: u32,
        ain: Vec<ARef>,
        aout: Vec<u32>,
        ein: Vec<ERef>,
        eout: Vec<u32>,
    },
    Output {
        channel: u16,
        src: ARef,
        delay: Option<u32>,
    },
    Capture {
        feedback: u32,
        src: u32,
    },
    EventCapture {
        feedback: u32,
        src: u32,
    },
}

/// A value under construction.
#[derive(Default)]
pub(crate) struct Val {
    pub(crate) writer: u32,
    pub(crate) readers: Vec<u32>,
}

/// What the op-emission pass produces for the colouring pass.
struct Emitted {
    pre: Vec<Pre>,
    audio: Vec<Val>,
    event: Vec<Val>,
    preds: Vec<BTreeSet<u32>>,
    delays: Vec<DelaySpec>,
}

impl Emitted {
    fn push(&mut self, op: Pre) -> u32 {
        self.pre.push(op);
        self.preds.push(BTreeSet::new());
        (self.pre.len() - 1) as u32
    }

    fn audio_value(&mut self, writer: u32) -> u32 {
        self.audio.push(Val {
            writer,
            readers: Vec::new(),
        });
        (self.audio.len() - 1) as u32
    }

    fn event_value(&mut self, writer: u32) -> u32 {
        self.event.push(Val {
            writer,
            readers: Vec::new(),
        });
        (self.event.len() - 1) as u32
    }

    fn read_audio(&mut self, v: u32, op: u32) {
        let w = self.audio[v as usize].writer;
        self.audio[v as usize].readers.push(op);
        self.preds[op as usize].insert(w);
    }

    fn read_event(&mut self, v: u32, op: u32) {
        let w = self.event[v as usize].writer;
        self.event[v as usize].readers.push(op);
        self.preds[op as usize].insert(w);
    }

    fn read_aref(&mut self, r: ARef, op: u32) {
        if let ARef::Val(v) = r {
            self.read_audio(v, op);
        }
    }

    fn read_eref(&mut self, r: ERef, op: u32) {
        if let ERef::Val(v) = r {
            self.read_event(v, op);
        }
    }
}

/// Compile `graph` against the units' `shapes`.
///
/// `prev` is the plan currently running, if any. It is read for exactly one
/// thing — which store index each surviving key already occupies — so that the
/// [`Delta`] moves only what changed. Delay-ring and feedback state carry over
/// by *key* at the runtime and need nothing from `prev`.
pub fn compile(
    graph: &ValidGraph,
    shapes: &Shapes,
    prepare: &Prepare,
    prev: Option<&Plan>,
) -> Result<(Plan, Delta), CompileError> {
    let topology = graph.topology();
    let keys: Vec<NodeKey> = topology.nodes.keys().copied().collect();
    let dense: BTreeMap<NodeKey, usize> = keys.iter().enumerate().map(|(i, &k)| (k, i)).collect();

    // ---- 1. shapes against the value -------------------------------------
    let mut node_shapes: Vec<Shape> = Vec::with_capacity(keys.len());
    for (&key, spec) in &topology.nodes {
        let shape = *shapes
            .get(&key)
            .ok_or(CompileError::MissingShape { node: key })?;
        if (spec.inputs, spec.outputs) != (shape.audio_in, shape.audio_out) {
            return Err(CompileError::WidthMismatch {
                node: key,
                declared: (spec.inputs, spec.outputs),
                shape: (shape.audio_in, shape.audio_out),
            });
        }
        if spec.latency != shape.latency.samples() {
            return Err(CompileError::LatencyMismatch {
                node: key,
                declared: spec.latency,
                shape: shape.latency,
            });
        }
        if spec.tail != shape.tail {
            return Err(CompileError::TailMismatch {
                node: key,
                declared: spec.tail,
                shape: shape.tail,
            });
        }
        for count in [
            shape.audio_in.count() as usize,
            shape.audio_out.count() as usize,
            shape.event_in as usize,
            shape.event_out as usize,
        ] {
            if count > MAX_PORTS {
                return Err(CompileError::TooManyPorts { node: key, count });
            }
        }
        node_shapes.push(shape);
    }
    for (&at, sources) in graph.events() {
        if at.port >= node_shapes[dense[&at.node]].event_in {
            return Err(CompileError::EventPortOutOfRange { at, from: None });
        }
        for e in sources {
            let from = e.from();
            if from.port >= node_shapes[dense[&from.node]].event_out {
                return Err(CompileError::EventPortOutOfRange {
                    at,
                    from: Some(from),
                });
            }
        }
    }

    // Feedback delays: at least one block, or a read would need the future.
    let max_block = prepare.max_block().samples();
    for (&at, e) in &topology.edges {
        if let Edge::Feedback(f) = *e {
            if f.delay < max_block {
                return Err(CompileError::FeedbackTooShort {
                    edge: CycleEdge::Audio(at),
                    delay: f.delay,
                    max_block,
                });
            }
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Feedback { from, delay } = *e {
                if delay < max_block {
                    return Err(CompileError::FeedbackTooShort {
                        edge: CycleEdge::Event { at, from },
                        delay,
                        max_block,
                    });
                }
            }
        }
    }

    // ---- 2. cycles --------------------------------------------------------
    // Direct dependencies, with multiplicity (for Kahn) and labelled (for the
    // error).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    let mut labelled: Vec<(usize, usize, CycleEdge)> = Vec::new();
    for (&at, edge) in &topology.edges {
        if let Edge::Direct(Source::Node(p)) = *edge {
            let (s, d) = (dense[&p.node], dense[&at.node]);
            preds[d].push(s);
            labelled.push((s, d, CycleEdge::Audio(at)));
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Direct(from) = *e {
                let (s, d) = (dense[&from.node], dense[&at.node]);
                preds[d].push(s);
                labelled.push((s, d, CycleEdge::Event { at, from }));
            }
        }
    }
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    for (d, ps) in preds.iter().enumerate() {
        for &s in ps {
            succ[s].push(d);
        }
    }
    let comp = order::scc(&succ);
    let mut comp_size = vec![0usize; keys.len()];
    for &c in &comp {
        comp_size[c] += 1;
    }
    let mut cycle: Vec<CycleEdge> = labelled
        .iter()
        .filter(|(s, d, _)| comp[*s] == comp[*d] && (s == d || comp_size[comp[*s]] > 1))
        .map(|&(_, _, e)| e)
        .collect();
    if !cycle.is_empty() {
        cycle.sort();
        return Err(CompileError::Cycle { edges: cycle });
    }

    // ---- 3. order ---------------------------------------------------------
    let order = order::kahn(&keys, &preds);

    // ---- 4. latency -------------------------------------------------------
    let latency: Vec<Latency> = node_shapes
        .iter()
        .map(|s| s.latency.min(Latency::new(MAX_NODE_LATENCY)))
        .collect();
    let mut arrival = vec![Latency::ZERO; keys.len()];
    for &n in &order {
        arrival[n] = preds[n]
            .iter()
            .map(|&p| arrival[p] + latency[p])
            .max()
            .unwrap_or_default();
    }
    let departure = |n: usize| arrival[n] + latency[n];
    // Per output channel, exactly as `latency::plan`: a non-node source
    // contributes no internal latency and arrives at zero.
    let channel_arrivals: Vec<Latency> = topology
        .outputs
        .iter()
        .map(|s| match s {
            Source::Node(p) => departure(dense[&p.node]),
            _ => Latency::ZERO,
        })
        .collect();
    let total = channel_arrivals.iter().copied().max().unwrap_or_default();
    let compensation: Vec<Samples> = channel_arrivals.iter().map(|&a| a.gap_to(total)).collect();

    // ---- 5. emit ----------------------------------------------------------
    let audio_fb_key = |f: FeedbackFrom| FeedbackKey::Audio {
        from: f.from,
        gen: graph.generation(f.from.node),
        delay: f.delay,
    };
    let event_fb_key = |at: EventIn, from: EventOut, delay: Samples| FeedbackKey::Event {
        at,
        from,
        gen: graph.generation(from.node),
        delay,
    };
    let audio_fb: Vec<FeedbackKey> = topology
        .edges
        .values()
        .filter_map(|e| match *e {
            Edge::Feedback(f) => Some(audio_fb_key(f)),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let event_fb: Vec<FeedbackKey> = graph
        .events()
        .iter()
        .flat_map(|(&at, sources)| {
            sources.iter().filter_map(move |e| match *e {
                EventEdge::Feedback { from, delay } => Some((at, from, delay)),
                EventEdge::Direct(_) => None,
            })
        })
        .map(|(at, from, delay)| event_fb_key(at, from, delay))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let fb_index = |key: FeedbackKey, list: &[FeedbackKey]| -> u32 {
        list.binary_search(&key).expect("collected above") as u32
    };

    let mut em = Emitted {
        pre: Vec::new(),
        audio: Vec::new(),
        event: Vec::new(),
        preds: Vec::new(),
        delays: Vec::new(),
    };

    // Global inputs actually read, each copied into a slot once.
    let mut global_val: BTreeMap<u16, u32> = BTreeMap::new();
    let used_globals: BTreeSet<u16> = topology
        .edges
        .values()
        .filter_map(|e| match *e {
            Edge::Direct(Source::Global(ch)) => Some(ch),
            _ => None,
        })
        .chain(topology.outputs.iter().filter_map(|s| match *s {
            Source::Global(ch) => Some(ch),
            _ => None,
        }))
        .collect();
    for ch in used_globals {
        let op = em.push(Pre::GlobalIn {
            channel: ch,
            dst: 0,
        });
        let v = em.audio_value(op);
        if let Pre::GlobalIn { dst, .. } = &mut em.pre[op as usize] {
            *dst = v;
        }
        global_val.insert(ch, v);
    }

    let mut audio_out_val: BTreeMap<OutPort, u32> = BTreeMap::new();
    let mut event_out_val: BTreeMap<EventOut, u32> = BTreeMap::new();

    for &n in &order {
        let key = keys[n];
        let shape = node_shapes[n];

        // Audio inputs, each through its PDC delay when it needs one.
        let mut ain = Vec::with_capacity(shape.audio_in.count() as usize);
        for port in 0..shape.audio_in.count() {
            let at = InPort { node: key, port };
            let r = match topology.edges.get(&at) {
                None | Some(Edge::Direct(Source::Zero)) => ARef::Zero,
                Some(&Edge::Direct(from @ (Source::Global(_) | Source::Node(_)))) => {
                    // A global input arrives at zero: it is a merge-point
                    // source like any other (see the module docs, step 4).
                    let (v, dep) = match from {
                        Source::Global(ch) => (global_val[&ch], Latency::ZERO),
                        Source::Node(p) => (audio_out_val[&p], departure(dense[&p.node])),
                        Source::Zero => unreachable!("matched above"),
                    };
                    let d = dep.gap_to(arrival[n]);
                    if d.is_zero() {
                        ARef::Val(v)
                    } else {
                        let delay = em.delays.len() as u32;
                        em.delays.push(DelaySpec {
                            key: DelayKey::Audio { at, from },
                            len: d,
                        });
                        let op = em.push(Pre::Delay {
                            delay,
                            src: v,
                            dst: 0,
                        });
                        em.read_audio(v, op);
                        let dv = em.audio_value(op);
                        if let Pre::Delay { dst, .. } = &mut em.pre[op as usize] {
                            *dst = dv;
                        }
                        ARef::Val(dv)
                    }
                }
                Some(Edge::Feedback(f)) => ARef::Fb(fb_index(audio_fb_key(*f), &audio_fb)),
            };
            ain.push(r);
        }

        // Event inputs: delay each source that needs it, then merge fan-in.
        let mut ein = Vec::with_capacity(shape.event_in as usize);
        for port in 0..shape.event_in {
            let at = EventIn { node: key, port };
            let sources = graph.events().get(&at).map(Vec::as_slice).unwrap_or(&[]);
            let mut refs: Vec<ERef> = Vec::with_capacity(sources.len());
            for e in sources {
                refs.push(match *e {
                    EventEdge::Feedback { from, delay } => {
                        ERef::Fb(fb_index(event_fb_key(at, from, delay), &event_fb))
                    }
                    EventEdge::Direct(from) => {
                        let v = event_out_val[&from];
                        let d = departure(dense[&from.node]).gap_to(arrival[n]);
                        match d {
                            d if !d.is_zero() => {
                                let delay = em.delays.len() as u32;
                                em.delays.push(DelaySpec {
                                    key: DelayKey::Event { at, from },
                                    len: d,
                                });
                                let op = em.push(Pre::EventDelay {
                                    delay,
                                    src: v,
                                    dst: 0,
                                });
                                em.read_event(v, op);
                                let dv = em.event_value(op);
                                if let Pre::EventDelay { dst, .. } = &mut em.pre[op as usize] {
                                    *dst = dv;
                                }
                                ERef::Val(dv)
                            }
                            _ => ERef::Val(v),
                        }
                    }
                });
            }
            ein.push(merge_tree(&mut em, refs));
        }

        let op = em.push(Pre::Node {
            unit: n as u32,
            ain: ain.clone(),
            aout: Vec::new(),
            ein: ein.clone(),
            eout: Vec::new(),
        });
        for &r in &ain {
            em.read_aref(r, op);
        }
        for &r in &ein {
            em.read_eref(r, op);
        }
        let aout: Vec<u32> = (0..shape.audio_out.count())
            .map(|port| {
                let v = em.audio_value(op);
                audio_out_val.insert(OutPort { node: key, port }, v);
                v
            })
            .collect();
        let eout: Vec<u32> = (0..shape.event_out)
            .map(|port| {
                let v = em.event_value(op);
                event_out_val.insert(EventOut { node: key, port }, v);
                v
            })
            .collect();
        if let Pre::Node {
            aout: a, eout: e, ..
        } = &mut em.pre[op as usize]
        {
            *a = aout;
            *e = eout;
        }
    }

    for (channel, source) in topology.outputs.iter().enumerate() {
        let channel = channel as u16;
        let src = match *source {
            Source::Node(p) => ARef::Val(audio_out_val[&p]),
            Source::Global(ch) => ARef::Val(global_val[&ch]),
            Source::Zero => ARef::Zero,
        };
        let d = compensation[channel as usize];
        // Delaying silence is silence: a zero channel reports its
        // compensation (as `latency::plan` does) but gets no ring.
        let delay = (!d.is_zero() && src != ARef::Zero).then(|| {
            em.delays.push(DelaySpec {
                key: DelayKey::Output {
                    channel,
                    from: *source,
                },
                len: d,
            });
            (em.delays.len() - 1) as u32
        });
        let op = em.push(Pre::Output {
            channel,
            src,
            delay,
        });
        em.read_aref(src, op);
    }

    // Captures run last. They feed the feedback delays, which the executor
    // reads from *before* the block (into the feedback slots), so a capture
    // needs no ordering against the feedback's readers: with a delay of a
    // whole `MaxBlock`, nothing it queues is due this block.
    for (f, key) in audio_fb.iter().enumerate() {
        let FeedbackKey::Audio { from, .. } = *key else {
            unreachable!("audio feedback keys are audio")
        };
        let v = audio_out_val[&from];
        let op = em.push(Pre::Capture {
            feedback: f as u32,
            src: v,
        });
        em.read_audio(v, op);
    }
    for (f, key) in event_fb.iter().enumerate() {
        let FeedbackKey::Event { from, .. } = *key else {
            unreachable!("event feedback keys are events")
        };
        let v = event_out_val[&from];
        let op = em.push(Pre::EventCapture {
            feedback: f as u32,
            src: v,
        });
        em.read_event(v, op);
    }

    // ---- 6. colour --------------------------------------------------------
    let op_count = em.pre.len();
    let mut succ_rows: Vec<Vec<u32>> = vec![Vec::new(); op_count];
    for (op, ps) in em.preds.iter().enumerate() {
        for &p in ps {
            debug_assert!((p as usize) < op, "ops are emitted in topological order");
            succ_rows[p as usize].push(op as u32);
        }
    }
    let reach = colour::Reach::new(&succ_rows);

    // In-place candidates: (value written, value it may overwrite).
    let mut in_place: BTreeMap<u32, u32> = BTreeMap::new();
    for pre in &em.pre {
        match pre {
            Pre::Delay { src, dst, .. } => {
                in_place.insert(*dst, *src);
            }
            Pre::Node {
                unit, ain, aout, ..
            } if node_shapes[*unit as usize].in_place => {
                for (&r, &out) in ain.iter().zip(aout) {
                    let ARef::Val(u) = r else { continue };
                    // Read by this node on exactly one port, or the other
                    // port would see the output instead of the input.
                    if ain.iter().filter(|&&x| x == r).count() != 1 {
                        continue;
                    }
                    in_place.insert(out, u);
                }
            }
            _ => {}
        }
    }

    let audio_fixed = 1 + audio_fb.len() as u32;
    let event_fixed = 1 + event_fb.len() as u32;
    let audio_colour = colour::colour(&em.audio, &reach, &in_place);
    let event_colour = colour::colour(&em.event, &reach, &BTreeMap::new());
    let aslot = |v: u32| audio_fixed + audio_colour.slot[v as usize];
    let eslot = |v: u32| event_fixed + event_colour.slot[v as usize];

    // Event slot capacities, in units of the executor's per-slot capacity: a
    // node output or a delay output holds one; a merge holds all its inputs
    // together, so it can never drop an event (a note-off least of all).
    let mut value_weight = vec![1u32; em.event.len()];
    for pre in &em.pre {
        if let Pre::EventMerge { srcs, dst } = pre {
            value_weight[*dst as usize] = srcs
                .iter()
                .map(|r| match *r {
                    ERef::Val(v) => value_weight[v as usize],
                    ERef::Fb(_) => 1,
                    ERef::Empty => 0,
                })
                .sum::<u32>()
                .max(1);
        }
    }
    let mut event_slot_weight = vec![1u32; (event_fixed + event_colour.count) as usize];
    event_slot_weight[EMPTY_SLOT as usize] = 0;
    for (v, &w) in value_weight.iter().enumerate() {
        let s = eslot(v as u32) as usize;
        event_slot_weight[s] = event_slot_weight[s].max(w);
    }
    let aref_slot = |r: ARef| match r {
        ARef::Val(v) => aslot(v),
        ARef::Zero => ZERO_SLOT,
        ARef::Fb(f) => 1 + f,
    };
    let eref_slot = |r: ERef| match r {
        ERef::Val(v) => eslot(v),
        ERef::Empty => EMPTY_SLOT,
        ERef::Fb(f) => 1 + f,
    };

    // ---- lower ------------------------------------------------------------
    let mut audio_list: Vec<u32> = Vec::new();
    let mut event_list: Vec<u32> = Vec::new();
    let span_of = |list: &mut Vec<u32>, items: &mut dyn Iterator<Item = u32>| {
        let start = list.len() as u32;
        list.extend(items);
        Span {
            start,
            len: list.len() as u32 - start,
        }
    };
    let mut ops: Vec<Op> = Vec::with_capacity(op_count);
    for pre in &em.pre {
        ops.push(match pre {
            Pre::GlobalIn { channel, dst } => Op::GlobalIn {
                channel: *channel,
                dst: aslot(*dst),
            },
            Pre::Delay { delay, src, dst } => Op::Delay {
                delay: *delay,
                src: aslot(*src),
                dst: aslot(*dst),
            },
            Pre::EventDelay { delay, src, dst } => Op::EventDelay {
                delay: *delay,
                src: eslot(*src),
                dst: eslot(*dst),
            },
            Pre::EventMerge { srcs, dst } => Op::EventMerge {
                srcs: span_of(&mut event_list, &mut srcs.iter().map(|&r| eref_slot(r))),
                dst: eslot(*dst),
            },
            Pre::Node {
                unit,
                ain,
                aout,
                ein,
                eout,
            } => {
                let mut mask = InPlaceMask::NONE;
                for (c, (&r, &o)) in ain.iter().zip(aout).enumerate() {
                    if let ARef::Val(u) = r {
                        if aslot(u) == aslot(o) {
                            mask = mask.with(c);
                        }
                    }
                }
                Op::Node {
                    unit: *unit,
                    audio_in: span_of(&mut audio_list, &mut ain.iter().map(|&r| aref_slot(r))),
                    audio_out: span_of(&mut audio_list, &mut aout.iter().map(|&v| aslot(v))),
                    event_in: span_of(&mut event_list, &mut ein.iter().map(|&r| eref_slot(r))),
                    event_out: span_of(&mut event_list, &mut eout.iter().map(|&v| eslot(v))),
                    in_place: mask,
                }
            }
            Pre::Output {
                channel,
                src,
                delay,
            } => Op::Output {
                channel: *channel,
                src: aref_slot(*src),
                delay: *delay,
            },
            Pre::Capture { feedback, src } => Op::Capture {
                feedback: *feedback,
                src: aslot(*src),
            },
            Pre::EventCapture { feedback, src } => Op::EventCapture {
                feedback: *feedback,
                src: eslot(*src),
            },
        });
    }

    let mut value_readers: Vec<u32> = Vec::new();
    let mut lower_values = |vals: &[Val], slot: &dyn Fn(u32) -> u32| -> Vec<Value> {
        vals.iter()
            .enumerate()
            .map(|(i, v)| {
                let start = value_readers.len() as u32;
                value_readers.extend(&v.readers);
                Value {
                    slot: slot(i as u32),
                    writer: v.writer,
                    readers: Span {
                        start,
                        len: v.readers.len() as u32,
                    },
                }
            })
            .collect()
    };
    let audio_values = lower_values(&em.audio, &aslot);
    let event_values = lower_values(&em.event, &eslot);

    // ---- 7. coarsen -------------------------------------------------------
    let (tasks, task_ops, task_succ, task_activation) = coarsen(&em.preds, &succ_rows);

    // ---- 9. place ---------------------------------------------------------
    let prev_units: BTreeMap<NodeKey, PlanUnit> = prev
        .map(|p| p.units.iter().map(|u| (u.key, *u)).collect())
        .unwrap_or_default();
    let mut delta = Delta::default();
    let mut idx_of: BTreeMap<NodeKey, UnitIdx> = BTreeMap::new();
    let mut taken: BTreeSet<u32> = BTreeSet::new();
    for &key in &keys {
        let gen = graph.generation(key);
        if let Some(old) = prev_units.get(&key) {
            idx_of.insert(key, old.idx);
            taken.insert(old.idx.0);
            if old.gen != gen {
                delta.replace.push((
                    Placement {
                        key,
                        gen: old.gen,
                        idx: old.idx,
                    },
                    Placement {
                        key,
                        gen,
                        idx: old.idx,
                    },
                ));
            }
        }
    }
    let mut next_free = 0u32;
    for &key in &keys {
        if idx_of.contains_key(&key) {
            continue;
        }
        while taken.contains(&next_free) {
            next_free += 1;
        }
        let idx = UnitIdx(next_free);
        taken.insert(next_free);
        idx_of.insert(key, idx);
        delta.insert.push(Placement {
            key,
            gen: graph.generation(key),
            idx,
        });
    }
    for (key, old) in &prev_units {
        if !topology.nodes.contains_key(key) {
            delta.retire.push(Placement {
                key: *key,
                gen: old.gen,
                idx: old.idx,
            });
        }
    }
    let store_len = taken.iter().next_back().map_or(0, |&m| m + 1);
    delta.store_len = store_len;

    let units: Vec<PlanUnit> = keys
        .iter()
        .enumerate()
        .map(|(n, &key)| PlanUnit {
            key,
            gen: graph.generation(key),
            idx: idx_of[&key],
            shape: node_shapes[n],
            arrival: arrival[n],
        })
        .collect();

    let nodes = NodeTables::lower(&ops, &audio_list, &event_list, &units);
    let plan = Plan {
        prepare: *prepare,
        ops,
        audio_list,
        event_list,
        op_succ: Csr::from_rows(&succ_rows),
        tasks,
        task_ops,
        task_succ,
        task_activation,
        audio_slots: audio_fixed + audio_colour.count,
        event_slots: event_fixed + event_colour.count,
        event_slot_weight,
        audio_feedback: audio_fb
            .iter()
            .enumerate()
            .map(|(i, &key)| FeedbackSpec {
                key,
                slot: 1 + i as u32,
            })
            .collect(),
        event_feedback: event_fb
            .iter()
            .enumerate()
            .map(|(i, &key)| FeedbackSpec {
                key,
                slot: 1 + i as u32,
            })
            .collect(),
        delays: em.delays,
        units,
        store_len,
        order: order.iter().map(|&n| keys[n]).collect(),
        global_inputs: topology.inputs.count(),
        compensation,
        total_latency: total,
        audio_values,
        event_values,
        value_readers,
        nodes,
    };

    // ---- 8. verify --------------------------------------------------------
    #[cfg(debug_assertions)]
    if let Err(e) = verify::verify(&plan) {
        panic!("tutti-graph produced an unsound plan: {e}");
    }

    Ok((plan, delta))
}

/// Merge `refs` into one event stream, in `(offset, source order)`.
///
/// Up to [`MAX_PORTS`] sources is one `EventMerge` op. Wider fan-in becomes a
/// tree: contiguous runs of at most `MAX_PORTS` sources merge first, and the
/// run results merge in run order. Because each merge breaks ties toward the
/// lower input and the runs are contiguous and in order, a tie between
/// sources in different runs still goes to the earlier source — the tree
/// delivers exactly what one flat merge would.
fn merge_tree(em: &mut Emitted, refs: Vec<ERef>) -> ERef {
    match refs.len() {
        0 => ERef::Empty,
        1 => refs[0],
        n if n <= MAX_PORTS => {
            let op = em.push(Pre::EventMerge {
                srcs: refs.clone(),
                dst: 0,
            });
            for &r in &refs {
                em.read_eref(r, op);
            }
            let mv = em.event_value(op);
            if let Pre::EventMerge { dst, .. } = &mut em.pre[op as usize] {
                *dst = mv;
            }
            ERef::Val(mv)
        }
        _ => {
            let runs: Vec<ERef> = refs
                .chunks(MAX_PORTS)
                .map(|run| merge_tree(em, run.to_vec()))
                .collect();
            merge_tree(em, runs)
        }
    }
}

/// Fuse chains: op `b` joins its only predecessor's task when that
/// predecessor's only successor is `b`. Such an edge needs no activation
/// counter at all under a parallel executor — the task just runs on.
fn coarsen(preds: &[BTreeSet<u32>], succ: &[Vec<u32>]) -> (Vec<Span>, Vec<u32>, Csr, Vec<u32>) {
    let n = preds.len();
    let mut task_of = vec![u32::MAX; n];
    let mut members: Vec<Vec<u32>> = Vec::new();
    for op in 0..n {
        let fused = (preds[op].len() == 1)
            .then(|| *preds[op].iter().next().expect("len 1"))
            .filter(|&p| succ[p as usize].len() == 1);
        match fused {
            Some(p) => {
                let t = task_of[p as usize];
                task_of[op] = t;
                members[t as usize].push(op as u32);
            }
            None => {
                task_of[op] = members.len() as u32;
                members.push(vec![op as u32]);
            }
        }
    }
    let mut tasks = Vec::with_capacity(members.len());
    let mut task_ops = Vec::with_capacity(n);
    for m in &members {
        let start = task_ops.len() as u32;
        task_ops.extend_from_slice(m);
        tasks.push(Span {
            start,
            len: m.len() as u32,
        });
    }
    let mut rows: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); members.len()];
    for (a, ss) in succ.iter().enumerate() {
        for &b in ss {
            let (ta, tb) = (task_of[a], task_of[b as usize]);
            if ta != tb {
                rows[ta as usize].insert(tb);
            }
        }
    }
    let mut activation = vec![0u32; members.len()];
    for row in &rows {
        for &t in row {
            activation[t as usize] += 1;
        }
    }
    let rows: Vec<Vec<u32>> = rows.into_iter().map(|r| r.into_iter().collect()).collect();
    (tasks, task_ops, Csr::from_rows(&rows), activation)
}
