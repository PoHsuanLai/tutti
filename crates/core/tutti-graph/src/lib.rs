//! The native audio graph: a pure compiler from a graph **value** to an
//! immutable [`Plan`], a serial executor for plans, and the naive
//! [`Reference`] interpreter the executor is proven against.
//!
//! This is Phase 1 of `docs/design/013-native-graph.md` (PR #2): the road off
//! fundsp's `Net` runtime. `tutti_core::Engine::new` renders an
//! [`Executor`] behind the engine (Phase 2), and since Phase 3 the Bevy
//! adapter and export render nothing else. Every node is a [`Node`]
//! (Phase 4). Of Phase 2 it has the
//! sample-accuracy contract's type-level half (doc 013 §6: [`Offset`] vs
//! `Frame`, timestamped commands, [`Io::sub_blocks`], [`Resolution`]),
//! transport changes inside a block ([`TransportChanges`], carried by
//! [`Env`]) and live re-preparation ([`Editor::reprepare`]). Of Phase 3 it
//! has replace-with-fade ([`Editor::replace`], a [`Fade`] along a
//! [`CrossfadeCurve`]).
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
//! - **[`Shape::event_capacity`]** — each event output port's declared
//!   events per block, from which every buffer downstream is sized at
//!   compile and prepare time; a writer past it drops the newest and counts
//!   it, and nothing downstream of a writer can refuse an event.
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
//! beat in [`Env`], from which a node computes where the wrap falls. A
//! transport command landing inside a block (a start, a stop, a seek) is not
//! a split either: [`Env::changes`] carries it, and [`Env::transport_at`]
//! answers "what was the transport at this frame" for both.
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
//! 1. **A new crate**, `tutti-graph`, below `tutti-core`, depending on no
//!    tutti crate but `tutti-types`. (It depended on `tutti-node` too, for
//!    the adapter that ran an `AudioUnit` as a node until every node was
//!    ported; the adapter and the edge went in Phase 4.)
//! 2. **`f32` only inside the graph.** See "Precision" above.
//! 3. **No typed static-combinator layer.** A fixed sub-graph that wants one
//!    compiles into a single [`Node`], never into the graph.
//! 4. **Events are graph ports** ([`Event`], [`EventIn`], [`EventOut`]), so the
//!    one PDC pass aligns MIDI and automation with audio.
//! 5. **Fan-in is allowed on event ports only**, merged deterministically by
//!    `(offset, source order)`, source order being the source port's
//!    `(NodeKey, port)` — a property of the wiring, not of the order a spec
//!    lists its edges in. Audio keeps one source per port — summing is a
//!    node's job, and `Topology` still makes audio fan-in unrepresentable;
//!    events merge losslessly, so they need no `Sum`.
//! 6. **Automation is linear ramp events first** ([`ParamRamp`]); curve
//!    segments can be added as another [`EventKind`] when a non-linear shape
//!    needs sample accuracy without sub-chunking.
//!
//! # Where to read next
//!
//! - [`Node`] and [`Io`] — the contract, and what it drops from fundsp's
//!   `AudioUnit`.
//! - [`compile`] — the pass pipeline, including the buffer colouring that is
//!   correct under *any* schedule the op DAG allows, not only the serial one.
//! - [`Editor`] and [`Executor`] — the runtime pair, the queues between them
//!   (commits, and timestamped commands), and their back-pressure.
//! - [`Editor::replace`] and [`Fade`] — swapping a running unit with a
//!   crossfade: both units run for the fade, the old one retires on the
//!   control thread, and a replace during a fade waits for it.
//! - [`GraphSpec::connect_param`] and [`Io::param`] — compiler-owned param
//!   modulation: a node's declared params ([`Shape::params`]) summed with
//!   their sources onto the node's own control, clamped, per frame; an
//!   unconnected param reads its base, never 0 (design doc 013 item 6).
//! - [`Reference`] — the oracle, and the recompile semantics it pins.
//! - [`ParamNode`] and [`param_parts`] — a node whose controls are its
//!   `Param<U>` cells, addressed by [`ParamSet`], forked from the values last
//!   set. A node's latency can change at runtime with
//!   [`Editor::set_latency`].
//! - [`Editor::fork`] — a copy of the graph, or of the sub-graph feeding
//!   one node, that shares no state with the live one: the offline export
//!   (and live duplicate) that replaces `Net::clone_isolated` +
//!   `isolate_for_offline` + `reset`. A node is forkable only if it handed
//!   the editor a [`ForkSource`] at insert ([`IntoNode::into_parts`]);
//!   [`ForkByClone`] and [`param_parts`] do, [`Unforkable`] does not (a mic
//!   monitor).
//!
//! # Building a graph in a test
//!
//! Test authors, examples and simple hosts: start from [`GraphBuilder`]. It
//! speaks `Net`'s calls (`add` for `push`, `connect`, `pipe_input`,
//! `pipe_output`, `chain`, …, with `Net`'s fan-out rules), builds the
//! [`Editor`]/[`Executor`] pair through the public editor API, and its
//! [`Renderer`] drives the executor block by block and hands back planar or
//! interleaved output. It is not a second graph model: all it produces is a
//! [`GraphSpec`] and the units it names, and what it builds is exactly what
//! a host writing that spec by hand would get.
//!
//! ```
//! use tutti_graph::{ForkByClone, GraphBuilder, Prepare};
//! use tutti_types::{ChannelLayout, SampleRate, Samples};
//! # use tutti_graph::{Cx, Io, Node, Shape, Status};
//! # /// A one-pole lowpass.
//! # #[derive(Clone)]
//! # struct Lowpass { a: f32, y: f32 }
//! # impl Node for Lowpass {
//! #     fn shape(&self) -> Shape { Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO) }
//! #     fn prepare(&mut self, _: &Prepare) {}
//! #     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//! #         let (ins, mut outs) = io.split();
//! #         for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
//! #             self.y += self.a * (i - self.y);
//! #             *o = self.y;
//! #         }
//! #         Status::Modified
//! #     }
//! #     fn reset(&mut self) { self.y = 0.0; }
//! # }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let lp = g.add(ForkByClone(Lowpass { a: 0.1, y: 0.0 }));
//! g.pipe_input(lp).pipe_output(lp);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate(48_000.0), Samples(256)))
//!     .expect("builds");
//! let out = r.render_input(&[&[1.0; 1_000]]);
//! assert!(out[0][999] > 0.9); // a lowpass passes DC
//! ```
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
mod builder;
mod command;
mod compile;
#[cfg(feature = "contract")]
pub mod contract;
mod controls;
mod editor;
mod event;
mod exec;
mod fade;
mod fork;
mod io;
mod kernels;
mod node;
mod param;
mod plan;
mod reference;
mod spec;
mod time;

pub use builder::{GraphBuilder, Renderer, Solo};
pub use command::{CommandId, ScheduleError, CANCEL_CAPACITY, COMMAND_CAPACITY};
pub use compile::{compile, CompileError, CycleEdge, Shapes, VerifyError};
pub use controls::{param_parts, ParamFork, ParamNode, ParamSet, ParamSetBuilder};
pub use editor::{CommitError, Editor, Limits};
pub use event::{
    Event, EventKind, EventOrderError, EventRejected, EventWriter, Harmony, HarmonyKind, ParamRamp,
    SortedEvents, SubBlocks, Ump,
};
pub use exec::{Executor, DEFAULT_EVENT_CAPACITY, FADE_CAPACITY, QUEUE_CAPACITY};
pub use fade::{CrossfadeCurve, Fade};
pub use fork::{
    ForkByClone, ForkCause, ForkError, ForkFault, ForkFaultKind, ForkHealth, ForkMode, ForkSource,
    ForkTarget, Forked, Unforkable,
};
pub use io::{Channel, Inputs, Io, Outputs, PortKind};
pub use node::{
    ConstantMask, Cx, Env, InPlaceMask, IntoNode, LoopRange, MaxBlock, Node, NodeParts, Prepare,
    Resolution, Scratch, Shape, SilenceMask, Status, Transport, TransportChange,
    TransportChangeRejected, TransportChanges, MAX_PORTS, MAX_TRANSPORT_CHANGES,
};
pub use param::{
    ParamFrom, ParamIn, ParamInput, ParamMod, ParamPorts, ParamPortsError, ParamRange,
    ParamShaping, ParamSource, ShapeLut, MAX_PARAM_PORTS, MAX_PARAM_SOURCES, PARAM_DECLICK,
    SHAPE_LUT_LEN,
};
pub use plan::{
    Csr, DelayKey, DelaySpec, Delta, EventSlotCapacity, FeedbackKey, FeedbackSpec, Op, ParamPortOp,
    ParamSlot, ParamSourceOp, Placement, Plan, PlanUnit, Span, UnitIdx, Value, EMPTY_SLOT,
    ZERO_SLOT,
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

/// Check the crossfades `delta` carries into `plan`, from `prev` (the plan
/// running before it): each names a key the delta replaces, once, and the
/// unit it fades from has the shape of the one it fades to in everything but
/// its tail — ports, latency, in-place acceptance, event resolution, event
/// capacity, declared params.
/// [`Editor::package`] runs this on every delta it is handed.
pub fn verify_fades(prev: Option<&Plan>, plan: &Plan, delta: &Delta) -> Result<(), VerifyError> {
    compile::verify::verify_fades(prev, plan, delta)
}
