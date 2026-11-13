#![doc = include_str!("../README.md")]
//! The audio graph of the Tutti engine: a graph described as a value, a pure
//! compiler from that value to an immutable [`Plan`], and a real-time
//! [`Executor`] that runs the plan block by block, serially or spread over
//! a [`Pool`]'s threads.
//!
//! Use it directly to host audio processors ([`Node`]s) in a graph you build
//! and edit at run time, or to write a node that runs inside the engine.
//! `tutti-engine`'s engine renders an [`Executor`] behind its device callback,
//! and the `tutti` facade re-exports this crate as `tutti::graph`.
//!
//! # Quick start
//!
//! Implement [`Node`] for a processor, add it to a [`GraphBuilder`], wire it
//! and render:
//!
//! ```
//! use tutti_types::MaxBlock;
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
//!         Shape::through_unassigned(ChannelLayout::MONO)
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
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(256))))
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
//!  GraphSpec          Topology + event edges + param sources        plain value, Eq + Hash
//!      │ validate()
//!  ValidGraph         checked without the nodes' shapes
//!      │ compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)
//!  Plan               ops, buffer slots, delay and feedback tables   immutable, Send + Sync
//!      │ Editor::commit ──queue──▶ Executor, which sends the replaced state back
//!  Executor           owns the units; runs the plan's ops serially, one block per call
//! ```
//!
//! - [`GraphSpec`] is the graph as data: a
//!   [`Topology`] (nodes, audio edges, global inputs
//!   and outputs) plus event edges and each param port's event source.
//! - [`Editor`] (control thread) holds the spec and the units not yet
//!   shipped. [`Editor::insert`] adds a node and returns its typed controls;
//!   [`Editor::commit`] validates, [`compile()`]s against the plan sent last
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
//! controls are `Param<U>` cells addressed by [`ParamSet`]. Every example
//! below is a doctest: it compiles and runs.
//!
//! ## The shortest node
//!
//! A gain: one mono channel in and out, `Io::channel(c).map(..)` applying a
//! function to each sample whether the executor gave the node separate
//! buffers or one it works in place.
//!
//! ```
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};
//!
//! #[derive(Clone)]
//! struct Gain(f32);
//!
//! impl Node for Gain {
//!     fn shape(&self) -> Shape {
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let g = self.0;
//!         io.channel(0).map(|x| x * g);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let gain = g.add(ForkByClone(Gain(0.5)));
//! g.pipe_input(gain).pipe_output(gain);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(&[&[1.0; 8]])[0], [0.5; 8]);
//! ```
//!
//! `ForkByClone` says the clone shares nothing with the original, so an
//! export can fork the node; a node that cannot be copied is `Unforkable`.
//!
//! ## Shape
//!
//! [`Shape`] is everything the compiler needs to know about a node before it
//! runs: its audio ports and buses ([`Shape::through_unassigned`],
//! [`Shape::source_unassigned`], [`Shape::buses`] for a node with a
//! sidechain or several buses), its event ports ([`with_events`](Shape::with_events)),
//! its [`Cost`] ([`with_cost`](Shape::with_cost): a cheap node says so, one that
//! says nothing is `Heavy`, worth a thread), its **processing** latency
//! ([`with_latency`](Shape::with_latency), which feeds delay compensation: a
//! musical delay is not latency) and its **tail**, the ceiling on how long it
//! keeps sounding after its input stops ([`with_tail`](Shape::with_tail)).
//! A node that changes width declares its buses; the graph never guesses a
//! dry path.
//!
//! A ducker has a main input and a mono sidechain, so its shape is two
//! buses in and one out. The sidechain is read through the handle the shape
//! issued, not by index:
//!
//! ```
//! use tutti_graph::{Bus, BusRole, Buses, Cost, Cx, InBus, Io, Node, Prepare, Shape, Status};
//! use tutti_types::{BusTopology, ChannelLayout, Latency, Samples, Tail};
//!
//! struct Ducker {
//!     key: InBus,
//! }
//!
//! impl Ducker {
//!     fn ports() -> Shape {
//!         let mono = BusTopology::unassigned(ChannelLayout::MONO).unwrap();
//!         let ins = Buses::main(mono)
//!             .with(Bus::new(mono, BusRole::Sidechain))
//!             .unwrap();
//!         Shape::buses(ins, Buses::unassigned(ChannelLayout::MONO))
//!             .with_cost(Cost::Light)
//!             .with_latency(Latency::new(Samples(0)))
//!             .with_tail(Tail::None)
//!     }
//!     fn new() -> Self {
//!         Self { key: Self::ports().input_bus(1).expect("the sidechain is the second bus") }
//!     }
//! }
//!
//! impl Node for Ducker {
//!     fn shape(&self) -> Shape {
//!         Self::ports()
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         // The main input in place or not, the sidechain by its handle.
//!         let (inputs, mut outputs) = io.split();
//!         let key = inputs.bus(self.key);
//!         for (o, k) in outputs.get(0).iter_mut().zip(key.get(0)) {
//!             *o = (1.0 - k.abs().min(1.0)) * *o;
//!         }
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let shape = Ducker::new().shape();
//! assert_eq!(shape.audio_in().count(), 2);
//! assert_eq!(shape.audio_out().count(), 1);
//! assert_eq!(shape.cost, Cost::Light);
//! ```
//!
//! ## A node with params
//!
//! A param is a `Param<U>` cell in the node, in a unit (never a bare float).
//! [`declare_params!`] states the node's params once, next to the cells: what a
//! host reaches by address, what the graph may modulate (the shape's ports)
//! and the unit each is in. `process` reads a driven param **through the
//! declaration** (`declared_param`), never by a port index, so reordering the
//! rows cannot swap which param a node reads. A lane on the port overrides the
//! cell for the block (`last_frame` for a block-rate control, `frames` for
//! per-sample); undriven, the node reads its own cell.
//!
//! The node is inserted as [`Params`]`(node)`, never `ForkByClone`: its fork
//! must start from the values set through its [`ParamSet`], not share the
//! live cell. (`ForkByClone(param_node)` still compiles, Rust having no way
//! to refuse the type; see [`Params`].) In a session it is
//! `graph.add(Params(node))`, which returns the key and the [`ParamSet`] and
//! gives the key its typed param methods from the declaration.
//!
//! ```
//! use tutti_graph::{
//!     Cx, GraphBuilder, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status,
//!     declare_params,
//! };
//! use tutti_types::{Amplitude, ChannelLayout, MaxBlock, Param, SampleRate, Samples, UnitParam};
//!
//! #[derive(Clone)]
//! struct Volume {
//!     level: Param<Amplitude>,
//! }
//!
//! declare_params!([] Volume, n {
//!     Volume => Amplitude = &n.level,
//! });
//!
//! impl Node for Volume {
//!     fn shape(&self) -> Shape {
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!             .with_param_addrs(self.declared_ports().as_slice())
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let level = self
//!             .declared_param::<{ UnitParam::Volume as u16 }>(&io)
//!             .last_frame()
//!             .unwrap_or_else(|| self.level.load().get());
//!         io.channel(0).map(|x| x * level);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//!     fn param_base(&self, port: usize) -> Option<f32> {
//!         self.declared_base(port)
//!     }
//! }
//!
//! impl ParamNode for Volume {
//!     fn param_set(&self) -> ParamSet {
//!         self.declared_set()
//!     }
//!     fn fork_fresh(&self) -> Self {
//!         let mut fork = self.clone();
//!         fork.level.detach();
//!         fork
//!     }
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let (volume, controls) = g.add_with_controls(Params(Volume { level: Param::new(Amplitude(1.0)) }));
//! g.pipe_input(volume).pipe_output(volume);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(&[&[1.0; 4]])[0], [1.0; 4]);
//! // A host sets it by address; the next block reads the cell.
//! assert!(controls.set(UnitParam::Volume, 0.25));
//! assert_eq!(r.render_input(&[&[1.0; 4]])[0], [0.25; 4]);
//! ```
//!
//! ## Events in and out
//!
//! Events travel on ports the shape declares. A node reads an input with
//! [`Io::sub_blocks`], which splits the block at the events' offsets so it is
//! sample-accurate by construction, and writes an output with
//! [`Io::event_out`]. Here a source emits a note-on at frame 3 and a gate
//! opens on it:
//!
//! ```
//! use tutti_graph::{
//!     Cx, Event, EventKind, ForkByClone, GraphBuilder, Io, Node, Offset, Prepare, Shape, Status,
//! };
//! use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};
//! use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Ump, Voice1};
//!
//! #[derive(Clone)]
//! struct Pulse;
//!
//! impl Node for Pulse {
//!     fn shape(&self) -> Shape {
//!         Shape::EMPTY.with_events(0, 1).with_event_capacity(4)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let on = Message::midi1(MidiChannel::of(0), Voice1::note_on(NoteNumber::of(60), U7::of(100)));
//!         if let Some(at) = io.offset(3) {
//!             let _ = io.event_out(0).push(Event::midi(at, Ump::from(on)));
//!         }
//!         Status::Silent
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! #[derive(Clone)]
//! struct Gate {
//!     open: bool,
//! }
//!
//! impl Node for Gate {
//!     fn shape(&self) -> Shape {
//!         Shape::source_unassigned(ChannelLayout::MONO).with_events(1, 0)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         for (range, events) in io.sub_blocks(0) {
//!             for e in events {
//!                 if let EventKind::Midi(_) = e.kind {
//!                     self.open = true;
//!                 }
//!             }
//!             io.output(0)[range].fill(f32::from(u8::from(self.open)));
//!         }
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {
//!         self.open = false;
//!     }
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let pulse = g.add(ForkByClone(Pulse));
//! let gate = g.add(ForkByClone(Gate { open: false }));
//! g.connect_events(pulse, 0, gate, 0).pipe_output(gate);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))))
//!     .expect("builds");
//! assert_eq!(r.render(6)[0], [0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
//! # let _ = Offset::ZERO;
//! ```
//!
//! ## Status and tail
//!
//! `process` returns a [`Status`] that tells the executor what it may skip:
//! `Modified` (outputs written, no claim), `Masked` (some channels silent or
//! constant, a claim the node must back), `Silent` (wrote nothing, output is
//! silence this block), `Idle` (silent with no pending activity: park me until
//! an input arrives), `Constant`, and `Bypass` (the output is the input, the
//! executor copies).
//!
//! [`Shape::tail`] is the node's ceiling. A node whose ring-out follows its
//! params narrows it each block with [`Io::report_tail`]: the report is
//! measured from the last non-quiet input, must cover the ring-out and err
//! long, and only ever moves the silence skip (the plan, delay compensation
//! and export read the ceiling).
//!
//! ```
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples, Tail};
//!
//! /// A feedback echo whose ring-out is `taps` repeats of `period` frames.
//! #[derive(Clone)]
//! struct Echo {
//!     taps: usize,
//!     period: usize,
//!     on: bool,
//! }
//!
//! impl Node for Echo {
//!     fn shape(&self) -> Shape {
//!         // The ceiling: never longer than this, whatever the params.
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!             .with_tail(Tail::Finite(Samples(8 * 4_800)))
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         if !self.on {
//!             // Switched off: the output is the input.
//!             return Status::Bypass;
//!         }
//!         // The ring-out follows the params, so say it; err long.
//!         io.report_tail(Tail::Finite(Samples(self.taps * self.period)));
//!         io.channel(0).map(|x| x);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let echo = g.add(ForkByClone(Echo { taps: 3, period: 4_800, on: false }));
//! g.pipe_input(echo).pipe_output(echo);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(&[&[0.5; 4]])[0], [0.5; 4]);
//! ```
//!
//! ## What not to do
//!
//! - **Share state outside ports.** Nodes communicate only through ports: two
//!   nodes sharing an `Arc<Mutex<_>>` or a cursor are unordered when a block's
//!   tasks run on a worker pool, and the render stops being bit-identical to
//!   the serial one. Share through an edge, or make it one node.
//! - **Allocate, lock or block in `process`.** It runs on the audio thread:
//!   size every buffer in `prepare` (against [`Prepare::max_block`]), and
//!   publish non-scalar state to it through `tutti_rt`'s `ArcLend`, never by
//!   swapping an `Arc` the audio thread then drops.
//! - **Report a musical delay as latency.** Latency is processing delay only.
//! - **Read a driven param by port index.** Use the declaration
//!   (`declared_param`), above.
//!
//! Several mistakes are ruled out by types:
//!
//! - **Latency is declared, never a delay time.** [`Shape::latency`] is a
//!   [`Latency`](tutti_types::Latency), which a musical delay in `Samples` or
//!   `Seconds` cannot be passed as, so the graph's delay compensation (PDC)
//!   never drags parallel paths behind an echo.
//! - **Blocks never exceed the [`MaxBlock`](tutti_types::MaxBlock) a node was
//!   prepared with** ([`Prepare::max_block`]); every [`Io`] is at most that long,
//!   so a node that sizes its scratch from it never clamps.
//! - **Block offsets are not frames.** An event carries an [`Offset`] inside
//!   its block, a different type from an absolute
//!   [`Frame`](tutti_types::Frame); the two convert only through the block's
//!   [`Env`] ([`Env::offset_of`], [`Env::frame_at`]).
//! - **Events arrive sorted and in range** ([`SortedEvents`]), and
//!   [`Io::sub_blocks`] splits the block at their offsets, so a node written
//!   against it is sample-accurate by construction.
//! - **Params are typed.** A [`ParamRamp`] or [`ParamSegment`] is built
//!   from a typed `ParamKey<U>` and read back as a `U`.
//!
//! # Whole blocks
//!
//! The executor hands every node the **whole block**. It never splits one,
//! not at event offsets (sub-chunking at an event is the node's job), not at
//! a transport loop wrap, and not at a transport change inside the block.
//! That keeps an out-of-process plugin's declared pipeline latency constant.
//! A loop wrap is visible through [`TransportPosition::looping`] and the block-start
//! beat in [`Env`]; a start, stop or seek landing inside a block is carried
//! by [`Env::changes`], and [`Env::transport_at`] answers "what was the
//! transport at this frame". A node behind a latent path reads
//! [`Cx::transport_at`] instead: the transport its *input* was rendered at,
//! [`Cx::arrival`] frames earlier, reaching into earlier blocks through the
//! executor's [`TransportHistory`].
//!
//! # Events and timing
//!
//! Events ([`Event`], [`EventKind`]: MIDI 2.0 [`Ump`](tutti_ump::Ump) packets,
//! [`ParamRamp`] and [`ParamSegment`] automation, [`Harmony`]) travel on
//! graph ports ([`EventIn`], [`EventOut`]), so the same delay compensation that aligns audio aligns
//! them. An event input may have several sources, merged by offset and then
//! by the source port's `(NodeKey, port)` — a property of the wiring, not of
//! the order edges were listed in. An audio input port, and a
//! global output channel, holds a set of sources, which the graph sums
//! (`Op::Sum`, doc 013 "Audio fan-in sums").
//!
//! - Each event output port declares how many events it writes per block
//!   ([`Shape::event_capacity`]), and every buffer downstream is sized from
//!   that at compile time. A writer past it is refused the newest event,
//!   except a note-off, which has room past the capacity
//!   ([`note_off_room`]); the executor counts each refusal
//!   ([`Executor::dropped_event_count`]), and per port
//!   ([`Editor::take_event_losses`]). Nothing downstream of a writer ever
//!   drops one. A source whose rate is per second sizes its declaration from
//!   the block it is prepared for ([`EventDensity`]).
//! - [`Editor::schedule`] delivers an event on an exact frame or beat
//!   ([`At`](tutti_types::At)). A late command lands at the start of the next
//!   block and is counted ([`Executor::late_command_count`]), never dropped.
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
//! - [`GraphSpec::connect_param`] drives one of a node's declared params
//!   ([`Shape::params`]) from another node's event output (a control
//!   lane's value and ramp segments); the node reads the result through
//!   [`Io::param`], and an unconnected param reads its own control, never 0.
//! - [`Editor::fork`] builds an independent copy of the graph, or of the part
//!   feeding one node, for an offline export or a live duplicate.
//! - A feedback loop is an explicit edge with its own delay
//!   (`crate::FeedbackFrom`), so an offline bounce at a larger
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
//! - `Reference` (feature `testing`) is a naive interpreter the executor is
//!   checked against, bit for bit.
//! - [`Plan::verify`] and [`Plan::verify_fades`] check a [`Plan`] against its
//!   op DAG; [`compile()`] runs `verify` in every debug build.
//! - [`GraphBuilder::renderer`] returns a [`Renderer`] that drives an
//!   executor and hands back planar or interleaved output.
//!
//! # Feature flags
//!
//! - `contract` (off by default): the `contract` module, a
//!   harness that checks a node responds on exactly the frame its declared
//!   latency and resolution promise, on every path the graph can put it on.
//!   Test support: enable it from a dev-dependency.
//! - `testing` (off by default): `Reference`, the naive interpreter this
//!   crate's differential suites check the executor against, and the `testing`
//!   module: the small test nodes (`TestNode`, `Kind`), the executor-versus-
//!   reference harness (`Pair`) and a worker pool for tests (`TestPool`).
//!   Test support: this crate's own tests turn it on through a self
//!   dev-dependency, and a node crate's tests may do the same.
//!
//! # Import paths
//!
//! Every public item is re-exported at the crate root; import from there
//! (`tutti_graph::Plan`). The only public modules are `contract` and `testing`, both test support.

#![deny(missing_docs)]
// No `unsafe` anywhere in the crate: the arena's disjoint borrows are proven
// by `split_at_mut`, not by a comment. See `arena.rs`.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod arena;
mod builder;
mod bus;
mod bus_edge;
mod command;
mod compile;
#[cfg(feature = "contract")]
pub mod contract;
mod controls;
mod cost;
mod declare;
mod editor;
mod event;
mod event_inlet;
mod event_losses;
mod event_tap;
mod exec;
mod fade;
mod fork;
mod gain;
mod graph;
mod held;
mod history;
mod inlet;
mod instrument;
mod io;
mod kernels;
mod latency;
mod loss;
mod meter;
mod mode;
mod node;
mod par;
mod param;
mod pass;
mod plan;
mod play;
mod pool;
mod preroll;
#[cfg(feature = "testing")]
mod reference;
mod slots;
mod spec;
mod tail;
mod tap;
#[cfg(feature = "testing")]
pub mod testing;
mod time;
mod wake;

pub use builder::{GraphBuilder, Renderer, Solo};
pub use command::{CANCEL_CAPACITY, COMMAND_CAPACITY, CommandId, ScheduleError};
pub use compile::{CompileError, CycleEdge, Shapes, VerifyError, compile};
pub use controls::{ParamFork, ParamNode, ParamSet, ParamSetBuilder, Params, param_parts};
pub use cost::{Cost, Schedule, Work};
pub use declare::{HasParam, HasParams, ParamUnit, Scalar};
pub use editor::{CloseError, CommitError, Editor, Limits, MAX_ROOT_CHANNELS, MissingSource};
pub use event::{
    Event, EventDensity, EventKind, EventOrderError, EventRejected, EventWriter, Harmony,
    HarmonyKind, MAX_EVENT_PORT_CAPACITY, MIN_EVENT_PORT_CAPACITY, ParamRamp, ParamSegment,
    SortedEvents, SubBlocks, UmpAt, note_off_room,
};
pub use event_inlet::{
    EventCapacity, EventCapacityError, EventInletNode, EventInletWriter, InletCounts, Refused,
    Stamp,
};
pub use event_losses::Losses;
pub use event_tap::{EventTapReader, EventTapWriter, TapEvent};
pub use exec::{DEFAULT_EVENT_CAPACITY, Executor, FADE_CAPACITY, QUEUE_CAPACITY};
pub use fade::{CrossfadeCurve, Fade};
pub use fork::{
    ForkBuild, ForkByClone, ForkCause, ForkError, ForkFault, ForkFaultKind, ForkHealth, ForkMode,
    ForkRemains, ForkSource, ForkTarget, Forked, PendingFork, Unforkable,
};
pub use tutti_curve::{Curvature, Exponent, Law};
// The audio graph as a value, and the two passes over it. `plan` is the
// latency pass and is exported as `latency_plan`.
pub use bus::{Bus, BusRole, Buses, InBus, MAX_BUSES, OutBus};
pub use bus_edge::{BusError, BusFeed, BusFold, BusRef, BusSide, BusSource, Lowering};
pub use gain::{EDGE_DECLICK, EdgeFade, EdgeGain};
pub use graph::{
    Edge, EdgeId, EventTapId, EventTapSource, EventTapSpec, FeedbackFrom, InPort, Invalid, MeterId,
    MeterSpec, NodeKey, NodeSpec, OutPort, ParamValue, Sink, Source, TapAlign, TapId, TapSpec,
    Topology, Valid,
};
pub use history::TransportHistory;
pub use inlet::{InletNode, InletRing};
pub use instrument::Instrument;
pub use io::{BusIn, BusOut, Channel, Inputs, Io, Outputs, PortKind};
pub use latency::{
    ChannelFeed, Compensation, Delays, Feed, InputDelay, LatencyGraph, MAX_NODE_LATENCY,
    OutputDelay, PortFeed, delays, plan as latency_plan,
};
pub use loss::EventLoss;
pub use mode::{MODE_DECLICK, MixError, ModeError, NodeMix, NodeMode};
pub use node::{
    Chase, ConstantMask, Cx, Env, InPlaceMask, IntoNode, LoopRange, MAX_PORTS,
    MAX_TRANSPORT_CHANGES, Node, NodeParts, Prepare, Resolution, Scratch, Shape, SilenceMask,
    Status, TransportChange, TransportChangeRejected, TransportChanges, TransportPosition,
};
pub use par::Pool;
pub use param::{
    MAX_PARAM_PORTS, PARAM_DECLICK, ParamIn, ParamInput, ParamMod, ParamPorts, ParamPortsError,
    ParamRange, ParamRangeReader,
};
pub use pass::{Pass, PassError};
pub use plan::{
    Csr, DelayKey, DelaySpec, Delta, EMPTY_SLOT, EventSlotCapacity, FeedbackKey, FeedbackSpec,
    GainPortOp, Op, ParamPortOp, Placement, Plan, PlanUnit, Span, UnitIdx, Value, ZERO_SLOT,
};
pub use play::PlayClock;
pub use pool::{WorkerPool, WorkerPoolBuilder};
pub use preroll::PrerollTable;
pub use tail::{GraphTail, MAX_NODE_TAIL, TailGraph, graph_tail};
pub use wake::Wake;
// The reading is a value type, `tutti-types`' (the modulation compiler
// carries one without naming the graph); the clock that keeps it is here.
pub use meter::{Level, MeterReader, MeterWriter};
#[cfg(feature = "testing")]
pub use reference::Reference;
pub use spec::{EventEdge, EventIn, EventOut, GraphInvalid, GraphSpec, ValidGraph};
pub use tap::{TapReader, TapWriter};
pub use time::{Continuity, Due, Offset, Playhead};
pub use tutti_rt::BlockWaits;
pub use tutti_types::PlayReading;

/// What [`declare_params!`] expands to, and nothing a caller names.
#[doc(hidden)]
pub mod __private {
    pub use crate::declare::{Cells, Row, Visit, base, input, ports, row, set};
    pub use tutti_types::{UnitAddr, UnitParam};
}
