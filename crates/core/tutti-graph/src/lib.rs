//! The native audio graph: a pure compiler from a graph **value** to an
//! immutable [`Plan`], a serial executor for plans, and the naive
//! [`Reference`] interpreter the executor is proven against.
//!
//! This is Phase 1 of `docs/design/013-native-graph.md` (PR #2): the road off
//! fundsp's `Net` runtime. Nothing in the engine uses it yet — Phase 2 puts it
//! behind `Engine`, Phase 3 flips the Bevy adapter — so it can be read, tested
//! and benchmarked on its own. Of Phase 2 it already has the sample-accuracy
//! contract's type-level half (doc 013 §6: [`Offset`] vs `Frame`,
//! timestamped commands, [`Io::sub_blocks`], [`Resolution`]) and live
//! re-preparation ([`Editor::reprepare`]).
//!
//! # The four layers (doc 013 §"The design")
//!
//! ```text
//!  GraphSpec (value)   Topology + event edges + unit generations   pure, Eq + Hash
//!      │ validate()
//!  ValidGraph          proof in the type
//!      │ compile(&ValidGraph, &Shapes, prev) -> (Plan, Delta)      pure, control thread
//!  Plan (SoA)          ops, slots, CSR DAG, delay + feedback tables
//!      │ commit box { plan, delta, units }  ──SPSC──▶  and back with retirees
//!  Executor            unit store + arena, serial walk of the ops
//! ```
//!
//! # Invariants carried by types
//!
//! Each of these was a bug class somewhere in the engine; here it is a type:
//!
//! - **[`Latency`](tutti_types::Latency)** — a node's declared processing
//!   latency. A delay time is a `Samples` or a `Seconds` and cannot be passed
//!   as one (doc 013 defects D1–D3).
//! - **[`MaxBlock`]** — obtainable only from [`Prepare`]; every [`Io`] is at
//!   most that long, enforced where `Io` is built, so a node never clamps
//!   (defect D4).
//! - **[`Offset`] vs [`Frame`](tutti_types::Frame)** — an event carries a
//!   position *inside its block*, which is a different type from an absolute
//!   frame; the two convert only through the block's [`Env`]
//!   ([`offset_of`](Env::offset_of), [`frame_at`](Env::frame_at)), so the
//!   off-by-a-block bug does not compile (doc 013 §6, item 1).
//! - **[`SortedEvents`]** — an event slice that is sorted and inside the
//!   block by construction.
//! - **[`Io::sub_blocks`]** — the block split at event offsets, so a node
//!   written against it is sample-accurate by construction (item 4).
//! - **[`At`](tutti_types::At)** — every scheduled command
//!   ([`Editor::schedule`]) says when: a frame, a beat, or an explicit
//!   `NextBlock`. Late commands land at the next block and are counted, never
//!   dropped (item 3).
//! - **[`Resolution`]** — each node declares how finely it honours event
//!   offsets ([`Shape::event_resolution`]), and an event edge marked with
//!   [`GraphSpec::require_resolution`] into a node that cannot honour it is
//!   [`CompileError::ResolutionTooCoarse`] (item 5).
//! - **[`ParamRamp`]** — built from a typed `ParamKey<U>` and read back as a
//!   `U`; the raw `f32` in between is private.
//! - **The commit box and the unit box**, both crate-private — everything
//!   that crosses to the executor travels over the editor/executor queue
//!   pair and is freed on the control side; a drop while the executor runs
//!   or applies panics in debug builds. No caller ever holds a box, so none
//!   can be dropped unapplied or applied out of order (see [`Editor`]).
//!   (`tutti_types::Retire` is the generic, move-only form of the same
//!   guard.)
//! - **`FeedbackFrom::delay`** — a feedback loop's length is part of the
//!   graph, so a bounce at a larger `MaxBlock` loops like playback; a delay
//!   shorter than `MaxBlock` is [`CompileError::FeedbackTooShort`].
//!
//! # Whole blocks, and loop wraps
//!
//! The executor hands every node the **whole block** — it never splits one,
//! not at event offsets (sub-chunking at an event is the node's job) and not
//! at a transport loop wrap. That is what keeps an out-of-process plugin's
//! declared pipeline latency constant. A wrap inside a block is visible to
//! the nodes that care through [`Transport::looping`] and the block-start
//! beat in [`Env`], from which a node computes where the wrap falls.
//!
//! # Precision
//!
//! The graph is `f32` by decision (doc 013, owner decision 2): every buffer a
//! node is handed is planar `f32`. `f64` is expected in three places, all
//! outside the graph's buffers: **inside nodes** (filter state and
//! coefficients, oscillator phase — a node converts at its edge), **time**
//! ([`Env::frame`] is a `u64` [`Frame`](tutti_types::Frame) and the transport
//! position a [`Beat`](tutti_types::Beat), which is `f64`; only the
//! block-relative [`Offset`] is `u32`), and **the plugin boundary** (a plugin that processes in
//! double converts inside its node). If `f64` buffers are ever wanted between
//! nodes, they come in as a port *format* on [`PortKind::Audio`], with the
//! compiler inserting a conversion op at an edge whose ends disagree — which
//! is why [`Io`] exposes buffers through methods rather than as
//! `&[&[f32]]` fields.
//!
//! # Decisions taken by the owner, recorded here
//!
//! 1. **A new crate**, `tutti-graph`, below `tutti-core`, depending only on
//!    `tutti-types` and `tutti-node` (the latter for the [`Legacy`] adapter
//!    alone).
//! 2. **`f32` only inside the graph.** See "Precision" above.
//! 3. **No typed static-combinator layer.** A fixed sub-graph that wants one
//!    compiles into a single [`Node`], never into the graph.
//! 4. **Events are graph ports** ([`Event`], [`EventIn`], [`EventOut`]), so the
//!    one PDC pass aligns MIDI and automation with audio.
//! 5. **Fan-in is allowed on event ports only**, merged deterministically by
//!    `(offset, source order)`. Audio keeps one source per port — summing is a
//!    node's job, and `Topology` still makes audio fan-in unrepresentable.
//! 6. **Automation is linear ramp events first** ([`ParamRamp`]); curve
//!    segments can be added as another [`EventKind`] when a non-linear shape
//!    needs sample accuracy without sub-chunking.
//!
//! # Where to read next
//!
//! - [`Node`] and [`Io`] — the contract, and what it drops from `AudioUnit`.
//! - [`compile`] — the pass pipeline, including the buffer colouring that is
//!   correct under *any* schedule the op DAG allows, not only the serial one.
//! - [`Editor`] and [`Executor`] — the runtime pair, the queues between them
//!   (commits, and timestamped commands), and their back-pressure.
//! - [`Reference`] — the oracle, and the recompile semantics it pins.
//!
//! # Import paths
//!
//! Every module is private; every public item is re-exported here, so
//! `tutti_graph::Plan` is the only spelling (`scripts/check-canonical-paths.sh`
//! is run with this crate's module names in CI to keep prose honest too).

#![deny(missing_docs)]
// No `unsafe` anywhere in the crate: the arena's disjoint borrows are proven
// by `split_at_mut`, not by a comment. See `arena.rs`.
#![forbid(unsafe_code)]

mod arena;
mod command;
mod compile;
mod editor;
mod event;
mod exec;
mod io;
mod kernels;
mod legacy;
mod node;
mod plan;
mod reference;
mod spec;
mod time;

pub use command::{CommandId, ScheduleError, CANCEL_CAPACITY, COMMAND_CAPACITY};
pub use compile::{compile, CompileError, CycleEdge, Shapes, VerifyError};
pub use editor::{CommitError, Editor};
pub use event::{
    Event, EventKind, EventOrderError, EventRejected, EventWriter, ParamRamp, SortedEvents,
    SubBlocks, Ump,
};
pub use exec::{Executor, DEFAULT_EVENT_CAPACITY, QUEUE_CAPACITY};
pub use io::{Channel, Inputs, Io, Outputs, PortKind};
pub use legacy::Legacy;
pub use node::{
    ConstantMask, Cx, Env, InPlaceMask, IntoNode, LoopRange, MaxBlock, Node, Prepare, Resolution,
    Scratch, Shape, SilenceMask, Status, Transport, TransportChange, TransportChangeRejected,
    TransportChanges, MAX_PORTS, MAX_TRANSPORT_CHANGES,
};
pub use plan::{
    Csr, DelayKey, DelaySpec, Delta, FeedbackKey, FeedbackSpec, Op, Placement, Plan, PlanUnit,
    Span, UnitIdx, Value, EMPTY_SLOT, ZERO_SLOT,
};
pub use reference::Reference;
pub use spec::{EventEdge, EventIn, EventOut, GraphInvalid, GraphSpec, ValidGraph};
pub use time::{Due, Offset, Playhead};

/// Check `plan` against its op DAG: no slot shared by ops that may run
/// concurrently, every read of the value it was meant to read, feedback read
/// before it is captured. `compile` runs this in every debug build.
pub fn verify(plan: &Plan) -> Result<(), VerifyError> {
    compile::verify::verify(plan)
}
