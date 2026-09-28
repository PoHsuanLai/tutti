//! The audio graph of the Tutti engine: a graph described as a value, a pure
//! compiler from that value to an immutable [`Plan`], and a real-time
//! [`Executor`] that runs the plan block by block.
//!
//! Use it directly to host audio processors ([`Node`]s) in a graph you build
//! and edit at run time, or to write a node that runs inside the engine.
//! `tutti-core`'s engine renders an [`Executor`] behind its device callback,
//! and the `tutti` facade re-exports this crate as `tutti::graph`.
//!
//! # Quick start
//!
//! Implement [`Node`] for a processor, add it to a [`GraphBuilder`], wire it
//! and render:
//!
//! ```
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_types::{ChannelLayout, SampleRate, Samples};
//!
//! /// A one-pole lowpass.
//! #[derive(Clone)]
//! struct Lowpass {
//!     a: f32,
//!     y: f32,
//! }
//!
//! impl Node for Lowpass {
//!     fn shape(&self) -> Shape {
//!         Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let (ins, mut outs) = io.split();
//!         for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
//!             self.y += self.a * (i - self.y);
//!             *o = self.y;
//!         }
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {
//!         self.y = 0.0;
//!     }
//! }
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
//! [`GraphBuilder`] is a convenience for fixed graphs (tests, examples,
//! simple hosts). A host that edits its graph while it plays uses the
//! [`Editor`]/[`Executor`] pair directly; see [`Editor`] for an example.
//!
//! # How it fits together
//!
//! ```text
//!  GraphSpec          Topology + event edges + param modulation    plain value, Eq + Hash
//!      │ validate()
//!  ValidGraph         checked without the nodes' shapes
//!      │ compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)
//!  Plan               ops, buffer slots, delay and feedback tables   immutable, Send + Sync
//!      │ Editor::commit ──queue──▶ Executor, which sends the replaced state back
//!  Executor           owns the units; runs the plan's ops serially, one block per call
//! ```
//!
//! - [`GraphSpec`] is the graph as data: a
//!   [`Topology`](tutti_types::Topology) (nodes, audio edges, global inputs
//!   and outputs) plus event edges and param modulation.
//! - [`Editor`] (control thread) holds the spec and the units not yet
//!   shipped. [`Editor::insert`] adds a node and returns its typed controls;
//!   [`Editor::commit`] validates, [`compile`]s against the plan sent last
//!   and sends the result — the new [`Plan`] and only the units that changed
//!   — over a preallocated queue.
//! - [`Executor`] (audio thread) installs each commit at the start of a
//!   block and sends everything it replaced back, so nothing is freed on the
//!   audio thread. With no commit pending, [`Executor::process`] does not
//!   allocate, lock or block.
//!
//! [`Editor::new`] is the only way to build an executor, and builds the two
//! as a pair from one [`Prepare`] (sample rate and maximum block), so the
//! block size every node sized its buffers for is the one it is handed.
//!
//! # Writing a node
//!
//! A [`Node`] has four methods: [`shape`](Node::shape) declares its ports,
//! latency and tail as a [`Shape`]; [`prepare`](Node::prepare) sizes its
//! buffers on the control thread; [`process`](Node::process) renders one
//! block from an [`Io`] on the audio thread and returns a [`Status`];
//! [`reset`](Node::reset) clears its state. A node is inserted through
//! [`IntoNode`], which also says whether it can be forked: wrap a plain node
//! in [`ForkByClone`] or [`Unforkable`], or implement `IntoNode` to hand out
//! typed controls. [`ParamNode`] and [`param_parts`] cover a node whose
//! controls are `Param<U>` cells addressed by [`ParamSet`].
//!
//! Several mistakes are ruled out by types:
//!
//! - **Latency is declared, never a delay time.** [`Shape::latency`] is a
//!   [`Latency`](tutti_types::Latency), which a musical delay in `Samples` or
//!   `Seconds` cannot be passed as, so the graph's delay compensation (PDC)
//!   never drags parallel paths behind an echo.
//! - **Blocks never exceed [`MaxBlock`]**, which is obtainable only from
//!   [`Prepare`]; every [`Io`] is at most that long, so a node never clamps.
//! - **Block offsets are not frames.** An event carries an [`Offset`] inside
//!   its block, a different type from an absolute
//!   [`Frame`](tutti_types::Frame); the two convert only through the block's
//!   [`Env`] ([`Env::offset_of`], [`Env::frame_at`]).
//! - **Events arrive sorted and in range** ([`SortedEvents`]), and
//!   [`Io::sub_blocks`] splits the block at their offsets, so a node written
//!   against it is sample-accurate by construction.
//! - **Params are typed.** A [`ParamRamp`] is built from a typed `ParamKey<U>`
//!   and read back as a `U`.
//!
//! # Whole blocks
//!
//! The executor hands every node the **whole block**. It never splits one,
//! not at event offsets (sub-chunking at an event is the node's job), not at
//! a transport loop wrap, and not at a transport change inside the block.
//! That keeps an out-of-process plugin's declared pipeline latency constant.
//! A loop wrap is visible through [`Transport::looping`] and the block-start
//! beat in [`Env`]; a start, stop or seek landing inside a block is carried
//! by [`Env::changes`], and [`Env::transport_at`] answers "what was the
//! transport at this frame".
//!
//! # Events and timing
//!
//! Events ([`Event`], [`EventKind`]: MIDI 2.0 [`Ump`] packets, [`ParamRamp`]
//! automation, [`Harmony`]) travel on graph ports ([`EventIn`],
//! [`EventOut`]), so the same delay compensation that aligns audio aligns
//! them. An event input may have several sources, merged by offset and then
//! by the source port's `(NodeKey, port)` — a property of the wiring, not of
//! the order edges were listed in. Audio keeps one source per input port;
//! summing is a node's job.
//!
//! - Each event output port declares how many events it writes per block
//!   ([`Shape::event_capacity`]), and every buffer downstream is sized from
//!   that at compile time. A writer past it is refused the newest event and
//!   the executor counts it ([`Executor::dropped_events`]); nothing
//!   downstream of a writer ever drops one.
//! - [`Editor::schedule`] delivers an event on an exact frame or beat
//!   ([`At`](tutti_types::At)). A late command lands at the start of the next
//!   block and is counted ([`Executor::late_commands`]), never dropped.
//! - Each node declares how finely it honours event offsets
//!   ([`Shape::event_resolution`], a [`Resolution`]); an event edge marked
//!   with [`GraphSpec::require_resolution`] into a node that cannot honour it
//!   is [`CompileError::ResolutionTooCoarse`].
//!
//! # Editing a running graph
//!
//! - [`Editor::replace`] swaps a node's unit with a crossfade ([`Fade`],
//!   along a [`CrossfadeCurve`]); the outgoing unit retires on the control
//!   thread.
//! - [`Editor::set_latency`] changes a node's declared latency (a hosted
//!   plugin's pipeline changing).
//! - [`Editor::reprepare`] changes the sample rate or maximum block,
//!   re-preparing every unit on the control thread.
//! - [`GraphSpec::connect_param`] modulates a node's declared params
//!   ([`Shape::params`]) from other nodes' outputs or from ramp events; the
//!   node reads the result through [`Io::param`], and an unconnected param
//!   reads its own control, never 0.
//! - [`Editor::fork`] builds an independent copy of the graph, or of the part
//!   feeding one node, for an offline export or a live duplicate.
//! - A feedback loop is an explicit edge with its own delay
//!   (`tutti_types::graph::FeedbackFrom`), so an offline bounce at a larger
//!   block loops like playback; a delay shorter than the maximum block is
//!   [`CompileError::FeedbackTooShort`].
//!
//! # Precision
//!
//! Every buffer between nodes is planar `f32`. `f64` belongs inside nodes
//! (filter state, oscillator phase — a node converts at its edge), in time
//! ([`Env::frame`] is a `u64` [`Frame`](tutti_types::Frame) and the
//! transport position a `f64` [`Beat`](tutti_types::Beat)), and at the
//! plugin boundary (a plugin that processes in double converts inside its
//! node). [`Io`] exposes buffers through methods rather than fields so a
//! port format can be added later without breaking nodes.
//!
//! # Testing
//!
//! - [`Reference`] is a naive interpreter the executor is checked against,
//!   bit for bit.
//! - [`verify`] and [`verify_fades`] check a [`Plan`] against its op DAG;
//!   [`compile`] runs `verify` in every debug build.
//! - [`GraphBuilder::renderer`] returns a [`Renderer`] that drives an
//!   executor and hands back planar or interleaved output.
//!
//! # Feature flags
//!
//! - `contract` (off by default): the `contract` module, a
//!   harness that checks a node responds on exactly the frame its declared
//!   latency and resolution promise, on every path the graph can put it on.
//!   Test support: enable it from a dev-dependency.
//!
//! # Import paths
//!
//! Every public item is re-exported at the crate root; import from there
//! (`tutti_graph::Plan`). The only public module is `contract`.

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

/// Checks `plan` against its op DAG: no slot shared by ops that may run
/// concurrently, every read of the value it was meant to read, feedback read
/// before it is captured. [`compile`] runs this in every debug build.
///
/// # Errors
///
/// Returns the first [`VerifyError`] found.
pub fn verify(plan: &Plan) -> Result<(), VerifyError> {
    compile::verify::verify(plan)
}

/// Checks the crossfades `delta` carries into `plan`, from `prev` (the plan
/// running before it): each names a key the delta replaces, once, and the
/// unit it fades from has the shape of the one it fades to in everything but
/// its tail — ports, latency, in-place acceptance, event resolution, event
/// capacity, declared params. [`Editor::package`] runs this on every delta
/// it is handed.
///
/// # Errors
///
/// Returns the first [`VerifyError`] found.
pub fn verify_fades(prev: Option<&Plan>, plan: &Plan, delta: &Delta) -> Result<(), VerifyError> {
    compile::verify::verify_fades(prev, plan, delta)
}
