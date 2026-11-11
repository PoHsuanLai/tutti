//! The compiler: `compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)`.
//!
//! Pure: no I/O, no audio, no units — only the value, the shapes
//! the units declared, and the previous plan (for placements and nothing
//! else). The passes, in order:
//!
//! 1. **Check** every node's shape against its spec — widths, latency and
//!    tail must all agree — and every event edge's ports against the shapes.
//!    The shape is what the unit says about itself and the spec is what the
//!    value says; if they disagree, one of them is stale, and compiling
//!    either would make the latency folds over the value
//!    (`crate::latency_plan`) disagree with the plan. Every event
//!    edge marked with a required resolution is checked against its sink's
//!    declared `Shape::event_resolution`.
//! 2. **SCC** ([`order::scc`]) over direct audio + event dependencies. A cycle
//!    not broken by a feedback edge is [`CompileError::Cycle`], naming the
//!    edges. A feedback edge becomes a read of a slot the executor fills from
//!    the edge's own delay state before the block, and a `Capture` op
//!    that feeds the delay, so the op DAG stays acyclic.
//! 3. **Order** ([`order::kahn`]) — one deterministic topological sort.
//! 4. **Latency solve** — `arrival = max(departures)`, `departure = arrival +
//!    own`: `crate::delays`' solve, over the spec (its
//!    `LatencyGraph` impl lists event and param sources). **Event and param
//!    edges count toward arrival**, and **a `Source::Global` input is a
//!    merge-point source**, delayed to the node's arrival like any other.
//!
//!    Emits a `Delay` op per mismatched audio port and per mismatched event
//!    *source*, plus per-output alignment rings, with state keyed by
//!    [`DelayKey`]. An event fan-in merges its sources by `(offset, source
//!    port)` — ties go to the lower `(NodeKey, port)` source, whatever order
//!    the spec lists them in. One wider than [`MAX_PORTS`] becomes a tree
//!    of merges over contiguous source ranges, which keeps that rule
//!    exactly.
//! 5. **Emit ops** in the serial order, recording every value's writer and
//!    readers, and the op DAG.
//! 6. **Colour** ([`colour`]) — slot sharing that is correct under any
//!    parallel schedule, with in-place aliasing where legal.
//! 7. **Coarsen and price** ([`coarsen`]) — fuse single-successor /
//!    single-predecessor chains into tasks, pack sibling tasks too cheap to
//!    dispatch alone (never lengthening the critical path), fuse the chains
//!    that leaves, and total the
//!    cost model's estimates (`Plan::work`, `Plan::critical_path`; see
//!    `cost.rs`). The serial executor ignores tasks; the parallel one
//!    (`par.rs`) dispatches them, when `Plan::schedule` says spreading
//!    pays.
//! 8. **Verify** ([`verify`]) in debug builds.
//! 9. **Place** units: `NodeKey` → dense [`UnitIdx`], diffed against `prev`
//!    into a [`Delta`].
//!
//! Doc 013's step 6, the serial-vs-parallel decision, is
//! `Plan::schedule`: pure arithmetic on what step 7 totals, made when the
//! plan meets a pool (the pool's participant count is not the compiler's
//! to know).

mod coarsen;
mod colour;
mod order;
pub(crate) mod verify;

use std::collections::{BTreeMap, BTreeSet};

use crate::NodeKey;
use crate::NodeMix;
use crate::{Edge, EdgeId, FeedbackFrom, InPort, OutPort, Sink, Source};
use tutti_types::{ChannelLayout, Latency, Samples, Tail};

use crate::node::{InPlaceMask, MAX_PORTS, Prepare, Resolution, Shape};
use crate::param::ParamIn;
use crate::plan::{
    Csr, DelayKey, DelaySpec, Delta, EMPTY_SLOT, EventSlotCapacity, FeedbackKey, FeedbackSpec,
    GainPortOp, NodeTables, Op, ParamPortOp, Placement, Plan, PlanUnit, Span, UnitIdx, Value,
    ZERO_SLOT,
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
    /// A param edge from `from` into `at`. Param edges are always direct: a
    /// node cannot drive itself, or anything upstream of it.
    Param {
        /// The param port.
        at: ParamIn,
        /// The event output that drives it.
        from: EventOut,
    },
    /// A lane driving the gain of an edge into input port `at`, from `from`:
    /// it runs before the sum at the port, so before its node. Like a param
    /// edge, always direct. (The port and the source, not the whole edge's
    /// name, which would make every `CompileError` larger.)
    Gain {
        /// The port the weighted edge goes into.
        at: InPort,
        /// The event output that drives the gain.
        from: EventOut,
    },
}

/// Why a [`ValidGraph`] could not be compiled against these shapes.
///
/// This is the graph compiler's error; the modulation compiler's is
/// `tutti_modulation::CompileError`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The shape's latency disagrees with the spec's: one of the two is
    /// stale.
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
    /// An event edge marked as requiring a resolution (see
    /// [`GraphSpec::require_resolution`](crate::GraphSpec::require_resolution))
    /// targets a node that declares a coarser one: sample-accurate automation
    /// into a block-rate node, say.
    ResolutionTooCoarse {
        /// The sink port.
        at: EventIn,
        /// The source port.
        from: EventOut,
        /// What the edge requires.
        required: Resolution,
        /// What the sink declares.
        sink: Resolution,
    },
    /// A modulated param its node does not declare
    /// ([`Shape::params`](crate::Shape::params)).
    UnknownParam {
        /// The param port.
        at: ParamIn,
    },
    /// A param source names an event output its node does not have.
    ParamSourceOutOfRange {
        /// The param port.
        at: ParamIn,
        /// The source.
        from: EventOut,
    },
    /// An edge gain's source names an event output its node does not have.
    GainSourceOutOfRange {
        /// The sink of the edge whose gain it drives.
        sink: Sink,
        /// The source.
        from: EventOut,
    },
    /// A cycle that no feedback edge breaks. Every direct edge inside the
    /// cycle's strongly connected component is listed, in key order.
    Cycle {
        /// The edges.
        edges: Vec<CycleEdge>,
    },
    /// A bus edge could not be lowered against the shapes: a node's bus is
    /// gone or its topology no longer folds into the other's (a node replaced
    /// by one with another layout), or a lane overlaps an edge. Never a
    /// silent drop (doc 013, "Buses and coercion between declared layouts").
    Bus(crate::BusError),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(e) => write!(f, "{e}"),
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
            Self::ResolutionTooCoarse {
                at,
                from,
                required,
                sink,
            } => write!(
                f,
                "event edge from node {} port {} into node {} port {} requires {required:?} \
                 timing, but the sink honours only {sink:?}",
                from.node.0, from.port, at.node.0, at.port
            ),
            Self::UnknownParam { at } => write!(
                f,
                "node {} does not declare {:?} modulatable",
                at.node.0, at.param
            ),
            Self::ParamSourceOutOfRange { at, from } => write!(
                f,
                "param {:?} of node {}: source {from:?} is not an output of its node",
                at.param, at.node.0
            ),
            Self::GainSourceOutOfRange { sink, from } => write!(
                f,
                "gain of an edge into {sink:?}: source {from:?} is not an output of its node"
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
    Sum {
        srcs: Vec<ARef>,
        dst: u32,
    },
    /// A `Sum` with a weighted lane: `lanes` is one entry per addend.
    WeightedSum {
        srcs: Vec<ARef>,
        lanes: Vec<Option<EdgeId>>,
        /// The driven lanes among them.
        gains: Vec<GPort>,
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
        params: Vec<PPort>,
    },
    Output {
        channel: u16,
        src: ARef,
        delay: Option<u32>,
    },
    Tap {
        tap: u32,
        srcs: Vec<ARef>,
    },
    EventTap {
        tap: u32,
        src: u32,
    },
    Meter {
        meter: u32,
        srcs: Vec<ARef>,
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

/// A driven param port before colouring: its event source, and the event
/// value it reads.
struct PPort {
    port: u16,
    param: tutti_types::UnitAddr,
    from: EventOut,
    value: u32,
}

/// A driven lane of a weighted sum before colouring: which addend it is, its
/// event source, and the event value it reads (the sibling of [`PPort`]).
#[derive(Clone, Copy)]
struct GPort {
    lane: u16,
    from: EventOut,
    value: u32,
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

/// Compiles `graph` against the units' `shapes` into an immutable [`Plan`]
/// and the [`Delta`] from `prev`.
///
/// Pure: no I/O and no units, only the value, the shapes the units declared
/// and the `Prepare` they were prepared for. It checks each shape against
/// its node's spec, rejects cycles not broken by a feedback edge, orders the
/// nodes, solves latency compensation (inserting delays on the shorter
/// paths, for audio, events and params alike), assigns buffer slots, and
/// places units in the executor's store. Runs on the control thread and
/// allocates. [`Editor::commit`](crate::Editor::commit) calls it for you; call
/// it directly to inspect a plan or to drive [`Editor::package`](crate::Editor::package).
///
/// `prev` is the plan currently running, if any. It is read for exactly one
/// thing — which store index each surviving key already occupies — so that the
/// [`Delta`] moves only what changed. Delay-ring and feedback state carry over
/// by *key* at the runtime and need nothing from `prev`.
///
/// # Errors
///
/// Returns a [`CompileError`] when a node has no shape, a shape disagrees
/// with its spec, a port is out of range, a cycle has no feedback edge, a
/// feedback delay is shorter than the maximum block, a marked event edge
/// needs a finer resolution than its sink declares, or a limit is exceeded;
/// the variants name each case.
///
/// # Panics
///
/// In debug builds, if the plan it produced fails [`Plan::verify`]
/// — a compiler bug, never a property of the input.
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
        if (spec.inputs, spec.outputs) != (shape.audio_in(), shape.audio_out()) {
            return Err(CompileError::WidthMismatch {
                node: key,
                declared: (spec.inputs, spec.outputs),
                shape: (shape.audio_in(), shape.audio_out()),
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
            shape.audio_in().count() as usize,
            shape.audio_out().count() as usize,
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

    // Params: each is declared by its node, and each source is an output of
    // its own.
    for (&at, m) in graph.params() {
        if at.param != NodeMix::ADDR
            && node_shapes[dense[&at.node]]
                .params
                .index_of(at.param)
                .is_none()
        {
            return Err(CompileError::UnknownParam { at });
        }
        if let Some(from) = m.source
            && from.port >= node_shapes[dense[&from.node]].event_out
        {
            return Err(CompileError::ParamSourceOutOfRange { at, from });
        }
    }
    // Edge gains: each driving source is an output of its node.
    for (&id, gain) in graph.gains() {
        if let Some(from) = gain.source
            && from.port >= node_shapes[dense[&from.node]].event_out
        {
            return Err(CompileError::GainSourceOutOfRange {
                sink: id.sink,
                from,
            });
        }
    }

    // Resolution: a marked edge's sink must honour what it requires (see
    // `GraphSpec::require_resolution` for the rule, and why only marked edges).
    for (&(at, from), &required) in graph.required_resolution() {
        let sink = node_shapes[dense[&at.node]].event_resolution;
        if !sink.honours(required) {
            return Err(CompileError::ResolutionTooCoarse {
                at,
                from,
                required,
                sink,
            });
        }
    }

    // Feedback delays: at least one block, or a read would need the future.
    let max_block = prepare.max_block().samples();
    for (&at, e) in topology
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
        if let Edge::Feedback(f) = *e
            && f.delay < max_block
        {
            return Err(CompileError::FeedbackTooShort {
                edge: CycleEdge::Audio(at),
                delay: f.delay,
                max_block,
            });
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Feedback { from, delay } = *e
                && delay < max_block
            {
                return Err(CompileError::FeedbackTooShort {
                    edge: CycleEdge::Event { at, from },
                    delay,
                    max_block,
                });
            }
        }
    }

    // ---- 2. cycles --------------------------------------------------------
    // Direct dependencies, with multiplicity (for Kahn) and labelled (for the
    // error).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    let mut labelled: Vec<(usize, usize, CycleEdge)> = Vec::new();
    for (&at, edge) in topology
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
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
    for (&at, m) in graph.params() {
        if let Some(from) = m.source {
            let (src, d) = (dense[&from.node], dense[&at.node]);
            preds[d].push(src);
            labelled.push((src, d, CycleEdge::Param { at, from }));
        }
    }
    // A lane that drives the gain of an edge into a node's port runs before
    // the sum at that port, so before the node. (One into an output channel
    // feeds nothing a node reads.)
    for (&id, gain) in graph.gains() {
        if let (Sink::Port(at), Some(from)) = (id.sink, gain.source) {
            let (src, d) = (dense[&from.node], dense[&at.node]);
            preds[d].push(src);
            labelled.push((src, d, CycleEdge::Gain { at, from }));
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
    // One solve: `crate::delays` over the checked graph. The
    // compiler places its delays itself (event and param edges included) from
    // the solve's arrivals.
    let solved = crate::delays(graph);
    let arrival: Vec<Latency> = keys
        .iter()
        .map(|&k| Latency::new(solved.arrival(k)))
        .collect();
    let departures: Vec<Latency> = keys
        .iter()
        .map(|&k| Latency::new(solved.departure(k)))
        .collect();
    let departure = |n: usize| departures[n];
    let total = Latency::new(solved.compensation().total());
    let compensation: Vec<Samples> = (0..topology.outputs.len())
        .map(|ch| solved.compensation().for_channel(ch))
        .collect();

    // ---- 5. emit ----------------------------------------------------------
    let audio_fb_key = |f: FeedbackFrom| FeedbackKey::Audio {
        from: f.from,
        generation: graph.generation(f.from.node),
        delay: f.delay,
    };
    let event_fb_key = |at: EventIn, from: EventOut, delay: Samples| FeedbackKey::Event {
        at,
        from,
        generation: graph.generation(from.node),
        delay,
    };
    let audio_fb: Vec<FeedbackKey> = topology
        .edges
        .values()
        .flatten()
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
        .flatten()
        .filter_map(|e| match *e {
            Edge::Direct(Source::Global(ch)) => Some(ch),
            _ => None,
        })
        .chain(topology.outputs.iter().flatten().filter_map(|s| match *s {
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

        // Audio inputs: every source of a port through its own PDC delay
        // when it needs one, then the sum in **source order** (`Edge`'s `Ord`,
        // not the order the spec listed them in), so two specs that list the
        // same sources differently render alike (doc 013, "Audio fan-in
        // sums", point 2). One source is the value itself, none is silence.
        let mut ain = Vec::with_capacity(shape.audio_in().count() as usize);
        for port in 0..shape.audio_in().count() {
            let at = InPort { node: key, port };
            let mut sources: Vec<Edge> = topology.edges.get(&at).cloned().unwrap_or_default();
            sources.sort();
            let mut refs: Vec<Addend> = Vec::with_capacity(sources.len());
            for edge in sources {
                let r = match edge {
                    // Silence adds nothing, bit for bit (the sum starts at
                    // `+0.0`); `validate` refuses it in a set, so this is a
                    // raw write's.
                    Edge::Direct(Source::Zero) => continue,
                    Edge::Direct(from @ (Source::Global(_) | Source::Node(_))) => {
                        // A global input arrives at zero: it is a merge-point
                        // source like any other (see the module docs, step 4).
                        let (v, dep) = match from {
                            Source::Global(ch) => (global_val[&ch], Latency::ZERO),
                            Source::Node(p) => (audio_out_val[&p], departure(dense[&p.node])),
                            Source::Zero => unreachable!("matched above"),
                        };
                        align_audio(
                            &mut em,
                            v,
                            dep.gap_to(arrival[n]),
                            DelayKey::Audio { at, from },
                        )
                    }
                    Edge::Feedback(f) => ARef::Fb(fb_index(audio_fb_key(f), &audio_fb)),
                };
                // An edge with a gain entry is a weighted lane (doc 013,
                // "Edge gain").
                let id = EdgeId {
                    sink: Sink::Port(at),
                    from: edge,
                };
                let gain = graph.gains().get(&id);
                // Its driving lane, aligned to this node's arrival like a
                // param's source (a silent addend has none: nothing is added).
                let drive = gain
                    .and_then(|g| g.source)
                    .filter(|_| r != ARef::Zero)
                    .map(|from| {
                        let by = departure(dense[&from.node]).gap_to(arrival[n]);
                        GainDrive {
                            from,
                            value: align_event(
                                &mut em,
                                event_out_val[&from],
                                by,
                                DelayKey::EdgeGain { id, from },
                            ),
                        }
                    });
                refs.push(Addend {
                    r,
                    lane: gain.map(|_| id),
                    drive,
                });
            }
            ain.push(sum_chain(&mut em, refs));
        }

        // Event inputs: delay each source that needs it, then merge fan-in
        // in **source order**, `(source NodeKey, source port)` — not the
        // order the edges were listed in. The
        // tie-break at equal offsets is then a property of the wiring alone:
        // two specs that list the same sources differently merge alike, so
        // a host that rebuilds its spec from an unordered store (the ECS)
        // cannot reorder a chord by accident. Sources are distinct
        // (`GraphSpec::validate`), so the order is total.
        let mut ein = Vec::with_capacity(shape.event_in as usize);
        for port in 0..shape.event_in {
            let at = EventIn { node: key, port };
            let mut sources: Vec<EventEdge> = graph.events().get(&at).cloned().unwrap_or_default();
            sources.sort_by_key(|e| e.from());
            let mut refs: Vec<ERef> = Vec::with_capacity(sources.len());
            for e in &sources {
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

        // Driven params, in port order: each source through its PDC delay
        // when it needs one, exactly as an input is (see the `param` module
        // docs).
        let mut params: Vec<PPort> = Vec::new();
        for (k, &param) in shape.params.as_slice().iter().enumerate() {
            let at = ParamIn { node: key, param };
            let Some(e) = graph.params().get(&at).and_then(|m| m.source) else {
                continue;
            };
            let value = align_event(
                &mut em,
                event_out_val[&e],
                departure(dense[&e.node]).gap_to(arrival[n]),
                DelayKey::ParamEvent { at, from: e },
            );
            params.push(PPort {
                port: k as u16,
                param,
                from: e,
                value,
            });
        }
        // The node's mix, last: its port is past every declared index. Its
        // source is aligned like any param's (the same key kind: the mix is a
        // `ParamIn` of the node).
        let at = ParamIn {
            node: key,
            param: NodeMix::ADDR,
        };
        if let Some(e) = graph.params().get(&at).and_then(|m| m.source) {
            let value = align_event(
                &mut em,
                event_out_val[&e],
                departure(dense[&e.node]).gap_to(arrival[n]),
                DelayKey::ParamEvent { at, from: e },
            );
            params.push(PPort {
                port: ParamPortOp::MIX_PORT,
                param: NodeMix::ADDR,
                from: e,
                value,
            });
        }
        let reads: Vec<u32> = params.iter().map(|p| p.value).collect();

        let op = em.push(Pre::Node {
            unit: n as u32,
            ain: ain.clone(),
            aout: Vec::new(),
            ein: ein.clone(),
            eout: Vec::new(),
            params,
        });
        for &r in &ain {
            em.read_aref(r, op);
        }
        for &r in &ein {
            em.read_eref(r, op);
        }
        for v in reads {
            em.read_event(v, op);
        }
        let aout: Vec<u32> = (0..shape.audio_out().count())
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

    // A master meter or an outputs tap reads each output channel as it leaves
    // the alignment delay, so with one declared the delay is its own op
    // writing a slot that the `Output` op and the reader both read (one ring,
    // the same key).
    let master_meter = topology
        .meters
        .values()
        .any(|spec| matches!(spec, crate::MeterSpec::Outputs))
        || topology
            .taps
            .values()
            .any(|spec| matches!(spec, crate::TapSpec::Outputs { .. }));
    let mut output_refs: Vec<ARef> = Vec::with_capacity(topology.outputs.len());
    for (channel, sources) in topology.outputs.iter().enumerate() {
        let channel = channel as u16;
        let mut sources: Vec<Source> = sources
            .iter()
            .copied()
            .filter(|s| *s != Source::Zero)
            .collect();
        sources.sort();
        let lane_of = |source: Source| {
            let id = EdgeId {
                sink: Sink::Output(channel),
                from: Edge::Direct(source),
            };
            graph.gains().contains_key(&id).then_some(id)
        };
        if sources.len() > 1 || sources.iter().any(|&s| lane_of(s).is_some()) {
            // A summed channel (or one whose source is weighted): each source
            // is delayed to the output time on its own, as an op of its own,
            // then summed; the `Output` op carries no ring (doc 013, "Audio
            // fan-in sums", point 3).
            let refs: Vec<Addend> = sources
                .iter()
                .map(|&source| {
                    let (v, dep) = match source {
                        Source::Node(p) => (audio_out_val[&p], departure(dense[&p.node])),
                        Source::Global(ch) => (global_val[&ch], Latency::ZERO),
                        Source::Zero => unreachable!("filtered above"),
                    };
                    let r = align_audio(
                        &mut em,
                        v,
                        dep.gap_to(total),
                        DelayKey::Output {
                            channel,
                            from: source,
                        },
                    );
                    let lane = lane_of(source);
                    // Its driving lane, aligned to the output time.
                    let drive = lane
                        .and_then(|id| Some((id, graph.gains().get(&id)?.source?)))
                        .map(|(id, from)| GainDrive {
                            from,
                            value: align_event(
                                &mut em,
                                event_out_val[&from],
                                departure(dense[&from.node]).gap_to(total),
                                DelayKey::EdgeGain { id, from },
                            ),
                        });
                    Addend { r, lane, drive }
                })
                .collect();
            let src = sum_chain(&mut em, refs);
            output_refs.push(src);
            let op = em.push(Pre::Output {
                channel,
                src,
                delay: None,
            });
            em.read_aref(src, op);
            continue;
        }
        let source = &sources.first().copied().unwrap_or(Source::Zero);
        let mut src = match *source {
            Source::Node(p) => ARef::Val(audio_out_val[&p]),
            Source::Global(ch) => ARef::Val(global_val[&ch]),
            Source::Zero => ARef::Zero,
        };
        let d = compensation[channel as usize];
        // Delaying silence is silence: a zero channel reports its
        // compensation (as `latency::plan` does) but gets no ring.
        let mut delay = (!d.is_zero() && src != ARef::Zero).then(|| {
            em.delays.push(DelaySpec {
                key: DelayKey::Output {
                    channel,
                    from: *source,
                },
                len: d,
            });
            (em.delays.len() - 1) as u32
        });
        if let (true, Some(ring), ARef::Val(v)) = (master_meter, delay, src) {
            let op = em.push(Pre::Delay {
                delay: ring,
                src: v,
                dst: 0,
            });
            em.read_audio(v, op);
            let dv = em.audio_value(op);
            if let Pre::Delay { dst, .. } = &mut em.pre[op as usize] {
                *dst = dv;
            }
            src = ARef::Val(dv);
            delay = None;
        }
        output_refs.push(src);
        let op = em.push(Pre::Output {
            channel,
            src,
            delay,
        });
        em.read_aref(src, op);
    }

    // Taps: one lane per source, any ports. An aligned lane is delayed to the
    // graph's output time (the compensation an output of the same source
    // gets); a raw one reads the source as it leaves its node. A closed tap
    // is not in the topology, so nothing here is emitted for it. A lane whose
    // port does not exist (a node replaced by a narrower one, a removed node,
    // a global input past the declared width) reads silence rather than
    // failing the commit, as a meter's does; it gets no delay ring, and the
    // other lanes' rings are keyed by their own lane and source.
    let mut tap_ids: Vec<crate::TapId> = Vec::new();
    for (&tap, spec) in &topology.taps {
        let (sources, align) = match spec {
            crate::TapSpec::Outputs { width } => {
                // What each output channel carries, post-sum and aligned
                // (above); a lane past the output count is silence.
                let srcs: Vec<ARef> = (0..*width)
                    .map(|c| output_refs.get(c).copied().unwrap_or(ARef::Zero))
                    .collect();
                let op = em.push(Pre::Tap {
                    tap: tap_ids.len() as u32,
                    srcs: srcs.clone(),
                });
                for src in srcs {
                    em.read_aref(src, op);
                }
                tap_ids.push(tap);
                continue;
            }
            crate::TapSpec::Ports { sources, align } => (sources, *align),
        };
        let mut srcs = Vec::with_capacity(sources.len());
        for (lane, source) in sources.iter().enumerate() {
            let (mut src, dep) = match *source {
                Source::Node(p) => match (audio_out_val.get(&p), dense.get(&p.node)) {
                    (Some(&v), Some(&n)) => (ARef::Val(v), departure(n)),
                    _ => (ARef::Zero, Latency::ZERO),
                },
                Source::Global(ch) => match global_val.get(&ch) {
                    Some(&v) => (ARef::Val(v), Latency::ZERO),
                    None => (ARef::Zero, Latency::ZERO),
                },
                Source::Zero => (ARef::Zero, Latency::ZERO),
            };
            if align == crate::TapAlign::ToOutput && src != ARef::Zero {
                let d = dep.gap_to(total);
                if !d.is_zero() {
                    let ARef::Val(v) = src else {
                        unreachable!("not zero")
                    };
                    let delay = em.delays.len() as u32;
                    em.delays.push(DelaySpec {
                        key: DelayKey::Tap {
                            tap,
                            lane,
                            from: *source,
                        },
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
                    src = ARef::Val(dv);
                }
            }
            srcs.push(src);
        }
        // One op a tap, after every lane's delay: it writes the lanes together.
        let op = em.push(Pre::Tap {
            tap: tap_ids.len() as u32,
            srcs: srcs.clone(),
        });
        for src in srcs {
            em.read_aref(src, op);
        }
        tap_ids.push(tap);
    }

    // Event taps: one event output each. An aligned tap is delayed to the
    // graph's output time (the compensation an event edge into an output-time
    // sink would get); a raw one reads the port as it leaves its node. A closed
    // tap is not in the topology, so nothing is emitted for it. A tap whose
    // port does not exist (a node replaced by one with fewer event outputs, a
    // removed node) keeps its place in the table, so its writer is carried and
    // resumes if the port returns, but it gets no op and no delay ring: it
    // reads nothing and the commit succeeds. One op a tap, after its delay.
    let mut event_tap_ids: Vec<crate::EventTapId> = Vec::new();
    for (&tap, spec) in &topology.event_taps {
        let index = event_tap_ids.len() as u32;
        event_tap_ids.push(tap);
        let Some(from) = spec.source else {
            continue;
        };
        let (Some(&v), Some(&n)) = (event_out_val.get(&from), dense.get(&from.node)) else {
            continue;
        };
        let mut src = v;
        if spec.align == crate::TapAlign::ToOutput {
            let d = departure(n).gap_to(total);
            if !d.is_zero() {
                let delay = em.delays.len() as u32;
                em.delays.push(DelaySpec {
                    key: DelayKey::EventTap { tap, from },
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
                src = dv;
            }
        }
        let op = em.push(Pre::EventTap { tap: index, src });
        em.read_event(src, op);
    }

    // Meters: one lane per source. `Outputs` reads what each output channel
    // carries (after its alignment delay, above); `Ports` reads each source as
    // it leaves its node, or, aligned, delayed to the output time by a ring of
    // its own like a tap's. A lane whose port does not exist (a node replaced
    // by a narrower one, a removed node) reads silence rather than failing the
    // commit. One op a meter, so its lanes are folded together. A closed meter
    // is not in the topology.
    let mut meter_ids: Vec<(crate::MeterId, usize)> = Vec::new();
    for (&meter, spec) in &topology.meters {
        let srcs: Vec<ARef> = match spec {
            crate::MeterSpec::Outputs => output_refs.clone(),
            crate::MeterSpec::Ports { sources, align } => {
                let mut srcs = Vec::with_capacity(sources.len());
                for (lane, source) in sources.iter().enumerate() {
                    let (mut src, dep) = match *source {
                        Source::Node(p) => match (audio_out_val.get(&p), dense.get(&p.node)) {
                            (Some(&v), Some(&n)) => (ARef::Val(v), departure(n)),
                            _ => (ARef::Zero, Latency::ZERO),
                        },
                        Source::Global(ch) => match global_val.get(&ch) {
                            Some(&v) => (ARef::Val(v), Latency::ZERO),
                            None => (ARef::Zero, Latency::ZERO),
                        },
                        Source::Zero => (ARef::Zero, Latency::ZERO),
                    };
                    if *align == crate::TapAlign::ToOutput
                        && let ARef::Val(v) = src
                    {
                        let d = dep.gap_to(total);
                        if !d.is_zero() {
                            let delay = em.delays.len() as u32;
                            em.delays.push(DelaySpec {
                                key: DelayKey::Meter {
                                    meter,
                                    lane,
                                    from: *source,
                                },
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
                            src = ARef::Val(dv);
                        }
                    }
                    srcs.push(src);
                }
                srcs
            }
        };
        let op = em.push(Pre::Meter {
            meter: meter_ids.len() as u32,
            srcs: srcs.clone(),
        });
        meter_ids.push((meter, srcs.len()));
        for src in srcs {
            em.read_aref(src, op);
        }
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
            // The sum accumulates into its first addend's slot when that
            // addend is read nowhere else later (the colouring's own test).
            Pre::Sum { srcs, dst } | Pre::WeightedSum { srcs, dst, .. } => {
                if let Some(&ARef::Val(u)) = srcs.first() {
                    in_place.insert(*dst, u);
                }
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

    // Event slot capacities (`EventSlotCapacity`): a node output holds what
    // its shape declares, a delay output (and a feedback slot) all its FIFO
    // can hold at its source's declared rate — what can fall due in one
    // block — and a merge all its inputs together, so a merge can never drop
    // an event (a note-off least of all).
    // Emitted in topological order, so a merge's inputs are priced first.
    let block = prepare.max_block().get();
    let fb_cap = |key: &FeedbackKey| match *key {
        FeedbackKey::Event { from, delay, .. } => {
            EventSlotCapacity::fifo(node_shapes[dense[&from.node]].event_capacity, delay, block)
        }
        _ => unreachable!("event feedback keys are events"),
    };
    let mut value_cap = vec![EventSlotCapacity::NONE; em.event.len()];
    for pre in &em.pre {
        match pre {
            Pre::Node { unit, eout, .. } => {
                let port = EventSlotCapacity::port(node_shapes[*unit as usize].event_capacity);
                for &v in eout {
                    value_cap[v as usize] = port;
                }
            }
            Pre::EventDelay { delay, dst, .. } => {
                let d = &em.delays[*delay as usize];
                let (DelayKey::Event { from, .. }
                | DelayKey::ParamEvent { from, .. }
                | DelayKey::EdgeGain { from, .. }
                | DelayKey::EventTap { from, .. }) = d.key
                else {
                    unreachable!("an event delay is keyed as events")
                };
                value_cap[*dst as usize] = EventSlotCapacity::fifo(
                    node_shapes[dense[&from.node]].event_capacity,
                    d.len,
                    block,
                );
            }
            Pre::EventMerge { srcs, dst } => {
                value_cap[*dst as usize] = srcs.iter().fold(EventSlotCapacity::NONE, |acc, r| {
                    acc.plus(match *r {
                        ERef::Val(v) => value_cap[v as usize],
                        ERef::Fb(f) => fb_cap(&event_fb[f as usize]),
                        ERef::Empty => EventSlotCapacity::NONE,
                    })
                });
            }
            _ => {}
        }
    }
    let mut event_slot_capacity =
        vec![EventSlotCapacity::NONE; (event_fixed + event_colour.count) as usize];
    for (f, key) in event_fb.iter().enumerate() {
        event_slot_capacity[1 + f] = fb_cap(key);
    }
    for (v, &c) in value_cap.iter().enumerate() {
        let s = eslot(v as u32) as usize;
        event_slot_capacity[s] = event_slot_capacity[s].covering(c);
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
    let mut param_ports: Vec<ParamPortOp> = Vec::new();
    let mut sum_lanes: Vec<Option<EdgeId>> = Vec::new();
    let mut gain_ports: Vec<GainPortOp> = Vec::new();
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
            Pre::Sum { srcs, dst } => Op::Sum {
                srcs: span_of(&mut audio_list, &mut srcs.iter().map(|&r| aref_slot(r))),
                dst: aslot(*dst),
            },
            Pre::WeightedSum {
                srcs,
                lanes,
                gains,
                dst,
            } => {
                let lane_start = sum_lanes.len() as u32;
                sum_lanes.extend(lanes.iter().copied());
                let gain_start = gain_ports.len() as u32;
                gain_ports.extend(gains.iter().map(|g| GainPortOp {
                    lane: g.lane,
                    from: g.from,
                    slot: eslot(g.value),
                }));
                Op::WeightedSum {
                    srcs: span_of(&mut audio_list, &mut srcs.iter().map(|&r| aref_slot(r))),
                    dst: aslot(*dst),
                    lanes: Span {
                        start: lane_start,
                        len: lanes.len() as u32,
                    },
                    gains: Span {
                        start: gain_start,
                        len: gains.len() as u32,
                    },
                }
            }
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
                params,
            } => {
                let pstart = param_ports.len() as u32;
                for p in params {
                    param_ports.push(ParamPortOp {
                        port: p.port,
                        param: p.param,
                        from: p.from,
                        slot: eslot(p.value),
                    });
                }
                let params = Span {
                    start: pstart,
                    len: param_ports.len() as u32 - pstart,
                };
                let mut mask = InPlaceMask::NONE;
                for (c, (&r, &o)) in ain.iter().zip(aout).enumerate() {
                    if let ARef::Val(u) = r
                        && aslot(u) == aslot(o)
                    {
                        mask = mask.with(c);
                    }
                }
                Op::Node {
                    unit: *unit,
                    audio_in: span_of(&mut audio_list, &mut ain.iter().map(|&r| aref_slot(r))),
                    audio_out: span_of(&mut audio_list, &mut aout.iter().map(|&v| aslot(v))),
                    event_in: span_of(&mut event_list, &mut ein.iter().map(|&r| eref_slot(r))),
                    event_out: span_of(&mut event_list, &mut eout.iter().map(|&v| eslot(v))),
                    in_place: mask,
                    params,
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
            Pre::Tap { tap, srcs } => Op::Tap {
                tap: *tap,
                srcs: span_of(&mut audio_list, &mut srcs.iter().map(|&r| aref_slot(r))),
            },
            Pre::EventTap { tap, src } => Op::EventTap {
                tap: *tap,
                src: eslot(*src),
            },
            Pre::Meter { meter, srcs } => Op::Meter {
                meter: *meter,
                srcs: span_of(&mut audio_list, &mut srcs.iter().map(|&r| aref_slot(r))),
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

    // ---- 7. coarsen, and price the plan (the cost model) -----------------
    let op_work: Vec<crate::cost::Work> = ops
        .iter()
        .map(|op| crate::cost::op_work(op, &node_shapes, &sum_lanes))
        .collect();
    let coarse = coarsen::coarsen(
        &em.preds,
        &succ_rows,
        &op_work,
        prepare.max_block().samples(),
    );

    // ---- 9. place ---------------------------------------------------------
    let prev_units: BTreeMap<NodeKey, PlanUnit> = prev
        .map(|p| p.units.iter().map(|u| (u.key, *u)).collect())
        .unwrap_or_default();
    let mut delta = Delta::default();
    let mut idx_of: BTreeMap<NodeKey, UnitIdx> = BTreeMap::new();
    let mut taken: BTreeSet<u32> = BTreeSet::new();
    for &key in &keys {
        let generation = graph.generation(key);
        if let Some(old) = prev_units.get(&key) {
            idx_of.insert(key, old.idx);
            taken.insert(old.idx.0);
            if old.generation != generation {
                delta.replace.push((
                    Placement {
                        key,
                        generation: old.generation,
                        idx: old.idx,
                    },
                    Placement {
                        key,
                        generation,
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
            generation: graph.generation(key),
            idx,
        });
    }
    for (key, old) in &prev_units {
        if !topology.nodes.contains_key(key) {
            delta.retire.push(Placement {
                key: *key,
                generation: old.generation,
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
            generation: graph.generation(key),
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
        tasks: coarse.tasks,
        task_ops: coarse.task_ops,
        task_succ: coarse.succ,
        task_activation: coarse.activation,
        work: coarse.work,
        critical: coarse.critical,
        task_width: coarse.width,
        audio_slots: audio_fixed + audio_colour.count,
        event_slots: event_fixed + event_colour.count,
        event_slot_capacity,
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
        taps: tap_ids,
        event_taps: event_tap_ids,
        meters: meter_ids,
        event_edges: {
            let mut v: Vec<(EventOut, EventIn)> = graph
                .events()
                .iter()
                .flat_map(|(&at, edges)| edges.iter().map(move |e| (e.from(), at)))
                .collect();
            v.sort();
            v
        },
        units,
        store_len,
        order: order.iter().map(|&n| keys[n]).collect(),
        global_inputs: topology.inputs.count(),
        compensation,
        total_latency: total,
        audio_values,
        event_values,
        value_readers,
        param_ports,
        gain_ports,
        sum_lanes,
        nodes,
    };

    // ---- 8. verify --------------------------------------------------------
    #[cfg(debug_assertions)]
    if let Err(e) = verify::verify(&plan) {
        panic!("tutti-graph produced an unsound plan: {e}");
    }

    Ok((plan, delta))
}

/// `v` through its PDC delay of `by` frames, keyed `key`: the value itself
/// when `by` is zero (no op), else the output of a `Pre::Delay` ring.
fn align_audio(em: &mut Emitted, v: u32, by: Samples, key: DelayKey) -> ARef {
    if by.is_zero() {
        return ARef::Val(v);
    }
    let delay = em.delays.len() as u32;
    em.delays.push(DelaySpec { key, len: by });
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

/// Event value `v`, delayed by `by` through a ring keyed `key` when `by` is
/// not zero (the value itself otherwise).
fn align_event(em: &mut Emitted, v: u32, by: Samples, key: DelayKey) -> u32 {
    if by.is_zero() {
        return v;
    }
    let delay = em.delays.len() as u32;
    em.delays.push(DelaySpec { key, len: by });
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
    dv
}

/// One addend of a sum: the value, and the edge whose gain weights it when
/// the edge has an entry (doc 013, "Edge gain").
#[derive(Clone, Copy)]
struct Addend {
    r: ARef,
    lane: Option<EdgeId>,
    /// The event source that drives the lane's gain, and the event value
    /// that carries it to the sum (through its PDC delay when it needs one).
    drive: Option<GainDrive>,
}

/// The event source that drives an addend's lane, and the event value that
/// carries it to the sum.
#[derive(Clone, Copy)]
struct GainDrive {
    from: EventOut,
    value: u32,
}

/// Sum `refs` (already in source order) into one audio value: the left fold
/// `((+0.0 + a_0) + a_1) + ..` (doc 013, "Audio fan-in sums", point 2), each
/// weighted addend `acc + w * a` (doc 013, "Edge gain").
///
/// Silence (`ARef::Zero`) adds nothing and is dropped; none left is silence,
/// one plain one is itself (an alias, no op). A weighted one is an op even
/// alone (`+0.0 + w * a`: a plain alias keeps a `-0.0` and a weight does not).
/// Up to [`MAX_PORTS`] addends are one op. A wider fan-in is a **chain, not a
/// tree**: a float sum is not associative, so the first op folds the first
/// `MAX_PORTS` and each next one reads the last result as its first addend
/// (plain: its weight is `1.0`) and adds the following `MAX_PORTS - 1`. The
/// accumulator is never `-0.0`, so adding it to `+0.0` again changes no bit:
/// the chain is one flat fold, wherever it is cut. An op with no weighted
/// lane is a plain `Op::Sum`.
fn sum_chain(em: &mut Emitted, refs: Vec<Addend>) -> ARef {
    let refs: Vec<Addend> = refs.into_iter().filter(|a| a.r != ARef::Zero).collect();
    let emit = |em: &mut Emitted, addends: Vec<Addend>| -> ARef {
        let srcs: Vec<ARef> = addends.iter().map(|a| a.r).collect();
        let lanes: Vec<Option<EdgeId>> = addends.iter().map(|a| a.lane).collect();
        let gains: Vec<GPort> = addends
            .iter()
            .enumerate()
            .filter_map(|(lane, a)| {
                a.drive.map(|d| GPort {
                    lane: lane as u16,
                    from: d.from,
                    value: d.value,
                })
            })
            .collect();
        let op = em.push(if lanes.iter().any(Option::is_some) {
            Pre::WeightedSum {
                srcs: srcs.clone(),
                lanes,
                gains: gains.clone(),
                dst: 0,
            }
        } else {
            Pre::Sum {
                srcs: srcs.clone(),
                dst: 0,
            }
        });
        for &r in &srcs {
            em.read_aref(r, op);
        }
        // The sum reads each driving lane's events: a read of the op, so the
        // colouring and the verifier cover it.
        for g in &gains {
            em.read_event(g.value, op);
        }
        let v = em.audio_value(op);
        if let Pre::Sum { dst, .. } | Pre::WeightedSum { dst, .. } = &mut em.pre[op as usize] {
            *dst = v;
        }
        ARef::Val(v)
    };
    match refs.len() {
        0 => ARef::Zero,
        1 if refs[0].lane.is_none() => refs[0].r,
        len => {
            let first = len.min(MAX_PORTS);
            let mut acc = emit(em, refs[..first].to_vec());
            for run in refs[first..].chunks(MAX_PORTS - 1) {
                let mut addends = Vec::with_capacity(run.len() + 1);
                addends.push(Addend {
                    r: acc,
                    lane: None,
                    drive: None,
                });
                addends.extend_from_slice(run);
                acc = emit(em, addends);
            }
            acc
        }
    }
}

/// Merge `refs` (already in source order) into one event stream, in
/// `(offset, source order)`.
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
