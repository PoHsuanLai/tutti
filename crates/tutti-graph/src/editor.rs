//! The runtime's control side: [`Editor`] holds the graph value, prepares
//! units, compiles, and sends commits to its paired [`Executor`] — and drains
//! the boxes it sends back.
//!
//! # Commits are sent, not handed out
//!
//! [`commit`](crate::Editor::commit) compiles and, on success, **sends the box
//! itself** to the paired executor over a preallocated queue. The caller
//! never holds a box, so one cannot be dropped unapplied, applied twice or
//! reordered: every commit is compiled against the plan sent just before it,
//! the executor applies them in that order, and the editor's idea of the
//! running plan can never run ahead of what the executor will install.
//!
//! # Back-pressure
//!
//! The audio thread sends every box back after applying it, and
//! that push must never fail — a failed push on the audio thread means either
//! freeing there or blocking. At most [`QUEUE_CAPACITY`](crate::QUEUE_CAPACITY) commits may be out
//! (sent, and not yet drained back by [`collect`](crate::Editor::collect)); with that
//! many out, `commit` returns [`CommitError::Backpressure`] **before**
//! compiling or advancing its plan. The return ring holds one more than that,
//! so the executor's push always has room. `commit` drains the return ring
//! first, so a caller that only ever commits never has to call `collect`.
//!
//! A crossfade ([`replace`](crate::Editor::replace)) does **not** hold its commit:
//! the box comes back as soon as it is applied. The fade's outgoing unit
//! comes back on its own, on the fade-return ring, when the fade ends (or
//! is cut), and `collect` drains that ring too. Each fade a commit starts
//! takes one of [`FADE_CAPACITY`](crate::FADE_CAPACITY) slots on it, reserved before the commit
//! is sent; a commit that would need more is `Backpressure` too — a cap no
//! session reaches in practice.
//!
//! # One `Prepare`
//!
//! [`Editor::new`] builds the editor *and* its [`Executor`], from one
//! [`Prepare`]. Units are prepared by the editor and run by the executor, so
//! the `MaxBlock` a node sized its scratch from is the one every block it is
//! handed obeys; the plan carries its `Prepare` too, and applying refuses a
//! plan prepared for anything else.
//!
//! # Changing the rate or the maximum block
//!
//! [`reprepare`](crate::Editor::reprepare): a full recompile with every unit
//! re-prepared on the control thread, in two commits — the first checks the
//! units out of the executor (which adopts the new `Prepare`), the second
//! sends them back re-prepared, with a plan compiled against their new
//! shapes. Its doc states what happens to delay and feedback state. An
//! offline render must be prepared with a `MaxBlock` no larger than the
//! graph's shortest feedback delay (see `crate::FeedbackFrom`);
//! `compile` enforces it, and `reprepare` refuses such a change before
//! sending anything.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::EdgeId;
use crate::{EventTapId, MeterId, NodeKey, TapId};

use tutti_types::At;

use crate::bus_edge::{BusError, BusFold, BusRef, BusSource, Lowering};
use crate::command::{CommandId, ScheduleError, command_channel};
use crate::compile::{CompileError, Shapes};
use crate::event::EventKind;
use crate::event_tap::EventTapWriter;
use crate::exec::{DEFAULT_EVENT_CAPACITY, Executor, channels};
use crate::fork::{ForkHealth, ForkSource};
use crate::gain::EdgeFade;
use crate::meter::MeterWriter;
use crate::mode::NodeMix;
use crate::node::{Prepare, Resolution};
use crate::param::{ParamIn, ParamRange};
use crate::plan::{Delta, Plan};
use crate::spec::{EventIn, GraphSpec, ValidGraph};
use crate::tap::TapWriter;

mod cells;
mod commit;
mod controls;
mod errors;
mod lifecycle;
mod link;
mod nodes;
mod outlets;
mod placements;
mod taps;
use cells::LiveCells;
pub use errors::{CloseError, CommitError, MissingSource};
use lifecycle::Lifecycle;
use link::Link;
use outlets::Outlets;
use placements::Placements;
use tutti_types::Amplitude;

/// The widest graph root a host folds to its device: mono through 7.1. The
/// engine's fold scratch holds this many channels, so it bounds its editor's
/// [`Limits::max_global_outputs`] to it, and the sampler's voice width
/// equals it. (A device wider than this is fine: the channels past what the
/// root folds to are silent.) A size of the scratch, not a property of the graph.
pub const MAX_ROOT_CHANNELS: usize = 8;

/// What the host that runs an [`Executor`] can take, enforced by its
/// [`Editor`] on every later commit and re-prepare, so a graph the host
/// cannot run is refused on the control thread and never reaches it.
///
/// The executor itself takes any graph; a host has buffers of its own. The
/// engine (`tutti_engine::Engine::new`) sets these to its fold
/// scratch's width and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most global outputs a committed graph may have.
    pub max_global_outputs: usize,
    /// Largest `MaxBlock` a re-prepare may ask for, in frames.
    pub max_block: usize,
}

impl Limits {
    /// No limit: what an editor starts with.
    pub const NONE: Limits = Limits {
        max_global_outputs: usize::MAX,
        max_block: usize::MAX,
    };

    fn outputs(&self, outputs: usize) -> Result<(), CommitError> {
        if outputs > self.max_global_outputs {
            return Err(CommitError::TooManyOutputs {
                outputs,
                limit: self.max_global_outputs,
            });
        }
        Ok(())
    }

    fn block(&self, max_block: usize) -> Result<(), CommitError> {
        if max_block > self.max_block {
            return Err(CommitError::BlockTooLong {
                max_block,
                limit: self.max_block,
            });
        }
        Ok(())
    }
}

/// The control-thread half of a graph: holds the [`GraphSpec`], prepares
/// units, compiles, and sends each result to its paired [`Executor`].
///
/// [`Editor::new`] builds the editor and its executor together, from one
/// [`Prepare`]. Nodes are added with [`insert`](Self::insert) (which
/// prepares the unit and returns its typed controls), wired through
/// [`spec_mut`](Self::spec_mut), and shipped with [`commit`](Self::commit),
/// which validates and compiles the spec against the plan sent last and
/// sends a commit — the new [`Plan`] and only the units that changed — over a
/// preallocated queue. The executor installs it at the start of its next
/// block and sends back everything it replaced, which the editor frees on
/// the control thread in [`collect`](Self::collect) (`commit` collects
/// first, so a caller that only commits never needs to).
///
/// The caller never holds a commit, so one cannot be dropped unapplied,
/// applied twice or reordered. At most [`QUEUE_CAPACITY`](crate::QUEUE_CAPACITY) commits may be in
/// flight; past that `commit` returns [`CommitError::Backpressure`] before
/// compiling anything.
///
/// Every method runs on the control thread and may allocate. Other edits:
/// [`replace`](Self::replace) swaps a unit with a crossfade,
/// [`set_latency`](Self::set_latency) changes a node's declared latency,
/// [`reprepare`](Self::reprepare) changes the sample rate or maximum block,
/// [`schedule`](Self::schedule) delivers an event on an exact frame or beat,
/// and [`fork`](Self::fork) copies the graph for an offline render.
///
/// # Examples
///
/// ```
/// use tutti_types::MaxBlock;
/// use tutti_graph::{Cx, Editor, ForkByClone, Io, Node, Prepare, Shape, Status, TransportPosition};
/// use tutti_graph::{Edge, InPort, OutPort, Source};
/// use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate, Samples};
/// use tutti_graph::NodeKey;
///
/// #[derive(Clone)]
/// struct Half;
/// impl Node for Half {
///     fn shape(&self) -> Shape {
///         Shape::through_unassigned(ChannelLayout::MONO)
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         let (ins, mut outs) = io.split();
///         for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
///             *o = 0.5 * i;
///         }
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
///
/// let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))));
/// let half = NodeKey(1);
/// editor.insert(half, "half", ForkByClone(Half));
/// let topology = &mut editor.spec_mut().topology;
/// topology.inputs = ChannelLayout::MONO;
/// topology.connect(InPort { node: half, port: 0 }, Edge::Direct(Source::Global(0)));
/// topology.set_outputs([Source::Node(OutPort { node: half, port: 0 })]);
/// editor.commit().expect("compiles");
///
/// // On the audio thread:
/// let input = [1.0_f32; 64];
/// let mut output = [0.0_f32; 64];
/// let transport = TransportPosition::new(false, Bpm::of(120.0), Beat(0.0), None);
/// executor.process(64, &transport, &[&input], &mut [&mut output]);
/// assert_eq!(output[0], 0.5);
/// ```
pub struct Editor {
    /// The rings to the paired executor.
    link: Link,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    /// Each key's record: the unit waiting for the next commit, its wake
    /// and fork source, the crossfade or latency cut the commit carries,
    /// whether it is leaving, and its generation count.
    placements: Placements,
    /// The open taps: each reader's width and end latch.
    taps: Outlets<TapId, TapWriter, TapOpened>,
    /// The open event taps.
    event_taps: Outlets<EventTapId, EventTapWriter, ()>,
    /// The open meters: each one's lane capacity.
    meters: Outlets<MeterId, MeterWriter, usize>,
    /// Where the compile lifecycle stands: the plan sent last (what the
    /// executor will be running once the queue drains, and what the next
    /// commit is compiled against), a re-prepare between its two commits,
    /// or the cause a re-prepare left the editor poisoned by.
    life: Lifecycle,
    /// What the executor's host can take.
    limits: Limits,
    /// Events per event slot per block, as the executor was built with: a
    /// fork gets the same.
    event_capacity: usize,
    /// On a **fork** only: the health probe each forked unit's source handed
    /// over, by key (see `src/fork.rs`, "When a forked unit fails").
    fork_health: Vec<(NodeKey, Arc<dyn ForkHealth>)>,
    /// The cells shared with running units: param ranges, lane gains,
    /// modes. Written from the spec by every commit, and by the controls
    /// ([`set_param_range`](Self::set_param_range),
    /// [`set_edge_gain`](Self::set_edge_gain), [`set_mode`](Self::set_mode)
    /// ...) at once.
    cells: LiveCells,
    /// [`revision`](Self::revision): moved by every call that can change
    /// what a reader of the editor sees.
    revision: u64,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("link", &self.link)
            .field("prepare", &self.prepare)
            .field("placements", &self.placements)
            .field("planned", &self.life.plan().is_some())
            .field("committed", &self.life.committed().is_some())
            .field("poisoned", &self.poison_cause())
            .field("limits", &self.limits)
            .field("event_capacity", &self.event_capacity)
            .field("param_ranges", &self.cells.ranges.len())
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// What an open tap's reader was opened with.
struct TapOpened {
    /// Its lanes: the writer the executor holds has as many rings.
    width: usize,
    /// Raised when every node it read was removed.
    end: Arc<tutti_rt::FaultLatch>,
}

/// A panic payload as text.
fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic".to_string())
}

impl Editor {
    /// An empty graph, and the executor that will run it, both for
    /// `prepare`. The only way to build an [`Executor`].
    #[must_use]
    pub fn new(prepare: Prepare) -> (Self, Executor) {
        Self::with_event_capacity(prepare, DEFAULT_EVENT_CAPACITY)
    }

    /// As [`new`](Self::new), with `cap` events per block as the default
    /// capacity of an event output port whose node declares none
    /// ([`Shape::event_capacity`](crate::Shape::event_capacity)) — its
    /// writer's limit, and the rate the delay FIFOs fed by it are sized
    /// from. A declared capacity overrides it for that node's ports.
    #[must_use]
    pub fn with_event_capacity(prepare: Prepare, cap: usize) -> (Self, Executor) {
        let (channels, ends) = channels();
        let (commands, command_rx) = command_channel();
        let editor = Self {
            link: Link::new(channels, commands),
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            placements: Placements::new(),
            taps: Outlets::new(),
            event_taps: Outlets::new(),
            meters: Outlets::new(),
            life: Lifecycle::Unplanned,
            limits: Limits::NONE,
            event_capacity: cap,
            fork_health: Vec::new(),
            cells: LiveCells::new(),
            revision: 0,
        };
        (editor, Executor::new(prepare, cap, ends, command_rx))
    }

    /// What units are prepared for now — after a
    /// [`reprepare`](Self::reprepare), the new one.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
    }

    /// Bounds what this editor may send from now on, **tightening only**:
    /// each field becomes the smaller of the one in force and `limits`', so
    /// `Limits::NONE` changes nothing. Every later
    /// [`commit`](Self::commit), [`package`](Self::package) and
    /// [`reprepare`](Self::reprepare) is refused
    /// ([`CommitError::TooManyOutputs`], [`CommitError::BlockTooLong`]) past
    /// them. Refused itself, changing nothing, when what was already sent
    /// breaks them: the plan sent last, or the `Prepare` in force (or
    /// pending, mid re-prepare).
    ///
    /// Everything reaches the executor through this editor, so a host that
    /// holds the editor while it sets these knows no commit already sent,
    /// and none sent later, can exceed them.
    ///
    /// # Errors
    ///
    /// [`CommitError::TooManyOutputs`] or [`CommitError::BlockTooLong`] when
    /// the plan sent last or the `Prepare` in force already exceeds the
    /// tightened limits; nothing changes.
    pub fn set_limits(&mut self, limits: Limits) -> Result<(), CommitError> {
        // Limits only tighten: each field is the smaller of the one in force
        // and the one asked for, so no caller can undo a host's bound (a
        // looser one would let through a graph the host's buffers cannot
        // run).
        let limits = Limits {
            max_global_outputs: limits
                .max_global_outputs
                .min(self.limits.max_global_outputs),
            max_block: limits.max_block.min(self.limits.max_block),
        };
        self.collect();
        if let Some(plan) = self.life.plan() {
            limits.outputs(plan.global_outputs())?;
        }
        limits.block(self.prepare.max_block().get())?;
        self.limits = limits;
        Ok(())
    }

    /// What this editor was bounded to with [`set_limits`](Self::set_limits).
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether `executor` is the one this editor sends to: the two were
    /// built together by [`Editor::new`].
    pub fn is_paired_with(&self, executor: &Executor) -> bool {
        self.link.is_paired_with(executor)
    }

    /// The paired executor's play clock (`Env::play`), as it published it
    /// at the end of its last block: `(play_epoch, play)` at the next
    /// block's first frame, before a play start there. One wait-free load;
    /// before the first block it is the executor's starting reading (play
    /// 0). What the modulation compiler dates a rate edit's anchor from
    /// (doc 014 §4.3).
    pub fn play_reading(&self) -> crate::PlayReading {
        self.link.play_reading()
    }

    /// The output ports that lost events since the last call, one entry per
    /// port per block they lost some in, oldest first. Control thread; takes
    /// what the paired executor has noted, never blocks it.
    ///
    /// Each entry is exact for its block. The executor's
    /// [`dropped_event_count`](crate::Executor::dropped_event_count) is the
    /// total and is exact whether or not these were drained: a block that
    /// lost at more places than its log holds, or a control side that lets
    /// the ring (256 entries) fill, leaves entries out and the total stands.
    /// Drain it from the tick that collects (the session's `Collect` phase).
    pub fn take_event_losses(&mut self) -> impl Iterator<Item = crate::EventLoss> + '_ {
        std::iter::from_fn(|| self.link.pop_loss())
    }

    /// Refusals since the last call that [`take_event_losses`](Self::take_event_losses)
    /// did not name: a merge's, and those the per-block log (64 places) or the
    /// ring (256 entries) had no room for. Control thread.
    pub fn take_unreported_event_losses(&mut self) -> u64 {
        self.link.take_unreported()
    }

    /// The graph value.
    pub fn spec(&self) -> &GraphSpec {
        &self.spec
    }

    /// The graph value, for wiring. Nodes are added with
    /// [`insert`](Self::insert), not here, so every node has a unit.
    /// Moves the [`revision`](Self::revision), whether or not the borrow
    /// writes.
    pub fn spec_mut(&mut self) -> &mut GraphSpec {
        self.touch();
        &mut self.spec
    }

    /// A count that moves whenever anything the editor shows a reader may
    /// have changed: the spec ([`spec_mut`](Self::spec_mut) included), the
    /// shapes and generations, the plan sent last ([`base`](Self::base)),
    /// a param range or its cells, the [`Prepare`]. Equal revisions mean
    /// none of those changed in between, so a control-side pass that is a
    /// function of them (the modulation reconcile) may skip its work. It
    /// may move with nothing changed, never the other way round. It does
    /// not cover [`play_reading`](Self::play_reading), which the audio
    /// thread moves.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// The shapes the units declared.
    pub fn shapes(&self) -> &Shapes {
        &self.shapes
    }

    /// Declares the bus edge `from -> to` (doc 013, "Buses and coercion
    /// between declared layouts"): the output bus `from` feeds the input bus
    /// `to`, folded between the two declared topologies. The fold is checked
    /// **here**, against the shapes the units declared, so a failure is the
    /// author's call and nothing is stored. The commit lowers it to weighted
    /// lanes ([`GraphSpec::lowered`]); a source already declared is kept once.
    ///
    /// # Errors
    ///
    /// [`BusError::NoSuchNode`], [`BusError::NoSuchBus`] (a bus index the
    /// node does not declare on that side),
    /// [`BusError::Fold`] (the topologies cannot be folded: a channel with no
    /// place, a width that disagrees),
    /// [`BusError::GlobalTooWide`], and [`BusError::Overlaps`] /
    /// [`BusError::GainOnLane`] when a lane would land on an edge or a gain
    /// the spec already has.
    pub fn connect_bus(&mut self, from: BusSource, to: BusRef) -> Result<(), BusError> {
        self.spec
            .check_bus_edge(&self.shapes, from, to, BusFold::Speakers)?;
        self.touch();
        self.spec.connect_bus(from, to);
        Ok(())
    }

    /// Removes the bus edge `from -> to`: its lanes and their weights go with
    /// it at the next commit, and the author's own edges and gains stay.
    /// Returns whether it was declared.
    pub fn disconnect_bus(&mut self, from: BusSource, to: BusRef) -> bool {
        self.touch();
        self.spec.disconnect_bus(from, to)
    }

    /// Declares the bus edge `from -> root`: the output bus `from` feeds the
    /// graph's outputs, folded to the declared root topology
    /// ([`GraphSpec::root`]). Checked here, as [`connect_bus`](Self::connect_bus)
    /// is, and nothing is stored on a failure. An undeclared root refuses it.
    ///
    /// # Errors
    ///
    /// [`BusError::RootUndeclared`], [`BusError::NoSuchNode`],
    /// [`BusError::NoSuchBus`], [`BusError::GlobalTooWide`],
    /// [`BusError::RootFold`], and [`BusError::Overlaps`] /
    /// [`BusError::GainOnLane`] when a lane lands on an output source or a
    /// gain the spec already has.
    pub fn connect_bus_to_root(&mut self, from: BusSource) -> Result<(), BusError> {
        self.spec
            .check_root_bus_edge(&self.shapes, from, BusFold::Speakers)?;
        self.touch();
        self.spec.connect_bus_to_root(from);
        Ok(())
    }

    /// Removes the bus edge `from -> root`, its lanes with it. Returns
    /// whether it was declared.
    pub fn disconnect_bus_from_root(&mut self, from: BusSource) -> bool {
        self.touch();
        self.spec.disconnect_bus_from_root(from)
    }

    /// The spec with its bus edges lowered against the shapes as they stand:
    /// what the next commit compiles ([`GraphSpec::lowered`]).
    ///
    /// # Errors
    ///
    /// As [`GraphSpec::lowered`].
    pub fn lowered(&self) -> Result<Lowering, BusError> {
        self.spec.lowered(&self.shapes)
    }

    /// Lowers `spec` against `shapes` and validates the result, with the
    /// weights of the lanes the lowering made. The one door a commit, a
    /// re-prepare and its resume go through: a spec with no bus edge is
    /// validated as it stands.
    fn lower_and_validate(
        spec: &GraphSpec,
        shapes: &Shapes,
    ) -> Result<(ValidGraph, BusLanes), CommitError> {
        if !spec.has_bus_edges() {
            let valid = spec.validate().map_err(CommitError::Invalid)?;
            return Ok((valid, BTreeMap::new()));
        }
        let lowering = spec
            .lowered(shapes)
            .map_err(|e| CommitError::Compile(CompileError::Bus(e)))?;
        let valid = lowering.spec.validate().map_err(CommitError::Invalid)?;
        Ok((valid, lowering.weights()))
    }

    /// Crossfades sent and not yet drained back: running, waiting, or
    /// ended and waiting for [`collect`](Self::collect). At most
    /// [`FADE_CAPACITY`](crate::FADE_CAPACITY).
    pub fn fades_in_flight(&self) -> usize {
        self.link.fades_in_flight()
    }

    /// Commits sent and not yet drained back.
    pub fn in_flight(&self) -> usize {
        self.link.in_flight()
    }

    /// Whether a [`reprepare`](Self::reprepare) is between its two commits:
    /// until the next [`collect`](Self::collect) after the executor hands the
    /// units back, `commit`, `replace`, `set_latency` and `reprepare` refuse
    /// with [`CommitError::Repreparing`]. A host that must not lose what it
    /// would pass to one of them (a `replace` consumes its node) asks first.
    pub fn is_repreparing(&self) -> bool {
        matches!(self.life, Lifecycle::Repreparing { .. })
    }

    /// The cause the editor was poisoned by, if it is ([`CommitError::Poisoned`]).
    pub fn poison_cause(&self) -> Option<&str> {
        match &self.life {
            Lifecycle::Poisoned { cause, .. } => Some(cause),
            _ => None,
        }
    }

    /// The plan sent last — what the next commit is compiled against.
    pub fn base(&self) -> Option<&Arc<Plan>> {
        self.life.plan()
    }

    /// Where `key`'s forks come from, and the generation of the unit that
    /// handed it over, if it is forkable.
    pub(crate) fn fork_source(&self, key: NodeKey) -> Option<(u32, &dyn ForkSource)> {
        self.placements.fork_source(key)
    }

    /// Watch `health` for the forked unit at `key` (`Editor::fork` only).
    pub(crate) fn watch_fork(&mut self, key: NodeKey, health: Arc<dyn ForkHealth>) {
        self.fork_health.push((key, health));
    }

    /// The forked units' probes, in key order.
    pub(crate) fn fork_probes(&self) -> &[(NodeKey, Arc<dyn ForkHealth>)] {
        &self.fork_health
    }

    /// Events per event slot per block, as the executor was built with.
    pub(crate) fn event_capacity(&self) -> usize {
        self.event_capacity
    }
    /// Delivers `kind` into event input `to` at time `at` — a note, or a
    /// [`ParamRamp`](crate::ParamRamp) as `EventKind::Ramp` — on its exact
    /// frame. Control thread; the command crosses to the executor over a
    /// preallocated ring of plain values, so nothing is freed on the audio
    /// thread. The rules:
    ///
    /// - **`at` is required.** [`At::NextBlock`] is the untimed case, and it
    ///   has to be spelled.
    /// - The port is checked against the plan sent last, and the command is
    ///   delivered to whichever unit holds that key when it falls due.
    /// - A frame already past when the executor sees it lands at offset 0 of
    ///   that block and is counted
    ///   ([`Executor::late_command_count`](crate::Executor::late_command_count)) —
    ///   never dropped. Beats follow a different rule; see below.
    /// - At most [`COMMAND_CAPACITY`](crate::COMMAND_CAPACITY) commands are
    ///   outstanding; past that this returns
    ///   [`ScheduleError::Backpressure`] and sends nothing. A command holds
    ///   its credit until it lands or is [`cancel`](Self::cancel)led — and a
    ///   beat-timed one can wait indefinitely (a stopped transport, a beat
    ///   past the loop end, one a seek jumped over), so cancel what you no
    ///   longer want.
    /// - A [`ParamRamp`](crate::ParamRamp) or a
    ///   [`ParamSegment`](crate::ParamSegment) into a node that does not honour
    ///   offsets sample-accurately is refused
    ///   ([`ScheduleError::ResolutionTooCoarse`]), as a marked edge would be.
    ///
    /// **Frames and beats fall due differently.** A frame is
    /// never dropped: already past, it lands at offset 0 of the next block
    /// and is counted late. A beat fires when the playhead reaches or crosses
    /// it by continuous playback — a loop wrap landing at or after it counts
    /// — and is late only if continuous playback crossed it before the
    /// command was processed; a beat a seek or loop jumps *over* stays
    /// pending until reached or cancelled. Pairing (a note-off for every
    /// note-on) is the caller's job: a note-on that fires and a note-off that
    /// waits is a stuck note, and `cancel` is how to take the other back.
    ///
    /// # Errors
    ///
    /// Nothing is sent on [`ScheduleError::NoPlan`] (nothing committed yet),
    /// [`ScheduleError::NoSuchPort`] (`to` is not an event input of the plan
    /// sent last), [`ScheduleError::ResolutionTooCoarse`],
    /// [`ScheduleError::Backpressure`] or [`ScheduleError::Poisoned`].
    pub fn schedule(
        &mut self,
        at: At,
        to: EventIn,
        kind: EventKind,
    ) -> Result<CommandId, ScheduleError> {
        if self.poison_cause().is_some() {
            return Err(ScheduleError::Poisoned);
        }
        let plan = self.life.plan().ok_or(ScheduleError::NoPlan)?;
        let unit = plan.unit(to.node).filter(|u| to.port < u.shape.event_in);
        let Some(unit) = unit else {
            return Err(ScheduleError::NoSuchPort { to });
        };
        let sink = unit.shape.event_resolution;
        if matches!(kind, EventKind::Ramp(_) | EventKind::Segment(_))
            && !sink.honours(Resolution::Sample)
        {
            return Err(ScheduleError::ResolutionTooCoarse { to, sink });
        }
        self.link.schedule(at, to, kind)
    }

    /// Takes back scheduled command `id`, if it has not landed — freeing its
    /// credit. A no-op for one that has. Travels on its own ring, so it works
    /// even with every credit held; refused with `Backpressure` only when
    /// [`CANCEL_CAPACITY`](crate::CANCEL_CAPACITY) cancels are waiting for
    /// the executor's next block.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::Backpressure`] (above),
    /// [`ScheduleError::UnknownCommand`] for an id this editor never issued,
    /// or [`ScheduleError::Poisoned`].
    pub fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        if self.poison_cause().is_some() {
            return Err(ScheduleError::Poisoned);
        }
        self.link.cancel(id)
    }

    /// Takes back every command scheduled so far that has not landed.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::Backpressure`] when the cancel ring is full, or
    /// [`ScheduleError::Poisoned`].
    pub fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        if self.poison_cause().is_some() {
            return Err(ScheduleError::Poisoned);
        }
        self.link.cancel_all()
    }

    /// Scheduled commands sent and not yet delivered.
    pub fn commands_outstanding(&self) -> usize {
        self.link.commands_outstanding()
    }
}

/// The weighted lanes a lowering made, by edge: the fold's weight and the fade
/// the lane runs.
type BusLanes = BTreeMap<EdgeId, (Amplitude, EdgeFade)>;

/// Param port `at`'s range as `spec` holds it: unbounded without an entry.
fn range_of(spec: &GraphSpec, at: ParamIn) -> ParamRange {
    spec.params
        .get(&at)
        .map_or(ParamRange::UNBOUNDED, |m| m.range)
}

/// Node `key`'s mix range as `spec` holds it: the whole blend without an entry.
fn mix_range_of(spec: &GraphSpec, key: NodeKey) -> ParamRange {
    spec.params
        .get(&ParamIn {
            node: key,
            param: NodeMix::ADDR,
        })
        .map_or(NodeMix::DEFAULT_RANGE, |m| m.range)
}

/// The crossfades `delta` starts: one per fade that is not a plain swap.
/// Each is built into the commit and comes back once on the fade-return
/// ring (`Commit::build`).
fn starting(delta: &Delta) -> usize {
    delta.fades.iter().filter(|(_, f)| !f.is_cut()).count()
}
