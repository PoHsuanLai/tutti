//! The runtime's audio side: [`Executor`], and the commit box that carries an
//! edit to it over a queue and the retired state back.
//!
//! Units exist exactly once, in the executor's store. An edit
//! travels as a **commit box** holding the new plan and only the units that
//! change. [`Editor::new`](crate::Editor::new) builds the editor and the
//! executor as a pair joined by two preallocated single-producer,
//! single-consumer rings:
//!
//! ```text
//!   Editor ──commit()──▶ [ queue, QUEUE_CAPACITY ] ──▶ Executor::process
//!   Editor ◀─collect()── [ return, QUEUE_CAPACITY+1 ] ◀── (applied box)
//! ```
//!
//! At the start of every [`process`](Executor::process) the executor pulls
//! whatever is queued and applies it in FIFO order: it swaps pointers and
//! pushes **the same box** back, now holding everything it replaced — the
//! previous plan, the retired units, and the previous arena, event slots and
//! delay state. So nothing it replaced is freed on its side.
//!
//! Nobody else ever holds a box. That is what makes the protocol simple: a
//! box cannot be dropped unapplied, applied twice, applied to another
//! editor's executor or applied out of order, so every commit is compiled
//! against the plan sent just before it and the executor installs them in
//! that order — a linear chain, with no rollback to get wrong.
//!
//! **The return push cannot fail.** The editor counts a commit as out from
//! the moment it is sent until it has drained the box back, and refuses a
//! commit ([`CommitError::Backpressure`](crate::CommitError)) with
//! [`QUEUE_CAPACITY`] out — so at most that many boxes are ever in the
//! return ring, whose capacity is one more. (Were it ever full regardless,
//! the executor would leak the box rather than free it on the audio thread.)
//!
//! **Where boxes are freed.** Drained boxes are dropped by the editor, on the
//! control thread. If the executor is dropped with commits still queued (or
//! crossfades still running), the rings go with it and those commits and
//! crossfades are dropped on the thread that drops
//! the executor — the control thread in practice, never the callback. If the
//! editor is dropped, the executor keeps running its current plan; boxes it
//! returns afterwards wait in the return ring until the executor is dropped.
//!
//! Applying runs under the [`RtScope::enter`](tutti_rt::RtScope::enter) marker, so a unit or a
//! commit dropped there panics in a debug build. The marker's coverage of
//! applying is **partial**: applying *allocates* (it builds the new arena and
//! delay state) and frees transient allocations of its own — the key maps it
//! builds to carry state across, a ring's previous buffer on a retune, the
//! flush lists — which are plain `Vec`s and `BTreeMap`s the marker does not
//! see. Moving that work to the control side, so applying only swaps
//! pointers, is not done yet. Every type crossing
//! the queue is already `Send` (units, commits) or `Send + Sync` (the plan).
//!
//! # The serial executor
//!
//! (The parallel one, which runs the same op code on a pool's threads, is
//! `par.rs`; [`Executor::set_pool`] turns it on.)
//!
//! Walks [`Plan::ops`] in order, inside a flush-to-zero scope
//! ([`with_no_denormals`](tutti_rt::with_no_denormals)). Per block it never allocates when no commit is
//! pending (see `tests/rt_no_alloc.rs`) and hands every node the **whole
//! block** and its sorted events.
//!
//! **A node op is one record.** The compiler lowers each `Op::Node` into a
//! `NodeRec` (`plan.rs`): store index, generation, arrival, latency plus
//! tail, the port slots, the buffer borrow requests **already sorted**, and
//! a `Form` that picks the borrow. The verifier checks each record against its op, so a
//! call does one indexed load and never sorts. The commonest shapes — at most one audio input and one
//! audio output, or the stereo shapes 0→2, 1→2, 2→1 and 2→2, all with no
//! event ports — borrow their slots directly, and their whole call path is
//! specialised so the per-port loops fold away. An
//! event-free node of any width builds no event table. Per slot, what is
//! known about the block (silent, constant) is one flags byte.
//!
//! **It does not split blocks at a loop wrap.** The whole-block promise is
//! what keeps an out-of-process plugin's declared latency constant, so a
//! transport that loops inside a block is reported through
//! [`TransportPosition::looping`](crate::TransportPosition) and a node that cares computes
//! the wrap position itself.
//!
//! # The silence skip
//!
//! A node is not called — its outputs are written as silence — only when its
//! audio inputs are flagged silent, its event inputs are empty, and:
//!
//! - **for a node with event inputs**, its previous call returned
//!   [`Status::Idle`](crate::Status::Idle): "silent, and nothing pending — park me until input".
//!   Only the node knows whether it is idle. A synth in a delayed attack, or
//!   a sampler playing leading silence under a held note, has quiet inputs
//!   and a silent output and is still busy; it returns `Silent`, and is
//!   called every block.
//! - **for a node without event inputs**, its previous call left every audio
//!   output flagged silent (it returned `Silent`, `Idle`, or masks covering
//!   every channel), and its declared latency **plus** its declared tail have
//!   elapsed since its inputs went quiet. A tail excludes latency
//!   ([`Shape::tail`](crate::Shape::tail)), so a lookahead still holding its
//!   last frames is called until they are out, even while its output reads
//!   silent. A node never called yet is not quiet. Two shapes are never
//!   parked this way: a node with **no audio inputs** (a source — silent
//!   inputs say nothing about it) and a node with **no outputs at all** (a
//!   sink, which runs only for its side effects — a meter, a tap — and whose
//!   skip would save no work downstream).
//!
//! A node that makes no silence claim ([`Status::Modified`](crate::Status::Modified)) is never
//! parked: that is how a node fed out of band (a synth steered through its
//! own queue, a mic monitor) stays callable.
//!
//! # Crossfades
//!
//! [`Editor::replace`](crate::Editor::replace) (rules in `src/fade.rs`). A
//! crossfade lives in the store beside its unit: the unit is the incoming
//! one, and the crossfade holds the outgoing one and a scratch arena for its
//! outputs, built on the control side with the commit. A node op with a
//! fade running leaves the hot path for `fading_node_op`, which runs the
//! outgoing unit, then the incoming one through the general borrow, then
//! blends — touching only the op's own slots — and is never skipped. A
//! replace while one runs waits in the unit's `queued` slot.
//!
//! **A fade does not hold its commit**: the box goes back as soon as it is
//! applied, like any other. Each crossfade comes back on its own, on a
//! third ring (the **fade-return ring**, [`FADE_CAPACITY`] long), when it
//! ends — or is cut, superseded, or placed with nothing to fade from —
//! carrying its outgoing (or never-run) unit, so that unit is freed on the
//! control side. The editor reserves a slot on that ring for every fade a
//! commit starts, before sending it, and refuses a commit that would take
//! more than [`FADE_CAPACITY`] fades in flight; so the push cannot fail.
//!
//! # Scheduled commands
//!
//! A third ring carries [`Editor::schedule`](crate::Editor::schedule)'s
//! timestamped commands (see the `command` module, `src/command.rs`). At the
//! start of each block, after applying commits and before the first op, the
//! executor pulls them into a preallocated pending list and resolves each
//! against the block's [`Env`](crate::Env) — so a beat is resolved against the transport
//! of the block it falls in. What lands this block is merged into its sink's
//! event input as one more source after the port's own events, in a buffer
//! sized when applying; a sink with a landing command is never skipped.
//!
//! # Flushed events
//!
//! When an event delay or event feedback disappears in a recompile and its
//! sink survives, its pending events are delivered on the sink's next call:
//! each flush keeps its events' relative spacing, shifted so the earliest
//! lands at offset 0, so a note-on/note-off pair keeps its length; only what
//! falls past that block's end is clamped to its last frame. Flushed events
//! sort ahead of the block's own on ties. They can reach a **replaced** unit
//! at that sink — a note-off for a note-on its predecessor saw — which a
//! unit must tolerate (a note-off for a note that is not sounding is a
//! no-op in MIDI).
//!
//! # The chase
//!
//! A node replaced by a commit (a crossfade, a replace, an insert at an
//! existing key) may write the state its successor starts from
//! ([`Node::chase`](crate::Node::chase), declared with
//! [`Shape::with_chase`](crate::Shape::with_chase)). Applying the commit calls
//! it once, before any task of the next block runs, into a buffer sized when
//! the plan was built, and the events are **flushed events** to the
//! successor's first event input: offset 0 of its first block, ahead of
//! everything else there. A successor without an event input drops them,
//! counted, as is what the buffer refused. A fade's outgoing node is not
//! sent anything.
//!
//! A replace that waits behind a running fade is chased too, a block later:
//! its predecessor (the unit fading in) writes its state when the replace is
//! applied, the crossfade holds it (a `Vec` allocated there, with the apply
//! that already allocates), and the plan's state keeps an injection for the
//! successor sized for it. When the running fade ends (`end_fades`) the
//! successor takes over, and the held events are delivered as flushed events
//! to its first event input at the next block, at offset 0, ahead of
//! everything else there, with no allocation. What the buffer refused, and
//! what a successor with no event input cannot take, is counted as
//! dropped. A queued replace that a later one supersedes, or that a cut
//! discards, takes its held events with it.
//!
//! # Held keys
//!
//! A unit whose event outputs carry notes ([`Shape::note_events`](crate::Shape::note_events)) has the keys
//! each port holds tracked after its op (`src/held.rs`). When a commit removes
//! an event edge, or its source is replaced or removed, the sink that survives
//! is sent the pedal-ups and note-offs of what the source held, after the
//! events flushed to it. A new edge sends nothing: the off of a note its sink
//! never saw begin reaches it, which ends nothing. While a replace with a fade
//! waits behind a running one, the running replacement keeps its table; when the
//! waiting unit takes over, the keys of the one that stopped are written ahead
//! of the next block's events on the unit's own output.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use ringbuf::{HeapCons, HeapProd};
use tutti_types::PlayReading;
use tutti_types::{Frame, Tail};

use crate::NodeKey;
use crate::command::CommandRx;
use crate::cost::Schedule;
use crate::event::Event;
use crate::held::Keys;
use crate::history::TransportHistory;
use crate::loss::{Dropped, EventLoss};
use crate::node::Prepare;
use crate::par::{Par, Pool};
use crate::plan::Plan;
use crate::play::PlayState;
use crate::slots::{AudioSlots, EventSlots, SlotFlags};
use crate::{EdgeFade, EdgeId};

mod commit;
mod mode_op;
mod node_op;
mod ops;
mod render;
mod state;
mod update;

pub(crate) use commit::{Channels, Commit, Crossfade, ExecutorEnds, Opened, channels};
use node_op::{CONSTANT, flag, run_node_op, run_params};
pub(crate) use node_op::{Head, OpState, SILENT, node_dispatch};
pub(crate) use ops::{
    audio_ring, capture_op, delay_op, event_delay_op, event_merge_op, event_ring, global_in,
    output_op, sum_op,
};

use state::State;
pub(crate) use state::{Inject, Ring, Unit};

/// Events per block an event output port holds when its node declares no
/// [`Shape::event_capacity`](crate::Shape::event_capacity), unless
/// configured otherwise ([`Editor::with_event_capacity`](crate::Editor::with_event_capacity)).
///
/// A hard cap, preallocated so the audio thread never grows a buffer. Past
/// it the port refuses the newest event, a note-off excepted until
/// [`note_off_room`](crate::note_off_room)
/// ([`EventRejected::Full`](crate::EventRejected::Full), returned to the
/// node), and the executor counts it in
/// [`dropped_event_count`](Executor::dropped_event_count); an engine forwards that
/// count to its load meter. A node that can emit more per block declares
/// its own capacity.
pub const DEFAULT_EVENT_CAPACITY: usize = 512;

/// Commits that may be out at once: sent and not yet drained back. The
/// queue holds this many; the return ring one more.
pub const QUEUE_CAPACITY: usize = 4;

/// Crossfades that may be in flight at once — started or waiting, and not
/// yet drained back by `Editor::collect` — across the whole graph. The
/// fade-return ring holds this many, and a commit that would start more is
/// refused (`CommitError::Backpressure`) before anything is sent. Each is a
/// replace with a fade still running or waiting, so the cap is far above
/// what a session does; it exists so the audio thread's push can never
/// fail.
pub const FADE_CAPACITY: usize = 256;

/// How many ended edge fades the executor can have waiting for the editor. A
/// lane that finds the ring full offers its report again next block, so a
/// full ring delays an edge's removal and loses nothing.
const EDGE_FADE_CAPACITY: usize = 1024;

/// The audio-thread half of a graph: runs the current [`Plan`] one block at a
/// time, serially or spread over a [`Pool`] (see [`set_pool`](Self::set_pool)).
///
/// Built with its [`Editor`](crate::Editor) by
/// [`Editor::new`](crate::Editor::new): the two share one [`Prepare`] and
/// one queue pair, and the executor applies that editor's commits, in order,
/// at the start of each block. It owns every running unit; the editor never
/// touches one.
///
/// Call [`process`](Self::process) from the audio callback. With no commit
/// queued it does not allocate, lock or block; applying a commit does
/// allocate (it builds the new arena and delay state), and everything a
/// commit replaces is sent back to the editor to be freed on the control
/// thread. The executor walks the plan's ops (serially, or as tasks on a
/// pool), hands every node the
/// **whole block** and its sorted events — it never splits a block at an
/// event, a loop wrap or a transport change — and skips a node whose inputs
/// are silent when its last [`Status`](crate::Status) (and, for a node without event
/// inputs, its declared tail) says it has nothing more to produce.
pub struct Executor {
    prepare: Prepare,
    event_cap: usize,
    /// The executor's end of the rings to its editor.
    inbox: Inbox,
    plan: Option<Arc<Plan>>,
    store: Vec<Option<Unit>>,
    state: State,
    /// Where the graph is in time.
    clock: Clock,
    /// Events dropped: the running total, and this block's log of where.
    dropped: Dropped,
    /// Whether a re-prepare has the units out.
    suspension: Suspension,
    /// The held-key tables of the units a re-prepare checked out, for the
    /// resume commit to hand back.
    parked: Vec<(NodeKey, Keys)>,
    /// Where a replaced node writes its state for its successor
    /// (`Node::chase`), sized when a plan is built from what its nodes
    /// declare (`Shape::chase`), so the swap never allocates it.
    chase: Vec<Event>,
    /// The pool blocks run on, if any (see [`set_pool`](Self::set_pool)).
    pool: Option<Arc<dyn Pool>>,
}

/// The executor's end of the rings to its editor, and the commands it
/// delivers.
struct Inbox {
    queue: HeapCons<Box<Commit>>,
    back: HeapProd<Box<Commit>>,
    /// Where crossfades go back (see the module docs, "Crossfades").
    fade_back: HeapProd<Box<Crossfade>>,
    /// Where each block's log of dropped events goes, for the editor to
    /// drain.
    losses: HeapProd<EventLoss>,
    unreported: Arc<AtomicU64>,
    /// Where an edge whose declick fade ended is reported.
    edge_fades: HeapProd<(EdgeId, EdgeFade)>,
    commands: CommandRx,
    /// `seq` of the last commit applied.
    applied: u64,
}

/// Where the graph is in time.
struct Clock {
    frame: Frame,
    /// The transport over the last frames, for nodes behind a latent path
    /// (`Cx::transport_at`). Fed every block, whatever it renders.
    history: TransportHistory,
    /// The graph's play clock (`src/play.rs`): kept here, handed to every
    /// node in its `Env`, published for the editor.
    play: PlayState,
}

/// Whether a re-prepare has the units out.
#[derive(Clone, Copy, Debug)]
enum Suspension {
    /// The units are in the store.
    Running,
    /// Between a re-prepare's two commits: every unit is checked out, and
    /// blocks render silence.
    Suspended {
        /// The `Prepare` the resume commit adopts when it lands. Not
        /// before: until then the device may still hand blocks sized for
        /// the old one (a shrinking `MaxBlock` must not fail them).
        next: Prepare,
        /// The sample rate changed: the resume's rebuild carries no
        /// time-based state (see [`Editor::reprepare`](crate::Editor::reprepare)).
        reset_time: bool,
    },
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("prepare", &self.prepare)
            .field("event_cap", &self.event_cap)
            .field("planned", &self.plan.is_some())
            .field("frame", &self.clock.frame)
            .field("dropped", &self.dropped.total())
            .field("applied", &self.inbox.applied)
            .field("suspension", &self.suspension)
            .field("play", &self.clock.play)
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}

/// The parallel state for `plan` on `pool`, when it would run in
/// parallel at all: when the cost model ([`Plan::schedule`]) says spreading
/// pays for some block length on this pool (which needs a pool of more
/// than one participant, and more than one task).
fn par_for(
    plan: &Plan,
    pool: Option<&Arc<dyn Pool>>,
    store_len: usize,
    inject: &[Inject],
    overlay: usize,
) -> Option<Par> {
    let participants = pool?.participants();
    match plan.schedule(participants) {
        Schedule::Serial => None,
        Schedule::Parallel { wake, from } => Some(Par::new(
            plan,
            participants,
            (wake, from.0),
            store_len,
            inject.iter().map(|i| i.unit),
            overlay,
        )),
    }
}

/// Whether `quiet` frames of quiet input cover `park_after`, a node's
/// latency then its tail (`NodeRec::park_after`).
fn tail_elapsed(park_after: Tail, quiet: u64) -> bool {
    match park_after {
        Tail::None => true,
        Tail::Finite(n) => quiet >= n.get() as u64,
        Tail::Unknown | Tail::Unbounded => false,
    }
}

impl Executor {
    pub(crate) fn new(
        prepare: Prepare,
        cap: usize,
        ends: ExecutorEnds,
        commands: CommandRx,
    ) -> Self {
        Self {
            prepare,
            event_cap: cap,
            inbox: Inbox {
                queue: ends.queue,
                back: ends.back,
                fade_back: ends.fade_back,
                losses: ends.losses,
                unreported: ends.unreported,
                edge_fades: ends.edge_fades,
                commands,
                applied: 0,
            },
            plan: None,
            store: Vec::new(),
            state: State::empty(prepare.max_block().get()),
            clock: Clock {
                frame: Frame::ZERO,
                history: TransportHistory::new(),
                play: PlayState::new(ends.play),
            },
            dropped: Dropped::new(),
            suspension: Suspension::Running,
            parked: Vec::new(),
            chase: Vec::new(),
            pool: None,
        }
    }

    /// Run blocks on `pool` from now on — the parallel executor (see the
    /// `par` module docs, `src/par.rs`) — or, with `None`, on the calling
    /// thread alone. Returns the pool it replaces.
    ///
    /// **Control thread**: it builds the parallel state for the running
    /// plan, which allocates. Set it before the executor goes to the audio
    /// thread (before `Engine::new`, or before an export renders).
    ///
    /// Whether a block spreads is the cost model's call
    /// ([`Plan::schedule`]), made for each plan when it meets the pool: a
    /// plan too cheap to pay for dispatch, a block shorter than the length
    /// it pays from, a plan of one task and a pool of one participant all
    /// run the serial walk. The serial executor is the parallel one's
    /// one-worker case, and both render the same samples.
    pub fn set_pool(&mut self, pool: Option<Arc<dyn Pool>>) -> Option<Arc<dyn Pool>> {
        let old = std::mem::replace(&mut self.pool, pool);
        self.state.par = self.plan.as_deref().and_then(|plan| {
            par_for(
                plan,
                self.pool.as_ref(),
                self.store.len(),
                &self.state.inject,
                self.state.overlay.capacity(),
            )
        });
        old
    }

    /// The pool blocks run on, if any.
    pub fn pool(&self) -> Option<&Arc<dyn Pool>> {
        self.pool.as_ref()
    }

    /// Whether the running plan's blocks are spread across the pool: a pool
    /// is set and [`Plan::schedule`] says spreading pays for the plan on
    /// it (blocks shorter than its `from` still run the serial walk).
    /// Otherwise every block runs the serial walk.
    pub fn is_parallel(&self) -> bool {
        self.state.par.is_some()
    }

    /// The command ring's executor end, for the pair check.
    pub(crate) fn commands(&self) -> &CommandRx {
        &self.inbox.commands
    }

    /// What this executor, and every unit it runs, is prepared for. During a
    /// re-prepare, the old one until the resume commit lands.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
    }

    /// The `Prepare` a re-prepare in progress will adopt: `Some` from the
    /// block its first commit is applied on — the units checked out, and on
    /// a rate change [`frame`](Self::frame) and every pending frame-timed
    /// command already rescaled — until its resume lands and
    /// [`prepare`](Self::prepare) becomes it.
    ///
    /// So the rate time is counted in *now* is this one's while it is
    /// `Some`: a host that keeps a clock beside the executor (the engine's
    /// transport clock, its own frame-timed commands) follows the rate here,
    /// on the same block the executor's clock moves, not a resume later.
    pub fn pending_prepare(&self) -> Option<&Prepare> {
        match &self.suspension {
            Suspension::Running => None,
            Suspension::Suspended { next, .. } => Some(next),
        }
    }

    /// The plan running now.
    pub fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// The graph's clock, which [`Env::frame`](crate::Env::frame) reads: samples at the current
    /// rate since start. It tracks device time — blocks rendered as silence
    /// while a re-prepare has the units checked out count — and a rate change
    /// rescales it (and every pending frame-timed command) to the same
    /// wall-clock time at the new rate, rounded to the nearest frame.
    pub fn frame(&self) -> Frame {
        self.clock.frame
    }

    /// The play clock (`Env::play`) at the next block's first frame, before
    /// a play start there: what the last block left it at. The control side
    /// reads the same through [`Editor::play_reading`](crate::Editor::play_reading).
    pub fn play_reading(&self) -> PlayReading {
        self.clock.play.reading()
    }

    /// Events refused so far: writer overflow, and delay or feedback FIFO
    /// overflow (which never drops a note-off while anything else can go).
    /// A merge never drops: its slot holds all its inputs.
    pub fn dropped_event_count(&self) -> u64 {
        self.dropped.total()
    }

    /// Events that never arrived: [`dropped_event_count`](Self::dropped_event_count)
    /// less the refusals a node recovered (a note-off it kept owed and wrote
    /// in a later block, `EventWriter::recovered`). What an export fails on.
    /// A refusal still owed counts as lost.
    pub fn lost_event_count(&self) -> u64 {
        self.dropped.lost()
    }

    /// Refusals a node took back so far (`EventWriter::recovered`): with
    /// [`dropped_event_count`](Self::dropped_event_count), the two figures
    /// [`lost_event_count`](Self::lost_event_count) is made of. Over a span,
    /// the loss is the change in one less the change in the other, since a
    /// recovery in the span may be of a refusal before it.
    pub fn recovered_event_count(&self) -> u64 {
        self.dropped.recovered()
    }

    /// Scheduled commands that were already past due when the executor
    /// first saw them, and so landed at offset 0 of that block instead of on
    /// their frame. Never dropped — see [`Editor::schedule`](crate::Editor::schedule).
    pub fn late_command_count(&self) -> u64 {
        self.inbox.commands.late()
    }

    /// Scheduled commands whose node or event port was gone by the time they
    /// fell due, so there was nowhere to deliver them.
    pub fn unrouted_command_count(&self) -> u64 {
        self.inbox.commands.unrouted()
    }

    /// Scheduled commands cancelled before they landed.
    pub fn cancelled_command_count(&self) -> u64 {
        self.inbox.commands.cancelled()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tutti_types::{At, MaxBlock, SampleRate, Samples};

    use super::*;
    use crate::event::EventKind;
    use crate::io::Io;
    use crate::node::{Cx, Env, Node, Status, TransportChanges, TransportPosition};
    use tutti_ump::Ump;

    /// Counts the events it receives.
    struct Count(Arc<AtomicUsize>);

    impl Node for Count {
        fn shape(&self) -> crate::node::Shape {
            crate::node::Shape::EMPTY.with_events(1, 0)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
            self.0.fetch_add(io.events(0).len(), Ordering::Relaxed);
            Status::Silent
        }
        fn reset(&mut self) {}
    }

    /// The hold, staged deterministically: the race it guards (the executor
    /// pulls a command before the commit it was checked against, which only
    /// a concurrent editor can cause — see `tests/events/commands.rs` for the
    /// threaded version) is reproduced by running the executor's command
    /// pass before it applies the queued commit. The command must wait, not
    /// be judged against the old plan (where its node does not exist), and
    /// land once the commit does.
    #[test]
    fn a_command_seen_before_its_commit_waits_for_it() {
        let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64)));
        let (mut ed, mut exec) = crate::Editor::new(prepare);
        let seen = Arc::new(AtomicUsize::new(0));
        ed.insert(NodeKey(1), "a", crate::Unforkable(Count(Arc::clone(&seen))));
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();

        ed.insert(NodeKey(2), "b", crate::Unforkable(Count(Arc::clone(&seen))));
        ed.commit().expect("queued, not applied");
        let to = crate::EventIn {
            node: NodeKey(2),
            port: 0,
        };
        ed.schedule(At::NextBlock, to, EventKind::Midi(Ump([1, 0, 0, 0])))
            .expect("checked against the queued commit");

        // The command is pulled first, against the plan still running.
        let plan = Arc::clone(exec.plan.as_ref().expect("a plan"));
        let env = Env {
            frame: exec.clock.frame,
            sample_rate: prepare.sample_rate(),
            block_len: Samples(64),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: crate::PlayClock::ZERO,
        };
        let mut has_due = vec![false; plan.units.len()];
        exec.inbox
            .commands
            .gather(&env, &plan, exec.inbox.applied, &mut has_due);
        assert_eq!(
            exec.unrouted_command_count(),
            0,
            "judged against a stale plan"
        );
        assert_eq!(ed.commands_outstanding(), 1, "it waits");

        // Then the commit lands, and the command with it.
        exec.process(64, &TransportPosition::default(), &[], &mut []);
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        assert_eq!(ed.commands_outstanding(), 0);
        assert_eq!(exec.unrouted_command_count(), 0);
    }
}
