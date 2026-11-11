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
//! Applying runs under the [`RtScope::enter`] marker, so a unit or a
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
//! ([`with_no_denormals`]). Per block it never allocates when no commit is
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
//! [`Transport::looping`](crate::Transport) and a node that cares computes
//! the wrap position itself.
//!
//! # The silence skip
//!
//! A node is not called — its outputs are written as silence — only when its
//! audio inputs are flagged silent, its event inputs are empty, and:
//!
//! - **for a node with event inputs**, its previous call returned
//!   [`Status::Idle`]: "silent, and nothing pending — park me until input".
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
//! A node that makes no silence claim ([`Status::Modified`]) is never
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
//! against the block's [`Env`] — so a beat is resolved against the transport
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

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::NodeKey;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_rt::{RtScope, with_no_denormals};
use tutti_types::{Frame, MaxBlock, Samples, Tail};

use crate::arena::{Arena, Role};
use crate::command::{CommandRx, Deliveries, overlay_capacity};
use crate::cost::Schedule;
use crate::event::PortCount;
use crate::event::{Event, EventWriter, SortedEvents, merge_into};
use crate::event_tap::EventTapWriter;
use crate::fade::CrossfadeCurve;
use crate::gain::{self, GainCell, SumLane, Weight};
use crate::held::{Held, Keys, Tables};
use crate::history::TransportHistory;
use crate::io::Io;
use crate::kernels::{AudioRing, EventFifo};
use crate::loss::{Dropped, EventLoss};
use crate::meter::{MeterWriter, meter_op};
use crate::mode::{BypassLine, ModeState};
use crate::node::{
    ConstantMask, Cx, Env, InPlaceMask, MAX_PORTS, Node, Prepare, SilenceMask, Status, Transport,
    TransportChanges,
};
use crate::par::{Block, Par, Pool};
use crate::param::{ParamInput, ParamState};
use crate::plan::GainPortOp;
use crate::plan::{DelayKey, Delta, Direct, FeedbackKey, Form, NodeRec, Op, Plan, UnitIdx};
use crate::play::{PlayClock, PlayReader, PlayState};
use crate::slots::{AudioSlots, EventSlots, SlotFlags, borrow_events, event_pair};
use crate::spec::{EventIn, EventOut};
use crate::tap::TapWriter;
use crate::{EdgeId, EventTapId, MeterId, TapId};
use tutti_types::PlayReading;

mod mode_op;

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

/// A unit, owned. Crate-private, so the only code that can replace or drop
/// the box is this crate's; its drop is checked against the audio-thread
/// marker (`RtScope::check_outside`). This crate must *run* its
/// units, so the box hands `&mut` out.
pub(crate) struct NodeBox(Box<dyn Node>);

impl NodeBox {
    pub(crate) fn new(node: Box<dyn Node>) -> Self {
        Self(node)
    }

    /// The unit, out of its box — control side, to re-prepare it. Leaves a
    /// zero-sized placeholder behind for the box's own drop, which therefore
    /// frees nothing (a `Box` of a zero-sized type never allocated).
    pub(crate) fn into_inner(mut self) -> Box<dyn Node> {
        std::mem::replace(&mut self.0, Box::new(Vacant))
    }
}

/// What a [`NodeBox`] holds once its unit has been taken out.
struct Vacant;

impl Node for Vacant {
    fn shape(&self) -> crate::node::Shape {
        crate::node::Shape::audio(
            tutti_types::ChannelLayout::EMPTY,
            tutti_types::ChannelLayout::EMPTY,
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

impl Deref for NodeBox {
    type Target = dyn Node;
    fn deref(&self) -> &(dyn Node + 'static) {
        &*self.0
    }
}

impl DerefMut for NodeBox {
    fn deref_mut(&mut self) -> &mut (dyn Node + 'static) {
        &mut *self.0
    }
}

impl Drop for NodeBox {
    fn drop(&mut self) {
        RtScope::check_outside("a unit");
    }
}

/// One crossfade's own state (see the `fade` module, `src/fade.rs`). Built on
/// the control side with the commit that asks for it, so its scratch is
/// allocated there; it travels back in a commit box, so it is freed there
/// too.
pub(crate) struct Crossfade {
    key: NodeKey,
    /// The unit fading out, once the fade has started.
    out: Option<NodeBox>,
    /// The unit fading in, while this fade waits behind the one running at
    /// its key. It moves into the store when this fade starts.
    waiting: Option<NodeBox>,
    /// The outgoing unit's outputs, one aligned slot per channel.
    scratch: Arena,
    /// Frames of the fade rendered so far.
    done: usize,
    len: usize,
    curve: CrossfadeCurve,
}

impl Crossfade {
    /// The units it carries back: the outgoing one, a waiting one that
    /// never ran, or both after a re-prepare's cut.
    pub(crate) fn units(&self) -> usize {
        usize::from(self.out.is_some()) + usize::from(self.waiting.is_some())
    }

    /// Its key.
    pub(crate) fn key(&self) -> NodeKey {
        self.key
    }
}

impl Drop for Crossfade {
    fn drop(&mut self) {
        RtScope::check_outside("a crossfade");
    }
}

/// A unit a commit places, with everything built for it on the control
/// side.
struct Incoming {
    idx: UnitIdx,
    generation: u32,
    node: NodeBox,
    /// The crossfade it arrives with, over the unit at its key.
    fade: Option<Box<Crossfade>>,
    params: ParamState,
    mode: ModeState,
}

/// A kept unit's rebuilt bypass line.
struct Relined {
    idx: UnitIdx,
    latency: usize,
    line: Option<BypassLine>,
}

/// A plan and the unit changes that go with it — what travels to the
/// executor and back. Crate-private: nobody outside the pair ever holds one.
///
/// Before it is applied it holds the new plan and the incoming units; after,
/// everything the executor replaced. Dropping it on the audio thread panics
/// in a debug build.
pub(crate) struct Commit {
    /// The editor's count of commits sent, this one included: a scheduled
    /// command checked against this commit waits until it is applied.
    seq: u64,
    /// A re-prepare's first half: check every unit out and adopt this
    /// `Prepare` (see `Editor::reprepare`). Carries no plan.
    suspend: Option<Prepare>,
    plan: Option<Arc<Plan>>,
    delta: Delta,
    incoming: Vec<Incoming>,
    /// Kept units whose declared latency changed, each with its rebuilt
    /// bypass line (`Editor::set_latency`).
    relined: Vec<Relined>,
    retired: Vec<(NodeKey, NodeBox)>,
    /// Param state the executor let go of, carried back to be freed on the
    /// control side: a retired unit's, and one built for an incoming unit
    /// that crossfades in over one already at its key, which keeps its own
    /// (the port's modulation runs on across a replace).
    spare_params: Vec<ParamState>,
    /// Mode state the executor let go of (a retired unit's, an unused
    /// incoming one's), freed on the control side.
    spare_modes: Vec<ModeState>,
    /// Bypass lines a rebuilt one replaced.
    spare_lines: Vec<BypassLine>,
    /// Held-key tables the executor let go of, freed on the control side.
    spare_held: Vec<Tables>,
    old_state: Option<State>,
    /// Tap producers and meter cells the editor opened since the last commit.
    opened: Opened,
}

/// What the editor hands the executor with a commit: what it opened since its
/// last one (a tap's producers and a meter's cells), and the cell of every
/// weighted lane the plan lists (`Plan::sum_lanes`).
#[derive(Debug, Default)]
pub(crate) struct Opened {
    /// The live base of each weighted lane of the plan, by edge. The editor
    /// owns the cells and keeps one per edge across commits.
    pub(crate) gains: BTreeMap<EdgeId, GainCell>,
    pub(crate) taps: Vec<(TapId, TapWriter)>,
    pub(crate) event_taps: Vec<(EventTapId, EventTapWriter)>,
    pub(crate) meters: Vec<(MeterId, MeterWriter)>,
}

impl Drop for Commit {
    fn drop(&mut self) {
        RtScope::check_outside("Commit");
    }
}

impl Commit {
    /// `params` builds each incoming unit's param state from its key and
    /// declared port count (the editor's range cells).
    pub(crate) fn build(
        seq: u64,
        plan: Arc<Plan>,
        delta: Delta,
        mut units: BTreeMap<NodeKey, Box<dyn Node>>,
        mut params: impl FnMut(NodeKey, usize, usize) -> ParamState,
        mut modes: impl FnMut(NodeKey) -> ModeState,
        opened: Opened,
    ) -> Box<Commit> {
        let wanted = delta
            .insert
            .iter()
            .copied()
            .chain(delta.replace.iter().map(|&(_, new)| new));
        let max_block = plan.prepare.max_block().get();
        let incoming = wanted
            .map(|p| {
                let unit = units
                    .remove(&p.key)
                    .unwrap_or_else(|| panic!("no unit supplied for node {}", p.key.0));
                // A fade's scratch holds the outgoing unit's outputs, which
                // have the incoming unit's width (`verify_fades`).
                let fade = delta
                    .fades
                    .iter()
                    .find(|(k, f)| *k == p.key && !f.is_cut())
                    .map(|&(key, f)| {
                        let width = plan.unit(key).map_or(0, |u| u.shape.audio_out.count());
                        Box::new(Crossfade {
                            key,
                            out: None,
                            waiting: None,
                            scratch: Arena::new(width as usize, max_block),
                            done: 0,
                            len: f.duration.get(),
                            curve: f.curve,
                        })
                    });
                // The unit's param state, sized here so the audio thread
                // never allocates it (see the `param` module).
                let params = params(
                    p.key,
                    plan.unit(p.key).map_or(0, |u| u.shape.params.len()),
                    max_block,
                );
                Incoming {
                    idx: p.idx,
                    generation: p.generation,
                    node: NodeBox::new(unit),
                    fade,
                    params,
                    mode: modes(p.key),
                }
            })
            .collect::<Vec<_>>();
        // A kept unit whose declared latency changed gets the line its new
        // latency needs, built here (`Editor::set_latency` cuts it).
        let relined = delta
            .cuts
            .iter()
            .filter_map(|p| {
                let shape = &plan.unit(p.key)?.shape;
                Some(Relined {
                    idx: p.idx,
                    latency: shape.latency.samples().get(),
                    line: BypassLine::for_shape(shape),
                })
            })
            .collect::<Vec<_>>();
        assert!(
            units.is_empty(),
            "units supplied for nodes the delta does not place: {:?}",
            units.keys().collect::<Vec<_>>()
        );
        // Reserved here so applying moves retirees in without growing.
        let retired = Vec::with_capacity(delta.retire.len() + delta.replace.len());
        // Every incoming unit's may come back unused, and every unit leaving
        // brings its own.
        let spare_params =
            Vec::with_capacity(incoming.len() + delta.retire.len() + delta.replace.len());
        let delta_units = delta.retire.len() + delta.replace.len();
        let spare_modes = Vec::with_capacity(incoming.len() + delta_units);
        let spare_lines = Vec::with_capacity(relined.len());
        Box::new(Self {
            seq,
            suspend: None,
            plan: Some(plan),
            delta,
            incoming,
            relined,
            retired,
            spare_params,
            spare_modes,
            spare_lines,
            spare_held: Vec::with_capacity(delta_units),
            old_state: None,
            opened,
        })
    }

    /// A re-prepare's first half: when applied, every unit leaves the store
    /// and comes back in this box, and the executor adopts `prepare`.
    /// `units` is how many the running plan has, reserved here so applying
    /// moves them in without growing.
    pub(crate) fn suspend(seq: u64, prepare: Prepare, units: usize) -> Box<Commit> {
        Box::new(Self {
            seq,
            suspend: Some(prepare),
            plan: None,
            delta: Delta::default(),
            incoming: Vec::new(),
            relined: Vec::new(),
            retired: Vec::with_capacity(units),
            spare_params: Vec::with_capacity(units),
            spare_modes: Vec::with_capacity(units),
            spare_lines: Vec::new(),
            spare_held: Vec::with_capacity(units),
            old_state: None,
            opened: Opened::default(),
        })
    }

    /// Whether this commit crossfades `key` in.
    fn fades_in(&self, key: NodeKey) -> bool {
        self.incoming
            .iter()
            .any(|i| i.fade.as_ref().is_some_and(|x| x.key == key))
    }

    /// Whether this box is a re-prepare's first half.
    pub(crate) fn is_suspend(&self) -> bool {
        self.suspend.is_some()
    }

    /// The units applying removed, by key.
    pub(crate) fn retired(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.retired.iter().map(|(k, _)| *k)
    }

    /// The units applying removed, out of their boxes (control side).
    pub(crate) fn take_retired(&mut self) -> Vec<(NodeKey, Box<dyn Node>)> {
        self.retired
            .drain(..)
            .map(|(k, b)| (k, b.into_inner()))
            .collect()
    }
}

/// Send an applied box back to the editor. The push cannot fail by the
/// credit count (module docs); were it ever full, leak rather than free on
/// the audio thread.
fn send_back(back: &mut HeapProd<Box<Commit>>, commit: Box<Commit>) {
    if let Err(full) = back.try_push(commit) {
        debug_assert!(false, "the return ring is full");
        std::mem::forget(full);
    }
}

/// Send a crossfade that ended, or was cut, superseded or never started,
/// back to the editor on the fade-return ring, with the units it carries.
/// The push cannot fail: the editor reserved this crossfade's slot before
/// sending the commit that built it (module docs). Were it ever full, leak
/// rather than free on the audio thread.
fn retire_fade(fade_back: &mut HeapProd<Box<Crossfade>>, x: Box<Crossfade>) {
    if let Err(full) = fade_back.try_push(x) {
        debug_assert!(false, "the fade-return ring is full");
        std::mem::forget(full);
    }
}

/// Cut `u`'s crossfades: the newest unit at the key (a waiting one, if
/// any — the one the plan's generation names) becomes `u`'s unit, and the
/// others go back on the fade-return ring with their crossfades.
///
/// Returns the keys of the unit that stopped writing, if the cut displaced
/// one (`Keys::displace`).
fn cut_fades(u: &mut Unit, fade_back: &mut HeapProd<Box<Crossfade>>) -> Option<Tables> {
    let mut stale = None;
    if let Some(mut q) = u.queued.take() {
        let newest = q.waiting.take().expect("a queued fade holds its unit");
        q.out = Some(std::mem::replace(&mut u.node, newest));
        retire_fade(fade_back, q);
        stale = Some(u.keys.displace());
    }
    if let Some(f) = u.fade.take() {
        retire_fade(fade_back, f);
        // A cut is a step: never skip the next call on stale flags.
        u.quiet = 0;
        u.last_quiet = false;
        u.last_idle = false;
        u.reported = None;
    }
    stale
}

/// The queue pair between an editor and its executor.
pub(crate) struct Channels {
    pub(crate) to_executor: HeapProd<Box<Commit>>,
    pub(crate) returned: HeapCons<Box<Commit>>,
    /// Crossfades coming back (see the module docs, "Crossfades").
    pub(crate) faded: HeapCons<Box<Crossfade>>,
    /// The executor's play clock, as it last published it.
    pub(crate) play: PlayReader,
    /// Where the executor says which ports lost events.
    pub(crate) losses: HeapCons<EventLoss>,
    /// Refusals the log and the ring could not name.
    pub(crate) unreported: Arc<AtomicU64>,
}

/// The executor's ends of the rings.
pub(crate) struct ExecutorEnds {
    queue: HeapCons<Box<Commit>>,
    back: HeapProd<Box<Commit>>,
    fade_back: HeapProd<Box<Crossfade>>,
    play: Arc<AtomicU64>,
    losses: HeapProd<EventLoss>,
    unreported: Arc<AtomicU64>,
}

/// Build the rings: the editor's ends and the executor's.
pub(crate) fn channels() -> (Channels, ExecutorEnds) {
    let (tx, rx) = HeapRb::<Box<Commit>>::new(QUEUE_CAPACITY).split();
    let (back_tx, back_rx) = HeapRb::<Box<Commit>>::new(QUEUE_CAPACITY + 1).split();
    let (fade_tx, fade_rx) = HeapRb::<Box<Crossfade>>::new(FADE_CAPACITY).split();
    let play = Arc::new(AtomicU64::new(0));
    let unreported = Arc::new(AtomicU64::new(0));
    let (loss_tx, loss_rx) = HeapRb::<EventLoss>::new(crate::loss::RING_CAPACITY).split();
    (
        Channels {
            to_executor: tx,
            returned: back_rx,
            faded: fade_rx,
            play: PlayReader(Arc::clone(&play)),
            losses: loss_rx,
            unreported: Arc::clone(&unreported),
        },
        ExecutorEnds {
            queue: rx,
            back: back_tx,
            fade_back: fade_tx,
            play,
            losses: loss_tx,
            unreported,
        },
    )
}

pub(crate) struct Unit {
    generation: u32,
    node: NodeBox,
    /// Consecutive frames of fully silent input, saturating.
    quiet: u64,
    /// Whether the last call (or skip) left every output silent and wrote no
    /// event.
    last_quiet: bool,
    /// Whether the last call returned [`Status::Idle`].
    last_idle: bool,
    /// The tail its last call narrowed its ceiling to
    /// ([`Io::report_tail`](crate::Io::report_tail), clamped to the
    /// ceiling); `None` is the ceiling. Written only by the task that runs
    /// the unit, so the skip it decides is a function of the unit's own
    /// inputs and reports.
    reported: Option<Tail>,
    /// The crossfade running here: `node` is its incoming unit.
    fade: Option<Box<Crossfade>>,
    /// A crossfade waiting for `fade` to end.
    queued: Option<Box<Crossfade>>,
    /// Its declared params' modulation state and per-frame buffers.
    params: ParamState,
    /// The keys its event outputs hold or owe (`held.rs`).
    keys: Keys,
    /// Its mode and the state that applies it (`mode.rs`).
    mode: ModeState,
}

impl Unit {
    fn new(generation: u32, node: NodeBox, params: ParamState, mode: ModeState) -> Self {
        Self {
            generation,
            node,
            quiet: 0,
            last_quiet: false,
            last_idle: false,
            reported: None,
            fade: None,
            queued: None,
            params,
            keys: Keys::default(),
            mode,
        }
    }
}

/// What a commit takes away from the keys held at a node: the tables of the
/// keys retired or replaced, and the keys replaced.
#[derive(Default)]
struct Gone {
    held: Vec<(NodeKey, Tables)>,
    /// Emptied tables to free on the control side.
    spare: Vec<Tables>,
    replaced: Vec<NodeKey>,
}

pub(crate) enum Ring {
    Audio(AudioRing),
    Event(EventFifo),
}

/// Events flushed from a delay that went away, waiting for their sink's next
/// call, at their offsets relative to the earliest (see the module docs).
pub(crate) struct Inject {
    pub(crate) unit: u32,
    port: u16,
    events: Vec<Event>,
    /// Where `events` and the sink's own slot events are merged, sized when
    /// applying so the merge never grows.
    merged: Vec<Event>,
    live: bool,
}

/// Everything the executor rebuilds when applying: the arenas, the delay and
/// feedback state, and pending injections. Retired into the commit box
/// whole, so the old one is freed on the control side.
struct State {
    arena: Arena,
    /// Per audio slot, what is known about its block: [`SILENT`] and
    /// [`CONSTANT`] bits. One byte per slot, so a node's input masks are
    /// built with a load and two shifts per channel. The parallel executor
    /// runs on a shared atomic copy (`par.rs`).
    flags: Vec<u8>,
    events: Vec<Vec<Event>>,
    rings: Vec<Option<Ring>>,
    audio_fb: Vec<Option<AudioRing>>,
    event_fb: Vec<Option<EventFifo>>,
    /// One producer per planned tap lane (`None`: nobody opened it).
    taps: Vec<Option<TapWriter>>,
    /// One producer per planned event tap (`None`: nobody opened it).
    event_taps: Vec<Option<EventTapWriter>>,
    /// One set of cells per planned meter (`None`: nobody opened it).
    meters: Vec<Option<MeterWriter>>,
    /// One state per entry of `Plan::sum_lanes`: a weighted lane's weight
    /// across blocks, or a plain addend's none.
    lanes: Vec<SumLane>,
    inject: Vec<Inject>,
    has_inject: Vec<bool>,
    /// Plan units with a scheduled command landing this block.
    has_due: Vec<bool>,
    /// Where a node's scheduled events are merged into its inputs; sized
    /// when applying so the merge never grows (`command::overlay_capacity`).
    overlay: Vec<Event>,
    /// The parallel executor's state for this plan, when the executor has a
    /// pool and the plan has tasks to spread (`par.rs`).
    par: Option<Par>,
}

impl State {
    fn empty(max_block: usize) -> Self {
        Self {
            arena: Arena::new(1, max_block),
            flags: vec![SILENT | CONSTANT],
            events: Vec::new(),
            rings: Vec::new(),
            audio_fb: Vec::new(),
            event_fb: Vec::new(),
            taps: Vec::new(),
            event_taps: Vec::new(),
            meters: Vec::new(),
            lanes: Vec::new(),
            inject: Vec::new(),
            has_inject: Vec::new(),
            has_due: Vec::new(),
            overlay: Vec::new(),
            par: None,
        }
    }
}

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
/// are silent when its last [`Status`] (and, for a node without event
/// inputs, its declared tail) says it has nothing more to produce.
pub struct Executor {
    prepare: Prepare,
    event_cap: usize,
    queue: HeapCons<Box<Commit>>,
    back: HeapProd<Box<Commit>>,
    /// Where crossfades go back (see the module docs, "Crossfades").
    fade_back: HeapProd<Box<Crossfade>>,
    plan: Option<Arc<Plan>>,
    store: Vec<Option<Unit>>,
    state: State,
    frame: Frame,
    /// Events dropped: the running total, and this block's log of where.
    dropped: Dropped,
    /// Where each block's log goes, for the editor to drain.
    losses: HeapProd<EventLoss>,
    unreported: Arc<AtomicU64>,
    commands: CommandRx,
    /// `seq` of the last commit applied.
    applied: u64,
    /// Between a re-prepare's two commits: every unit is checked out, and
    /// blocks render silence.
    suspended: bool,
    /// The `Prepare` a pending re-prepare will adopt when its resume commit
    /// lands. Not before: until then the device may still hand blocks sized
    /// for the old one (a shrinking `MaxBlock` must not fail them).
    next_prepare: Option<Prepare>,
    /// The held-key tables of the units a re-prepare checked out, for the
    /// resume commit to hand back.
    parked: Vec<(NodeKey, Keys)>,
    /// The sample rate changed in a re-prepare: the next rebuild carries no
    /// time-based state (see `Editor::reprepare`).
    reset_time: bool,
    /// The transport over the last frames, for nodes behind a latent path
    /// (`Cx::transport_at`). Fed every block, whatever it renders.
    history: TransportHistory,
    /// The graph's play clock (`src/play.rs`): kept here, handed to every
    /// node in its `Env`, published for the editor.
    play: PlayState,
    /// The pool blocks run on, if any (see [`set_pool`](Self::set_pool)).
    pool: Option<Arc<dyn Pool>>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("prepare", &self.prepare)
            .field("event_cap", &self.event_cap)
            .field("planned", &self.plan.is_some())
            .field("frame", &self.frame)
            .field("dropped", &self.dropped.total())
            .field("applied", &self.applied)
            .field("suspended", &self.suspended)
            .field("next_prepare", &self.next_prepare)
            .field("reset_time", &self.reset_time)
            .field("play", &self.play)
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
            queue: ends.queue,
            back: ends.back,
            fade_back: ends.fade_back,
            plan: None,
            store: Vec::new(),
            state: State::empty(prepare.max_block().get()),
            frame: Frame::ZERO,
            dropped: Dropped::new(),
            losses: ends.losses,
            unreported: ends.unreported,
            commands,
            applied: 0,
            suspended: false,
            next_prepare: None,
            parked: Vec::new(),
            reset_time: false,
            history: TransportHistory::new(),
            play: PlayState::new(ends.play),
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
        &self.commands
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
        self.next_prepare.as_ref()
    }

    /// The plan running now.
    pub fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// The graph's clock, which [`Env::frame`] reads: samples at the current
    /// rate since start. It tracks device time — blocks rendered as silence
    /// while a re-prepare has the units checked out count — and a rate change
    /// rescales it (and every pending frame-timed command) to the same
    /// wall-clock time at the new rate, rounded to the nearest frame.
    pub fn frame(&self) -> Frame {
        self.frame
    }

    /// The play clock (`Env::play`) at the next block's first frame, before
    /// a play start there: what the last block left it at. The control side
    /// reads the same through [`Editor::play_reading`](crate::Editor::play_reading).
    pub fn play_reading(&self) -> PlayReading {
        self.play.reading()
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
        self.commands.late()
    }

    /// Scheduled commands whose node or event port was gone by the time they
    /// fell due, so there was nowhere to deliver them.
    pub fn unrouted_command_count(&self) -> u64 {
        self.commands.unrouted()
    }

    /// Scheduled commands cancelled before they landed.
    pub fn cancelled_command_count(&self) -> u64 {
        self.commands.cancelled()
    }

    /// Applies every queued commit, in the order the editor sent them, and
    /// sends each box back. [`process`](Self::process) calls this first; it is
    /// public so a caller can install a commit without rendering.
    ///
    /// Delay rings, feedback state and units carry over by key (see
    /// [`DelayKey`] and [`FeedbackKey`]). An event delay or event feedback
    /// whose key disappears flushes its pending events to its sink when the
    /// sink survives: they arrive on the sink's next call, keeping their
    /// relative spacing, with the earliest at offset 0.
    ///
    /// # Panics
    ///
    /// If a plan was compiled for a different [`Prepare`] than this
    /// executor's (only possible through
    /// [`Editor::package`](crate::Editor::package)).
    pub fn apply_pending(&mut self) {
        let _rt = RtScope::enter();
        while let Some(mut commit) = self.queue.try_pop() {
            self.apply(&mut commit);
            send_back(&mut self.back, commit);
        }
    }

    fn apply(&mut self, c: &mut Commit) {
        self.applied = c.seq;
        if let Some(prepare) = c.suspend {
            // Check every unit out, into the box, for the editor to
            // re-prepare on the control thread. The plan and every piece of
            // delay and feedback state stay where they are, for the resume
            // commit's rebuild to carry — or, after a rate change, to reset.
            //
            // A re-prepare cuts every crossfade: what is checked out is the
            // newest unit at each key (a waiting one, if any), the one the
            // plan's generation names; the others go back with their fades.
            if let Some(plan) = &self.plan {
                for u in &plan.units {
                    let Some(mut unit) =
                        self.store.get_mut(u.idx.0 as usize).and_then(Option::take)
                    else {
                        continue;
                    };
                    // The unit a cut displaced still owes its keys: they close
                    // in the first block after the resume.
                    if let Some(stale) = cut_fades(&mut unit, &mut self.fade_back) {
                        c.spare_held.push(unit.keys.park(stale));
                    }
                    c.retired.push((u.key, unit.node));
                    c.spare_params.push(unit.params);
                    c.spare_modes.push(unit.mode);
                    self.parked.push((u.key, unit.keys));
                }
            }
            let (old, new) = (
                self.prepare.sample_rate().get(),
                prepare.sample_rate().get(),
            );
            if old != new {
                // `Frame` always means samples at the *current* rate since
                // start. Graph time tracks device time, so the clock and
                // every pending frame-timed command move to the same
                // wall-clock time at the new rate.
                self.reset_time = true;
                // Frames at the old rate name nothing at the new one.
                self.history.clear();
                let ratio = new / old;
                self.frame = crate::command::rescale(self.frame, ratio);
                self.play.rescale(ratio);
                self.commands.rescale(ratio, c.seq);
            }
            self.next_prepare = Some(prepare);
            self.suspended = true;
            return;
        }
        if let Some(p) = self.next_prepare.take() {
            // The resume commit of a re-prepare: its plan is for this.
            self.prepare = p;
        }
        let plan = c
            .plan
            .take()
            .expect("a commit carries its plan until applied");
        assert_eq!(
            plan.prepare, self.prepare,
            "a plan compiled for another Prepare: every unit is prepared for the executor's"
        );

        // A key retired or replaced leaves its held keys behind: the rebuild
        // below closes them on the edges they were held on. Not so a key
        // replaced with a fade while one runs: its unit keeps writing, and
        // the replacement waits (`end_fades` closes its keys then).
        let mut departed = Gone::default();
        let n_retire = c.delta.retire.len();
        for (i, p) in c
            .delta
            .retire
            .iter()
            .chain(c.delta.replace.iter().map(|(old, _)| old))
            .enumerate()
        {
            let waits = i >= n_retire
                && c.fades_in(c.delta.replace[i - n_retire].1.key)
                && self
                    .store
                    .get(p.idx.0 as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|u| u.fade.is_some());
            if waits {
                continue;
            }
            if let Some(u) = self
                .store
                .get_mut(p.idx.0 as usize)
                .and_then(Option::as_mut)
            {
                departed
                    .held
                    .push((p.key, u.keys.depart(&mut departed.spare)));
            }
        }
        departed.replaced = c.delta.replace.iter().map(|(_, new)| new.key).collect();
        // A retired key, or one replaced without a fade, goes whole: its
        // unit and any crossfade running or waiting there. A key replaced
        // with a fade stays, for the incoming unit to fade in over it.
        for i in 0..c.delta.retire.len() + c.delta.replace.len() {
            let p = match i.checked_sub(c.delta.retire.len()) {
                None => c.delta.retire[i],
                Some(j) => {
                    let (old, new) = c.delta.replace[j];
                    if c.fades_in(new.key) {
                        continue;
                    }
                    old
                }
            };
            if let Some(u) = self.store.get_mut(p.idx.0 as usize).and_then(Option::take) {
                debug_assert_eq!(
                    u.generation, p.generation,
                    "retiring the unit the delta named"
                );
                c.retired.push((p.key, u.node));
                c.spare_params.push(u.params);
                c.spare_modes.push(u.mode);
                for f in [u.fade, u.queued].into_iter().flatten() {
                    retire_fade(&mut self.fade_back, f);
                }
            }
        }
        // Cuts: a kept unit's crossfades go, its newest unit stays.
        for p in &c.delta.cuts {
            if let Some(u) = self
                .store
                .get_mut(p.idx.0 as usize)
                .and_then(Option::as_mut)
            {
                debug_assert_eq!(
                    u.generation, p.generation,
                    "cutting the unit the delta named"
                );
                if let Some(stale) = cut_fades(u, &mut self.fade_back) {
                    departed.held.push((p.key, stale));
                }
            }
        }
        if self.store.len() < c.delta.store_len as usize {
            self.store.resize_with(c.delta.store_len as usize, || None);
        }
        // Taken out and put back, so the list's buffer is not freed here.
        let mut incoming = std::mem::take(&mut c.incoming);
        for Incoming {
            idx,
            generation,
            node,
            fade,
            params,
            mode,
        } in incoming.drain(..)
        {
            let slot = &mut self.store[idx.0 as usize];
            match (fade, slot.as_mut()) {
                (Some(mut x), Some(u)) => {
                    // The key's param state and mode run on across the
                    // replace; the ones built for the incoming unit go back
                    // unused.
                    c.spare_params.push(params);
                    c.spare_modes.push(mode);
                    // A crossfade: the unit at this key fades out — or, with
                    // a fade already running here, this one waits for it,
                    // superseding one that was waiting.
                    u.generation = generation;
                    if u.fade.is_some() {
                        x.waiting = Some(node);
                        if let Some(q) = u.queued.replace(x) {
                            retire_fade(&mut self.fade_back, q);
                        }
                    } else {
                        x.out = Some(std::mem::replace(&mut u.node, node));
                        u.fade = Some(x);
                        u.quiet = 0;
                        u.last_quiet = false;
                        u.last_idle = false;
                        u.reported = None;
                    }
                }
                (fade, _) => {
                    debug_assert!(slot.is_none(), "store index {} is occupied", idx.0);
                    *slot = Some(Unit::new(generation, node, params, mode));
                    // Nothing to fade from (reachable only through
                    // `package`): the crossfade goes back unused.
                    if let Some(x) = fade {
                        retire_fade(&mut self.fade_back, x);
                    }
                }
            }
        }
        c.incoming = incoming;
        // A kept unit's bypass line, rebuilt for its new declared latency.
        let mut relined = std::mem::take(&mut c.relined);
        for Relined { idx, latency, line } in relined.drain(..) {
            if let Some(u) = self.store.get_mut(idx.0 as usize).and_then(Option::as_mut)
                && let Some(old) = u.mode.relined(latency, line)
            {
                c.spare_lines.push(old);
            }
        }
        c.relined = relined;

        let old_plan = self.plan.take();
        let new_state = self.rebuild(&plan, old_plan.as_deref(), &mut departed, &mut c.opened);
        c.spare_held.extend(departed.held.drain(..).map(|(_, k)| k));
        c.spare_held.append(&mut departed.spare);
        self.suspended = false;
        self.reset_time = false;
        let old_state = std::mem::replace(&mut self.state, new_state);
        c.old_state = Some(old_state);
        self.plan = Some(plan);
        c.plan = old_plan;
    }

    /// The state for `plan`, carrying what survives out of `self.state`
    /// (which keeps what does not, to be retired with it).
    fn rebuild(
        &mut self,
        plan: &Plan,
        old_plan: Option<&Plan>,
        departed: &mut Gone,
        opened: &mut Opened,
    ) -> State {
        let cap = self.event_cap;
        let max_block = self.prepare.max_block().get();
        // After a sample-rate change nothing time-based carries: every ring
        // and FIFO below starts fresh, and the old event FIFOs, left behind,
        // are flushed by the pass that flushes vanished delays (see
        // `Editor::reprepare` for the rule).
        let carry = !self.reset_time;
        let old = &mut self.state;

        // PDC rings and FIFOs, by key.
        let old_delays: BTreeMap<DelayKey, usize> = old_plan
            .map(|p| {
                p.delays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (d.key, i))
                    .collect()
            })
            .unwrap_or_default();
        // An event FIFO is sized from its source port's declared rate
        // (`Shape::event_capacity`), or the default for one that declared
        // none: the same figure the source's writer enforces.
        // The rate is what the port can hold, note-off room included: the
        // FIFO takes every event the port wrote.
        let rate = |from: EventOut| {
            crate::event::note_off_room(
                plan.event_port_capacity(from)
                    .map_or(cap, |n| n.get() as usize),
            )
        };
        let mut rings: Vec<Option<Ring>> = plan
            .delays
            .iter()
            .map(|d| {
                let carried = old_delays
                    .get(&d.key)
                    .filter(|_| carry)
                    .and_then(|&i| old.rings.get_mut(i))
                    .and_then(Option::take);
                Some(match (d.key, carried) {
                    (
                        DelayKey::Event { from, .. }
                        | DelayKey::ParamEvent { from, .. }
                        | DelayKey::EdgeGain { from, .. }
                        | DelayKey::EventTap { from, .. },
                        Some(Ring::Event(mut f)),
                    ) => {
                        f.retune(d.len);
                        f.resize(rate(from), max_block);
                        Ring::Event(f)
                    }
                    (
                        DelayKey::Event { from, .. }
                        | DelayKey::ParamEvent { from, .. }
                        | DelayKey::EdgeGain { from, .. }
                        | DelayKey::EventTap { from, .. },
                        _,
                    ) => Ring::Event(EventFifo::sized(d.len, rate(from), max_block)),
                    (_, Some(Ring::Audio(mut r))) => {
                        r.retune(d.len);
                        Ring::Audio(r)
                    }
                    (_, _) => Ring::Audio(AudioRing::new(d.len)),
                })
            })
            .collect();

        // Feedback delays, by key: exactly the edge's delay long.
        let old_fb = |kind: &[crate::plan::FeedbackSpec]| -> BTreeMap<FeedbackKey, usize> {
            kind.iter().enumerate().map(|(i, f)| (f.key, i)).collect()
        };
        let old_afb = old_plan
            .map(|p| old_fb(&p.audio_feedback))
            .unwrap_or_default();
        let old_efb = old_plan
            .map(|p| old_fb(&p.event_feedback))
            .unwrap_or_default();
        let audio_fb = plan
            .audio_feedback
            .iter()
            .map(|f| {
                let carried = old_afb
                    .get(&f.key)
                    .filter(|_| carry)
                    .and_then(|&i| old.audio_fb.get_mut(i))
                    .and_then(Option::take);
                Some(carried.unwrap_or_else(|| AudioRing::new(f.key.delay())))
            })
            .collect();
        let event_fb: Vec<Option<EventFifo>> = plan
            .event_feedback
            .iter()
            .map(|f| {
                let FeedbackKey::Event { from, .. } = f.key else {
                    unreachable!("event feedback keys are events")
                };
                let carried = old_efb
                    .get(&f.key)
                    .filter(|_| carry)
                    .and_then(|&i| old.event_fb.get_mut(i))
                    .and_then(Option::take)
                    .map(|mut fifo| {
                        fifo.resize(rate(from), max_block);
                        fifo
                    });
                Some(
                    carried
                        .unwrap_or_else(|| EventFifo::sized(f.key.delay(), rate(from), max_block)),
                )
            })
            .collect();

        // Taps, by id: a tap the old plan ran carries its writer; one opened
        // since the last commit arrives in the commit and wins over a carried
        // one of the same id (opening an id again replaces it). A writer the
        // new plan does not name stays in the old state (or the commit) and is
        // freed on the control side with it.
        let old_taps: BTreeMap<TapId, usize> = old_plan
            .map(|p| p.taps.iter().enumerate().map(|(i, k)| (*k, i)).collect())
            .unwrap_or_default();
        let taps: Vec<Option<TapWriter>> = plan
            .taps
            .iter()
            .map(|k| {
                opened
                    .taps
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| opened.taps.swap_remove(at).1)
                    .or_else(|| {
                        old_taps
                            .get(k)
                            .and_then(|&i| old.taps.get_mut(i))
                            .and_then(Option::take)
                    })
            })
            .collect();

        // Event taps, the same way. A tap whose port is missing has an entry
        // and no op, so its writer is carried across plans all the same.
        let old_event_taps: BTreeMap<EventTapId, usize> = old_plan
            .map(|p| {
                p.event_taps
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (*k, i))
                    .collect()
            })
            .unwrap_or_default();
        let event_taps: Vec<Option<EventTapWriter>> = plan
            .event_taps
            .iter()
            .map(|k| {
                opened
                    .event_taps
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| opened.event_taps.swap_remove(at).1)
                    .or_else(|| {
                        old_event_taps
                            .get(k)
                            .and_then(|&i| old.event_taps.get_mut(i))
                            .and_then(Option::take)
                    })
            })
            .collect();

        // Meters, the same way: carried by id, replaced by one opened since,
        // and the width each now measures told to its cells.
        let old_meters: BTreeMap<MeterId, usize> = old_plan
            .map(|p| {
                p.meters
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (*k, i))
                    .collect()
            })
            .unwrap_or_default();
        // Weighted lanes, by edge: what a lane holds (the weight it was last
        // given, and a driven lane's param state) carries across a commit
        // that keeps the edge (not a sample-rate change's reset: a weight is
        // not time), and a new edge starts at its value.
        let mut old_lanes: BTreeMap<EdgeId, usize> = old_plan
            .map(|p| {
                p.sum_lanes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, id)| Some(((*id)?, i)))
                    .collect()
            })
            .unwrap_or_default();
        // The lanes a source drives in this plan: each driven lane's state
        // is sized here, with the plan, so the audio thread allocates none.
        let mut driven = vec![false; plan.sum_lanes.len()];
        for op in &plan.ops {
            if let Op::WeightedSum { lanes, gains, .. } = *op {
                for g in &plan.gain_ports[gains.range()] {
                    driven[lanes.start as usize + usize::from(g.lane)] = true;
                }
            }
        }
        let lanes: Vec<SumLane> = plan
            .sum_lanes
            .iter()
            .zip(&driven)
            .map(|(id, &driven)| match id {
                None => SumLane::plain(),
                Some(id) => SumLane::weighted(
                    opened
                        .gains
                        .get(id)
                        .cloned()
                        .expect("the editor sends a cell for every weighted lane"),
                    old_lanes
                        .remove(id)
                        .and_then(|i| old.lanes.get_mut(i))
                        .map(SumLane::carry)
                        .unwrap_or_default(),
                    driven,
                    max_block,
                ),
            })
            .collect();
        let meters: Vec<Option<MeterWriter>> = plan
            .meters
            .iter()
            .map(|(k, lanes)| {
                let writer = opened
                    .meters
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| opened.meters.swap_remove(at).1)
                    .or_else(|| {
                        old_meters
                            .get(k)
                            .and_then(|&i| old.meters.get_mut(i))
                            .and_then(Option::take)
                    });
                if let Some(w) = &writer {
                    w.set_width(*lanes);
                }
                writer
            })
            .collect();

        // Flush what disappeared, to sinks that survive. Order: injections
        // not yet delivered, then event delays in key order, then event
        // feedback in key order — the reference interpreter's order too.
        let sink_of = |at: EventIn| -> Option<u32> {
            let u = plan.units.binary_search_by_key(&at.node, |u| u.key).ok()?;
            (at.port < plan.units[u].shape.event_in).then_some(u as u32)
        };
        let mut flushed: BTreeMap<(u32, u16), Vec<Event>> = BTreeMap::new();
        let mut take_into = |at: EventIn, events: Vec<Event>| {
            if let Some(u) = sink_of(at) {
                flushed.entry((u, at.port)).or_default().extend(events);
            }
        };
        if let Some(op) = old_plan {
            for inj in old.inject.iter().filter(|i| i.live) {
                let at = EventIn {
                    node: op.units[inj.unit as usize].key,
                    port: inj.port,
                };
                take_into(at, inj.events.clone());
            }
            let mut gone: Vec<(DelayKey, usize)> = old_delays
                .iter()
                .filter(|(k, _)| matches!(k, DelayKey::Event { .. }))
                .map(|(&k, &i)| (k, i))
                .collect();
            gone.sort();
            for (k, i) in gone {
                if let (DelayKey::Event { at, .. }, Some(Some(Ring::Event(f)))) =
                    (k, old.rings.get(i))
                {
                    take_into(at, f.flushed());
                }
            }
            for (k, &i) in &old_efb {
                if let (FeedbackKey::Event { at, .. }, Some(Some(f))) = (k, old.event_fb.get(i)) {
                    take_into(*at, f.flushed());
                }
            }
        }
        // A re-prepare's tables go back to the units that come back; the
        // others are as retired.
        for (key, mut keys) in std::mem::take(&mut self.parked) {
            let same = |a: Option<&Plan>| a.and_then(|p| p.unit(key)).map(|u| u.generation);
            let back = (same(old_plan) == same(Some(plan)))
                .then(|| plan.units.binary_search_by_key(&key, |u| u.key).ok())
                .flatten()
                .and_then(|i| self.store[plan.units[i].idx.0 as usize].as_mut());
            match back {
                Some(u) => {
                    keys.settle();
                    u.keys = keys;
                }
                None => departed.held.push((key, keys.depart(&mut departed.spare))),
            }
        }
        // Keys held on an edge that ended go to the sink that lost it, after
        // everything flushed to that sink (the offs of notes whose ons were in
        // flight). See `held.rs`.
        if let Some(op) = old_plan {
            let source_gone = |n: NodeKey| departed.held.iter().any(|(k, _)| *k == n);
            // What a port holds, and what it owes and has not yet sent.
            let held_of = |from: EventOut| -> (Option<&Held>, Option<&Held>) {
                let port = usize::from(from.port);
                if let Some((_, tables)) = departed.held.iter().find(|(k, _)| *k == from.node) {
                    return (tables.get(port).and_then(Option::as_ref), None);
                }
                let unit = plan
                    .units
                    .binary_search_by_key(&from.node, |u| u.key)
                    .ok()
                    .and_then(|i| self.store[plan.units[i].idx.0 as usize].as_ref());
                match unit {
                    Some(u) => (
                        u.keys.held().get(port).and_then(Option::as_ref),
                        u.keys.owed(from.port),
                    ),
                    None => (None, None),
                }
            };
            let mut closing: BTreeMap<(u32, u16), Vec<Event>> = BTreeMap::new();
            for e in &op.event_edges {
                let &(from, at) = e;
                let in_plan = plan.event_edges.binary_search(e).is_ok();
                if (in_plan && !source_gone(from.node)) || departed.replaced.contains(&at.node) {
                    continue;
                }
                let Some(u) = sink_of(at) else {
                    continue;
                };
                let (held, owed) = held_of(from);
                let owing = [held, owed]
                    .into_iter()
                    .flatten()
                    .filter(|h| !h.is_empty())
                    .collect::<Vec<_>>();
                if owing.is_empty() {
                    continue;
                }
                // A replaced source whose edge (and its delay) goes on: the
                // close travels the delay, behind what is in flight.
                let delay = plan
                    .delays
                    .iter()
                    .position(|d| d.key == DelayKey::Event { at, from });
                if in_plan && let Some(Some(Ring::Event(f))) = delay.map(|i| &mut rings[i]) {
                    let mut offs = Vec::new();
                    owing.iter().for_each(|h| h.close_into(&mut offs));
                    let lost = f.push(&offs);
                    self.dropped.charge_source(from, lost);
                } else {
                    let offs = closing.entry((u, at.port)).or_default();
                    owing.iter().for_each(|h| h.close_into(offs));
                }
            }
            for (k, mut offs) in closing {
                let end = flushed
                    .get(&k)
                    .and_then(|v| v.iter().map(|e| e.offset).max())
                    .unwrap_or(crate::time::Offset::ZERO);
                for e in &mut offs {
                    e.offset = end;
                }
                flushed.entry(k).or_default().extend(offs);
            }
        }
        // A port that carries notes is tracked from its unit's first block, so
        // an edge added later knows what the port already holds.
        for pu in plan.units.iter().filter(|u| u.shape.event_out > 0) {
            if let Some(u) = self.store[pu.idx.0 as usize].as_mut() {
                let waiting = u.queued.is_some();
                u.keys
                    .size(pu.shape.event_out as usize, pu.shape.note_events, waiting);
            }
        }

        // A FIFO carried across a commit that lowered its bound (a source
        // now declaring a smaller rate, a shorter delay, a larger
        // `MaxBlock`) can hold more than the bound its output slot was
        // priced at. The compiler is pure and cannot know what is queued, so
        // the slot is widened here, by that excess (`EventFifo::excess`),
        // where the state is built — and so is every merge downstream of
        // it, which the compiler priced at the sum of its inputs, and the
        // buffers a sink merges its inputs into (a flush's `merged`, the
        // command overlay, below): the backlog the FIFO holds can still go
        // out in one block, on its frames, and pass everything after it.
        // (One corner stays late, as it was without an excess: a delay
        // retuned below one block makes its whole backlog overdue at offset
        // 0; if that fills the slot, new input falling due in the same
        // block waits for the next.) Here and only here, on a commit that
        // already allocates this whole state; the queue never grows past it
        // (a new non-note-off is refused while it is full), so rendering
        // stays allocation-free. The next rebuild prices it afresh.
        //
        // Per slot, not per value: a slot the colouring pass shares takes
        // the largest excess of anything it holds, which only over-sizes.
        let mut extra = vec![0usize; plan.event_slot_capacity.len()];
        for (f, spec) in event_fb.iter().zip(&plan.event_feedback) {
            if let Some(f) = f {
                let x = &mut extra[spec.slot as usize];
                *x = (*x).max(f.excess());
            }
        }
        // Ops run in topological order, so a merge comes after whatever
        // feeds it.
        for op in &plan.ops {
            match *op {
                Op::EventDelay { delay, dst, .. } => {
                    if let Some(Ring::Event(f)) = &rings[delay as usize] {
                        let x = &mut extra[dst as usize];
                        *x = (*x).max(f.excess());
                    }
                }
                Op::EventMerge { srcs, dst } => {
                    let sum = plan.event_list[srcs.range()]
                        .iter()
                        .map(|&s| extra[s as usize])
                        .sum::<usize>();
                    let x = &mut extra[dst as usize];
                    *x = (*x).max(sum);
                }
                _ => {}
            }
        }
        let extra_total: usize = extra.iter().sum();
        let slot_capacity = |i: usize| plan.event_slot_capacity[i].events(cap) + extra[i];

        let mut has_inject = vec![false; plan.units.len()];
        let flushed_total: usize = flushed.values().map(Vec::len).sum();
        let widest = (0..plan.event_slot_capacity.len())
            .map(slot_capacity)
            .max()
            .unwrap_or(0);
        let inject: Vec<Inject> = flushed
            .into_iter()
            .map(|((unit, port), mut events)| {
                has_inject[unit as usize] = true;
                // Several flushes into one sink: one sorted list, ties in
                // flush order (stable).
                events.sort_by_key(|e| e.offset);
                let merged = Vec::with_capacity(events.len() + widest);
                Inject {
                    unit,
                    port,
                    events,
                    merged,
                    live: true,
                }
            })
            .collect();

        // The widened slots, all of them: an over-estimate of what they add
        // to the widest node's inputs. The parallel workers' overlays take
        // the same figure.
        let overlay = overlay_capacity(plan, cap, flushed_total + extra_total);
        let par = par_for(plan, self.pool.as_ref(), self.store.len(), &inject, overlay);
        State {
            arena: Arena::new(plan.audio_slots as usize, max_block),
            flags: {
                let mut v = vec![0u8; plan.audio_slots as usize];
                v[0] = SILENT | CONSTANT;
                v
            },
            events: (0..plan.event_slot_capacity.len())
                .map(|i| Vec::with_capacity(slot_capacity(i)))
                .collect(),
            rings,
            audio_fb,
            event_fb,
            taps,
            event_taps,
            meters,
            lanes,
            inject,
            has_inject,
            has_due: vec![false; plan.units.len()],
            overlay: Vec::with_capacity(overlay),
            par,
        }
    }

    /// Renders one block of `frames` into `outputs`, reading `inputs`.
    ///
    /// `inputs` must have a slice per global input channel the plan reads, and
    /// `outputs` one per global output channel; every slice at least `frames`
    /// long. Applies queued commits first ([`apply_pending`](Self::apply_pending));
    /// with none queued it never allocates. A caller that sizes the block from
    /// the executor's plan or prepare applies once itself and renders with
    /// [`render_applied`](Self::render_applied). Marks the thread with
    /// [`RtScope::enter`] for the duration.
    ///
    /// # Panics
    ///
    /// If `frames` is zero or exceeds the prepared maximum block.
    pub fn process(
        &mut self,
        frames: usize,
        transport: &Transport,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        self.process_with_changes(frames, transport, &TransportChanges::NONE, inputs, outputs);
    }

    /// As [`process`](Self::process), with the transport changing inside the
    /// block: `transport` holds at the first frame, and each of `changes`
    /// from its offset on (a transport command lands on its frame). The block
    /// is **not** split at them. Every node gets the whole block and an
    /// [`Env`] carrying the changes, and scheduled `At::Beat`
    /// commands resolve against the transport in force where playback
    /// reaches their beat.
    ///
    /// # Panics
    ///
    /// As [`process`](Self::process), and if a change is not inside the
    /// block (outside a re-prepare, when any block length is accepted).
    pub fn process_with_changes(
        &mut self,
        frames: usize,
        transport: &Transport,
        changes: &TransportChanges,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        with_no_denormals(|| {
            self.apply_pending();
            self.render_applied(frames, transport, changes, inputs, outputs);
        });
    }

    /// As [`process_with_changes`](Self::process_with_changes), without
    /// applying queued commits: the block renders the plan, prepare and rate
    /// the executor holds now, and a commit sent later waits for the next
    /// block.
    ///
    /// For a caller that reads [`plan`](Self::plan) or
    /// [`prepare`](Self::prepare) to size the block (its length, `outputs`)
    /// and must have them still true when it renders: it calls
    /// [`apply_pending`](Self::apply_pending) once, sizes from the result and
    /// renders here. [`process`](Self::process) would apply again, and a
    /// commit sent in between (a wider plan, a smaller `MaxBlock` from a
    /// re-prepare's resume) would change what the sizes were read from.
    ///
    /// `inputs` and `outputs` must have one slice per global input and output
    /// channel of the plan. Slices past those are ignored, serial or on a
    /// pool. Fewer of either is a caller bug: it panics in debug builds, and
    /// in release the block renders silence into what was given, running no
    /// node.
    ///
    /// # Panics
    ///
    /// As [`process_with_changes`](Self::process_with_changes).
    pub fn render_applied(
        &mut self,
        frames: usize,
        transport: &Transport,
        changes: &TransportChanges,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        let _rt = RtScope::enter();
        // `move` captures `self` as one pointer; the precise capture of every
        // field the body names is a large struct built per block.
        with_no_denormals(move || {
            if self.suspended {
                // Between a re-prepare's two commits the units are on the control
                // thread: render silence, for whatever block arrives — the device
                // may be on either side of the change, so no bound applies here.
                // The graph's own state (rings, FIFOs) stands still, but its
                // clock tracks device time: the frame counter advances, and the
                // playhead sees the transport move. A command whose time passes
                // meanwhile lands late when the units are back.
                for o in outputs.iter_mut() {
                    o[..frames].fill(0.0);
                }
                let rate = self.next_prepare.unwrap_or(self.prepare).sample_rate();
                let mut env = Env {
                    frame: self.frame,
                    sample_rate: rate,
                    block_len: Samples(frames),
                    transport: *transport,
                    changes: *changes,
                    play: PlayClock::ZERO,
                };
                env.play = self.play.begin(transport.playing, env.play_starts());
                self.commands.observe(&env);
                self.history.observe(&env);
                self.play.end(&env);
                self.frame += Samples(frames);
                return;
            }
            let max = self.prepare.max_block();
            assert!(
                frames > 0 && frames <= max.get(),
                "block of {frames} frames against a max of {}",
                max.get()
            );
            assert!(
                changes.as_slice().iter().all(|c| c.at.index() < frames),
                "a transport change past the end of a {frames}-frame block"
            );
            let Self {
                prepare,
                event_cap,
                queue: _,
                back: _,
                fade_back,
                plan,
                store,
                state,
                frame,
                dropped,
                losses,
                unreported,
                commands,
                applied,
                suspended: _,
                next_prepare: _,
                parked: _,
                reset_time: _,
                history,
                play,
                pool,
            } = self;
            let mut env = Env {
                frame: *frame,
                sample_rate: prepare.sample_rate(),
                block_len: Samples(frames),
                transport: *transport,
                changes: *changes,
                play: PlayClock::ZERO,
            };
            env.play = play.begin(transport.playing, env.play_starts());
            // Before any node reads it, and also when there is nothing to render:
            // the transport moved all the same, and so did the play clock.
            history.observe(&env);
            play.end(&env);
            let history: &TransportHistory = history;
            let Some(plan) = plan.as_ref() else {
                for o in outputs.iter_mut() {
                    o[..frames].fill(0.0);
                }
                *frame += Samples(frames);
                return;
            };
            if outputs.len() < plan.global_outputs()
                || inputs.len() < usize::from(plan.global_inputs())
            {
                debug_assert!(
                    false,
                    "{} input and {} output slices for a plan with {} global inputs and {} outputs",
                    inputs.len(),
                    outputs.len(),
                    plan.global_inputs(),
                    plan.global_outputs()
                );
                for o in outputs.iter_mut() {
                    o[..frames].fill(0.0);
                }
                commands.observe(&env);
                *frame += Samples(frames);
                return;
            }
            let State {
                arena,
                flags,
                events,
                rings,
                audio_fb,
                event_fb,
                taps,
                event_taps,
                meters,
                lanes,
                inject,
                has_inject,
                has_due,
                overlay,
                par,
            } = state;
            // Slices, not `&mut Vec`s: a slice's pointer and length are locals
            // the op loop can keep in registers across the opaque node calls,
            // where a `Vec` behind a reference is reloaded after every one.
            let (flags, events, inject, has_inject, has_due) = (
                &mut flags[..],
                &mut events[..],
                &mut inject[..],
                &mut has_inject[..],
                &mut has_due[..],
            );
            let plan: &Plan = plan;
            let cap = *event_cap;
            commands.gather(&env, plan, *applied, has_due);
            let mut fade_ended = false;

            // Every weighted lane reads its cell once, here: a store while
            // the block runs reaches no op of it, so the serial and the
            // parallel render cannot disagree on when it landed.
            for lane in lanes.iter_mut() {
                lane.snapshot();
            }
            // And every unit its mode (`mode.rs`), for the same reason.
            for rec in &plan.nodes.recs {
                if let Some(u) = store[rec.store as usize].as_mut() {
                    u.mode.snapshot();
                }
            }

            // Feedback reads happen before any op: the delay is a whole
            // `MaxBlock`, so nothing this block's captures queue is due yet.
            for (f, spec) in audio_fb.iter().zip(&plan.audio_feedback) {
                let ring = f.as_ref().expect("built by apply");
                let is_silent = ring.peek_oldest(arena.slot_mut(spec.slot, frames));
                flags.set(spec.slot, flag(is_silent, is_silent));
            }
            for (f, spec) in event_fb.iter_mut().zip(&plan.event_feedback) {
                let fifo = f.as_mut().expect("built by apply");
                let out = &mut events[spec.slot as usize];
                out.clear();
                fifo.pop_due(out, frames);
            }

            let due = commands.deliveries();
            // The cost model's verdict for this plan on this pool, and for a
            // block this long (`Plan::schedule`); either way the same samples.
            let spread = par.as_ref().is_some_and(|p| p.spreads(frames));
            if let (true, Some(par), Some(pool)) = (spread, par.as_mut(), pool.as_ref()) {
                let ended = crate::par::run_block(
                    par,
                    pool,
                    Block {
                        dropped: &mut *dropped,
                        plan,
                        env: &env,
                        history,
                        frames,
                        max,
                        cap,
                        inputs,
                        // The claim table is sized for the plan's width; slices
                        // past it are ignored here as on the serial path.
                        outputs: &mut outputs[..plan.global_outputs()],
                        arena,
                        flags,
                        events,
                        store,
                        rings,
                        audio_fb,
                        event_fb,
                        taps,
                        event_taps,
                        meters: &*meters,
                        lanes: &mut lanes[..],
                        inject,
                        has_inject,
                        has_due,
                        due,
                    },
                );
                fade_ended |= ended;
            } else {
                // Built once per block, not per op: rebuilt per node call it was
                // ~20 stack moves a node (measured on `graph_render`'s
                // `overhead` chains). A node op sets `at`.
                let mut st = OpState {
                    arena,
                    flags,
                    events,
                    inject,
                    has_inject,
                    has_due,
                    at: 0,
                    overlay,
                    due,
                    dropped: &mut *dropped,
                };
                for op in &plan.ops {
                    match *op {
                        Op::GlobalIn { channel, dst } => {
                            global_in(&mut *st.arena, &mut *st.flags, inputs, channel, dst, frames);
                        }
                        Op::Delay { delay, src, dst } => {
                            delay_op(
                                audio_ring(&mut rings[delay as usize]),
                                &mut *st.arena,
                                &mut *st.flags,
                                src,
                                dst,
                                frames,
                            );
                        }
                        Op::Sum { srcs, dst } => {
                            sum_op(
                                &mut *st.arena,
                                &mut *st.flags,
                                &*st.events,
                                &plan.audio_list[srcs.range()],
                                &mut [],
                                &[],
                                dst,
                                frames,
                            );
                        }
                        Op::WeightedSum {
                            srcs,
                            dst,
                            lanes: span,
                            gains,
                        } => {
                            sum_op(
                                &mut *st.arena,
                                &mut *st.flags,
                                &*st.events,
                                &plan.audio_list[srcs.range()],
                                &mut lanes[span.range()],
                                &plan.gain_ports[gains.range()],
                                dst,
                                frames,
                            );
                        }
                        Op::EventDelay { delay, src, dst } => {
                            let lost = event_delay_op(
                                event_ring(&mut rings[delay as usize]),
                                &mut *st.events,
                                src,
                                dst,
                                frames,
                            );
                            st.dropped.charge_delay(plan, delay, lost);
                        }
                        Op::EventMerge { srcs, dst } => {
                            let lost = event_merge_op(
                                &plan.event_list[srcs.range()],
                                dst,
                                &mut *st.events,
                            );
                            st.dropped.count(lost);
                        }
                        Op::Node { unit, .. } => {
                            // One record per node op, lowered at compile time and
                            // checked by the verifier: store index, generation,
                            // arrival, tail, port slots and presorted borrows.
                            let rec = &plan.nodes.recs[unit as usize];
                            let u = store[rec.store as usize]
                                .as_mut()
                                .expect("the delta placed every unit the plan runs");
                            let head = Head {
                                rec,
                                plan,
                                frames,
                                max,
                                cap,
                                env: &env,
                                history,
                            };
                            st.at = unit as usize;
                            fade_ended |= node_dispatch(u, &head, &mut st);
                        }
                        Op::Output {
                            channel,
                            src,
                            delay,
                        } => {
                            let ring = delay.map(|d| audio_ring(&mut rings[d as usize]));
                            output_op(
                                ring,
                                &mut outputs[channel as usize][..frames],
                                &*st.arena,
                                &*st.flags,
                                src,
                            );
                        }
                        Op::Tap { tap, srcs } => {
                            if let Some(tap) = taps[tap as usize].as_mut() {
                                crate::tap::tap_op(
                                    tap,
                                    &*st.arena,
                                    &plan.audio_list[srcs.range()],
                                    frames,
                                );
                            }
                        }
                        Op::EventTap { tap, src } => {
                            if let Some(tap) = event_taps[tap as usize].as_mut() {
                                crate::event_tap::event_tap_op(tap, &*st.events, src, &env);
                            }
                        }
                        Op::Meter { meter, srcs } => {
                            if let Some(meter) = meters[meter as usize].as_ref() {
                                meter_op(
                                    meter,
                                    &*st.arena,
                                    &*st.flags,
                                    &plan.audio_list[srcs.range()],
                                    frames,
                                );
                            }
                        }
                        Op::Capture { feedback, src } => {
                            let ring = audio_fb[feedback as usize]
                                .as_mut()
                                .expect("built by apply");
                            capture_op(ring, &*st.arena, &*st.flags, src, frames);
                        }
                        Op::EventCapture { feedback, src } => {
                            let fifo = event_fb[feedback as usize]
                                .as_mut()
                                .expect("built by apply");
                            let lost = fifo.push(st.events.ev(src));
                            st.dropped.charge_feedback(plan, feedback, u64::from(lost));
                        }
                    }
                }
            }

            dropped.flush(plan, losses, unreported);
            for f in event_fb.iter_mut().flatten() {
                f.advance(frames);
            }
            if fade_ended {
                end_fades(plan, store, fade_back);
            }
            *frame += Samples(frames);
        });
    }
}

/// After a block in which a crossfade reached its end: retire each ended
/// fade's outgoing unit into the commit that started it, and start the fade
/// waiting behind it, if any — from the next block on.
#[cold]
#[inline(never)]
fn end_fades(plan: &Plan, store: &mut [Option<Unit>], fade_back: &mut HeapProd<Box<Crossfade>>) {
    for rec in &plan.nodes.recs {
        let Some(u) = store[rec.store as usize].as_mut() else {
            continue;
        };
        if !u.fade.as_ref().is_some_and(|x| x.done >= x.len) {
            continue;
        }
        let ended = u.fade.take().expect("checked");
        retire_fade(fade_back, ended);
        if let Some(mut next) = u.queued.take() {
            let incoming = next.waiting.take().expect("a queued fade holds its unit");
            next.out = Some(std::mem::replace(&mut u.node, incoming));
            u.keys.promote();
            u.fade = Some(next);
            u.quiet = 0;
            u.last_quiet = false;
            u.last_idle = false;
            u.reported = None;
        }
    }
}

/// An audio delay's ring.
#[inline]
pub(crate) fn audio_ring(r: &mut Option<Ring>) -> &mut AudioRing {
    match r {
        Some(Ring::Audio(a)) => a,
        _ => unreachable!("an audio delay on an event ring"),
    }
}

/// An event delay's FIFO.
#[inline]
pub(crate) fn event_ring(r: &mut Option<Ring>) -> &mut EventFifo {
    match r {
        Some(Ring::Event(f)) => f,
        _ => unreachable!("an event delay on an audio ring"),
    }
}

/// [`Op::GlobalIn`].
#[inline]
pub(crate) fn global_in<A: AudioSlots, F: SlotFlags + ?Sized>(
    arena: &mut A,
    flags: &mut F,
    inputs: &[&[f32]],
    channel: u16,
    dst: u32,
    frames: usize,
) {
    arena
        .slot_mut(dst, frames)
        .copy_from_slice(&inputs[channel as usize][..frames]);
    flags.set(dst, 0);
}

/// [`Op::Delay`]; `src == dst` is the in-place form.
#[inline]
pub(crate) fn delay_op<A: AudioSlots, F: SlotFlags + ?Sized>(
    ring: &mut AudioRing,
    arena: &mut A,
    flags: &mut F,
    src: u32,
    dst: u32,
    frames: usize,
) {
    let src_silent = flags.get(src) & SILENT != 0;
    let out_silent = if src == dst {
        ring.run_in_place(arena.slot_mut(dst, frames), src_silent)
    } else {
        let (s, d) = arena.pair(src, dst, frames);
        ring.run(s, d, src_silent)
    };
    flags.set(dst, flag(out_silent, out_silent));
}

/// [`Op::Sum`] or [`Op::WeightedSum`] of the slots `list` into `dst`: the left
/// fold `((+0.0 + w_0 * a_0) + w_1 * a_1) + ..` in `f32`, one rounding per
/// multiply and per add, never fused (doc 013, "Audio fan-in sums", points 1,
/// 2 and 4, and "Edge gain"). `lanes` is empty for a plain sum, else one per
/// addend; an addend's weight is exactly `1.0` unless its lane says otherwise,
/// and a `1.0` weight is the plain add, no multiply (`crate::gain`).
///
/// An addend flagged [`SILENT`] is skipped whatever its weight: the
/// accumulator starts at `+0.0` and so is never `-0.0`, and `acc + w * (+0.0)`
/// is `acc` bit for bit (a finite `w`; every base is finite). A weight of
/// `0.0` is not skipped by value: `0.0 * NaN` is `NaN`, and the render must
/// not depend on whether a gain happens to be zero. Every lane's state
/// advances **every** block, skipped addend or not. When no addend is live
/// the output is zero-filled and flagged silent; otherwise it is constant when
/// every live addend is **and none is ramping** (a constant times a ramp is
/// not constant). The flags are a claim made from the addends' flags, never
/// from scanning samples. The loop is source-major (it vectorises); each
/// sample still sees the same sequence of adds as the frame-major fold.
///
/// `dst` is the first addend's own slot in the in-place form. That addend is
/// then not borrowed for reading: it is weighted in place (`dst = +0.0 + w *
/// dst`, which turns a `-0.0` into `+0.0`, as `+0.0 + w * a_0` does) and the
/// rest are added into it.
#[inline]
pub(crate) fn sum_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    arena: &mut A,
    flags: &mut F,
    events: &E,
    list: &[u32],
    lanes: &mut [SumLane],
    gains: &[GainPortOp],
    dst: u32,
    frames: usize,
) {
    debug_assert!(!list.is_empty() && list.len() <= MAX_PORTS);
    debug_assert!(lanes.is_empty() || lanes.len() == list.len());
    // Moved on first, for every lane: a silent addend's weight moves too. A
    // driven lane runs the param port's step on its source's events here.
    let mut weights = [Weight::Unit; MAX_PORTS];
    for (k, (w, lane)) in weights.iter_mut().zip(lanes.iter_mut()).enumerate() {
        let source = gains
            .iter()
            .find(|g| usize::from(g.lane) == k)
            .map(|g| (g.from, &events.ev(g.slot)[..]));
        *w = SumLane::weight(lane, frames, source);
    }
    // Requests, then sorted by slot; the order of addition is the `Read`
    // index, among the live addends.
    let mut reqs = [(0u32, Role::Read(0)); MAX_PORTS + 1];
    let mut live_weights = [Weight::Unit; MAX_PORTS];
    let (mut live, mut m) = (0usize, 0usize);
    let (mut in_place, mut constant) = (false, true);
    for (k, &s) in list.iter().enumerate() {
        let f = flags.get(s);
        if f & SILENT != 0 {
            continue;
        }
        constant &=
            f & CONSTANT != 0 && !matches!(weights[k], Weight::Ramp { .. } | Weight::Frames(_));
        if live == 0 && s == dst {
            in_place = true;
        } else {
            reqs[m] = (s, Role::Read(live as u8));
            m += 1;
        }
        live_weights[live] = weights[k];
        live += 1;
    }
    if live == 0 {
        arena.slot_mut(dst, frames).fill(0.0);
        flags.set(dst, SILENT | CONSTANT);
        return;
    }
    reqs[m] = (dst, Role::Write(0));
    m += 1;
    reqs[..m].sort_unstable();
    let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut outs: [&mut [f32]; 1] = [&mut [][..]];
    arena.borrow(frames, &reqs[..m], &mut ins, &mut outs);
    let [out] = outs;
    // `ins[k]` is live addend `k`, but for the first when it is `dst` itself.
    gain::first(out, (!in_place).then(|| ins[0]), live_weights[0]);
    for (a, &w) in ins[1..live].iter().zip(&live_weights[1..live]) {
        gain::add(out, a, w);
    }
    flags.set(dst, flag(false, constant));
}

/// [`Op::EventDelay`]. Returns the events its FIFO refused.
#[inline]
pub(crate) fn event_delay_op<E: EventSlots + ?Sized>(
    fifo: &mut EventFifo,
    events: &mut E,
    src: u32,
    dst: u32,
    frames: usize,
) -> u64 {
    let (input, output) = event_pair(events, src, dst);
    output.clear();
    fifo.run(input, output, frames) as u64
}

/// [`Op::EventMerge`] of the slots `list` into `dst`. Returns the events it
/// dropped, which is none: the slot holds all its inputs
/// (`event_slot_capacity`). Counted anyway, in case it ever does.
pub(crate) fn event_merge_op<E: EventSlots + ?Sized>(
    list: &[u32],
    dst: u32,
    events: &mut E,
) -> u64 {
    let mut reqs = [(0u32, Role::Read(0)); MAX_PORTS + 1];
    for (i, &s) in list.iter().enumerate() {
        reqs[i] = (s, Role::Read(i as u8));
    }
    reqs[list.len()] = (dst, Role::Write(0));
    let mut ins: [&[Event]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut output: Option<&mut Vec<Event>> = None;
    borrow_events(
        events,
        &mut reqs[..=list.len()],
        |port, v| ins[port as usize] = v,
        |_, v| output = Some(v),
    );
    let output = output.expect("dst borrowed");
    output.clear();
    let room = output.capacity();
    merge_into(&ins[..list.len()], output, room) as u64
}

/// [`Op::Output`]: slot `src` into `out`, through the channel's alignment
/// ring if it has one.
#[inline]
pub(crate) fn output_op<A: AudioSlots, F: SlotFlags + ?Sized>(
    ring: Option<&mut AudioRing>,
    out: &mut [f32],
    arena: &A,
    flags: &F,
    src: u32,
) {
    let s = arena.slot(src, out.len());
    match ring {
        Some(ring) => {
            ring.run(s, out, flags.get(src) & SILENT != 0);
        }
        None => out.copy_from_slice(s),
    }
}

/// [`Op::Capture`].
#[inline]
pub(crate) fn capture_op<A: AudioSlots, F: SlotFlags + ?Sized>(
    ring: &mut AudioRing,
    arena: &A,
    flags: &F,
    src: u32,
    frames: usize,
) {
    ring.push(arena.slot(src, frames), flags.get(src) & SILENT != 0);
}

/// [`Op::Node`]: run `u` for `h.rec`: the plain op for a unit whose mode asks
/// nothing of it, else the one that applies its mode. Returns whether a
/// crossfade running there reached its end this block.
#[inline(always)]
pub(crate) fn node_dispatch<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
) -> bool {
    if u.mode.is_plain() {
        return run_node_op(u, head, st);
    }
    mode_op::moded_node_op(u, head, st)
}

/// The node op by its borrow form (a crossfade running there takes its own).
/// Returns whether that crossfade reached its end this block.
#[inline(always)]
fn run_node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
) -> bool {
    let (rec, plan, frames) = (head.rec, head.plan, head.frames);
    debug_assert_eq!(
        u.generation, rec.generation,
        "unit generation matches the plan"
    );
    let borrows = &plan.nodes.borrows[..];
    if u.fade.is_some() {
        // A crossfade runs here: both units, then the blend. Off the hot
        // path, whatever the form.
        let ports = rec.ports(&plan.nodes.slots);
        return fading_node_op(u, head, st, ports, borrows);
    }
    // Each arm hands `node_op` its port slots. For the three direct forms
    // they are literal one-element arrays, so once `node_op` is inlined every
    // per-port loop in it — masks, skip, finish — folds to straight-line
    // code.
    match rec.form {
        Form::Source { out } => {
            node_op(u, head, st, [&[], &[out], &[], &[]], |call, node, st| {
                let mut outs = [st.arena.slot_mut(out, frames)];
                call.process(node, &[], &mut outs)
            });
        }
        Form::Split { input, out } => node_op(
            u,
            head,
            st,
            [&[input], &[out], &[], &[]],
            |call, node, st| {
                let (i, o) = st.arena.pair(input, out, frames);
                call.process(node, &[i], &mut [o])
            },
        ),
        Form::InPlace { slot } => node_op(
            u,
            head,
            st,
            [&[slot], &[slot], &[], &[]],
            |call, node, st| {
                let mut outs = [st.arena.slot_mut(slot, frames)];
                call.process(node, &[&[]], &mut outs)
            },
        ),
        // The stereo shapes: fixed widths, so `direct_op` gets the same
        // straight-line call path.
        Form::Direct(d) => {
            let slots = &plan.nodes.slots[..];
            match (d.ins, d.outs) {
                (0, 2) => direct_op::<0, 2, A, F, E>(u, head, st, &d, slots),
                (1, 2) => direct_op::<1, 2, A, F, E>(u, head, st, &d, slots),
                (2, 1) => direct_op::<2, 1, A, F, E>(u, head, st, &d, slots),
                (2, 2) => direct_op::<2, 2, A, F, E>(u, head, st, &d, slots),
                _ => unreachable!("rule 7 admits only `Direct::SHAPES`"),
            }
        }
        // Port tables are stack arrays; pick the smallest bucket that fits
        // so a two-port node does not initialise 64 entries per call. An
        // event-free node builds no event table at all, whatever its width.
        Form::Audio => {
            let ports = rec.ports(&plan.nodes.slots);
            node_op(u, head, st, ports, |call, node, st| {
                match rec.ain.max(rec.aout) {
                    0..=4 => call.run_audio::<4, A>(node, &mut *st.arena, borrows),
                    5..=16 => call.run_audio::<16, A>(node, &mut *st.arena, borrows),
                    _ => call.run_audio::<MAX_PORTS, A>(node, &mut *st.arena, borrows),
                }
            });
        }
        Form::General => {
            let ports = rec.ports(&plan.nodes.slots);
            node_op(u, head, st, ports, |call, node, st| {
                let a = rec.ain.max(rec.aout);
                let e = rec.ein.max(rec.eout);
                let (inject, overlay) = (&*st.inject, &st.overlay[..]);
                let extra = Extra { inject, overlay };
                let (arena, events) = (&mut *st.arena, &mut *st.events);
                let dropped = &mut *st.dropped;
                match (a, e) {
                    (0..=4, 0..=4) => {
                        call.run::<4, 4, A, E>(node, arena, events, borrows, extra, dropped)
                    }
                    (0..=16, 0..=4) => {
                        call.run::<16, 4, A, E>(node, arena, events, borrows, extra, dropped)
                    }
                    _ => call.run::<MAX_PORTS, MAX_PORTS, A, E>(
                        node, arena, events, borrows, extra, dropped,
                    ),
                }
            });
        }
    }
    false
}

/// A node op with a crossfade running: the outgoing unit on the op's inputs
/// into its scratch, then the incoming unit as any op runs (events and all,
/// through the general borrow), then the blend into the op's output slots.
/// Returns whether the fade reached its end in this block.
///
/// The outgoing unit runs **first**: an input aliased in place is also the
/// incoming unit's output, and holds the input only until that unit runs.
/// It is handed its own output buffers (never in place), no events, and
/// event writers that accept nothing — see the `fade` module (`src/fade.rs`).
/// Only the op's own slots are touched, so the verifier's rules about the op
/// cover the fade too.
#[cold]
#[inline(never)]
fn fading_node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    [ain, aout, ein, eout]: [&[u32]; 4],
    borrows: &[(u32, Role)],
) -> bool {
    let frames = h.frames;
    // The key's params, once for both units: the outgoing one hears the
    // modulation the incoming one does.
    let modulated = h.rec.params.len != 0 || u.params.busy();
    if modulated {
        run_params(u, h, st);
    }
    let params = u.params.inputs(frames);
    let params = &params[..u.params.port_count()];
    let x = u.fade.as_mut().expect("a fading unit");
    let status = {
        let (silent, constant) = in_masks(ain, &*st.flags);
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (i, &s) in ins.iter_mut().zip(ain) {
            *i = st.arena.slot(s, frames);
        }
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        x.scratch.leading_mut(frames, &mut outs[..aout.len()]);
        for o in &mut outs[..aout.len()] {
            o.fill(0.0);
        }
        let evin = [SortedEvents::EMPTY; MAX_PORTS];
        let mut evout: [EventWriter<'_>; MAX_PORTS] =
            std::array::from_fn(|_| EventWriter::detached());
        let io = Io::new(
            h.max,
            frames,
            &ins[..ain.len()],
            &mut outs[..aout.len()],
            silent,
            constant,
            InPlaceMask::NONE,
            &evin[..ein.len()],
            &mut evout[..eout.len()],
        )
        .with_params(params);
        let cx = Cx {
            env: h.env,
            arrival: h.rec.arrival,
            history: h.history,
        };
        x.out
            .as_mut()
            .expect("a running fade holds its outgoing unit")
            .process(&cx, io)
    };
    // The outgoing unit's status, applied to its scratch as `finish` applies
    // one to the arena.
    for c in 0..aout.len() {
        let o = x.scratch.slot_mut(c as u32, frames);
        match status {
            Status::Modified | Status::Masked { .. } => {}
            Status::Silent | Status::Idle => o.fill(0.0),
            Status::Constant => {
                let v = o[0];
                o.fill(v);
            }
            Status::Bypass => match ain.get(c) {
                Some(&i) => o.copy_from_slice(st.arena.slot(i, frames)),
                None => o.fill(0.0),
            },
        }
    }

    node_op_with(
        u,
        h,
        st,
        [ain, aout, ein, eout],
        !modulated,
        |call, node, st| {
            let (inject, overlay) = (&*st.inject, &st.overlay[..]);
            let extra = Extra { inject, overlay };
            let (arena, events) = (&mut *st.arena, &mut *st.events);
            let dropped = &mut *st.dropped;
            call.run::<MAX_PORTS, MAX_PORTS, A, E>(node, arena, events, borrows, extra, dropped)
        },
    );

    // The blend: `incoming * g_in + outgoing * g_out` while the fade lasts,
    // the incoming unit alone after it (`CrossfadeCurve::gains`).
    let x = u.fade.as_mut().expect("still fading");
    let n = (x.len - x.done).min(frames);
    for (c, &s) in (0u32..).zip(aout) {
        let y = st.arena.slot_mut(s, frames);
        let out = x.scratch.slot(c, frames);
        for (i, (y, &o)) in y[..n].iter_mut().zip(&out[..n]).enumerate() {
            let (g_in, g_out) = x.curve.gains(x.done + i, x.len);
            *y = *y * g_in + o * g_out;
        }
        st.flags.set(s, 0);
    }
    x.done += n;
    // Never skipped while fading: the next block's skip test reads these.
    u.last_quiet = false;
    u.last_idle = false;
    x.done >= x.len
}

/// Per audio slot: every sample of the block is exact `0.0`.
pub(crate) const SILENT: u8 = 1;
/// Per audio slot: the block holds one value throughout.
const CONSTANT: u8 = 2;

#[inline]
fn flag(silent: bool, constant: bool) -> u8 {
    (silent as u8 * SILENT) | (constant as u8 * CONSTANT)
}

/// A node's input masks from its input slots' flags: one load and two
/// shifts per channel. Channels past 63 are never set (the masks are 64
/// wide, and `compile` refuses a wider node anyway).
#[inline]
fn in_masks<F: SlotFlags + ?Sized>(ain: &[u32], flags: &F) -> (SilenceMask, ConstantMask) {
    let (mut silent, mut constant) = (0u64, 0u64);
    for (c, &s) in (0..64u32).zip(ain) {
        let f = u64::from(flags.get(s));
        silent |= (f & u64::from(SILENT)) << c;
        constant |= ((f & u64::from(CONSTANT)) >> 1) << c;
    }
    (SilenceMask(silent), ConstantMask(constant))
}

/// A node op's constants: its record and the block's.
pub(crate) struct Head<'p, 'e> {
    pub(crate) rec: &'p NodeRec,
    /// The plan, for the param tables.
    pub(crate) plan: &'p Plan,
    pub(crate) frames: usize,
    pub(crate) max: MaxBlock,
    pub(crate) cap: usize,
    pub(crate) env: &'e Env,
    /// The executor's transport history, observed before any op runs this
    /// block: every participant reads the same one, and nobody writes it
    /// until the block is done.
    pub(crate) history: &'e TransportHistory,
}

/// What a node op may write besides its own unit: every buffer through `A`
/// and `E` (the arena and event slots themselves serially, a claimed view
/// of them in parallel), and only its own unit's per-block flags.
pub(crate) struct OpState<'s, A: ?Sized, F: ?Sized, E: ?Sized> {
    pub(crate) arena: &'s mut A,
    pub(crate) flags: &'s mut F,
    pub(crate) events: &'s mut E,
    /// Flushed events waiting for their sinks: every entry serially, only
    /// this unit's in parallel. Either way, entries are matched by unit.
    pub(crate) inject: &'s mut [Inject],
    /// Per plan unit, whether flushed events wait for it: every unit's
    /// serially, only this one's (at index `at`) in parallel. Indexed only
    /// for a node with event inputs, so an event-free call pays nothing.
    pub(crate) has_inject: &'s mut [bool],
    /// Per plan unit, whether a scheduled command lands on it this block;
    /// indexed like `has_inject`.
    pub(crate) has_due: &'s mut [bool],
    /// This unit's index into `has_inject` and `has_due`.
    pub(crate) at: usize,
    /// Where a node's scheduled events are merged into its inputs.
    pub(crate) overlay: &'s mut Vec<Event>,
    pub(crate) due: Deliveries<'s>,
    pub(crate) dropped: &'s mut Dropped,
}

/// Run one node op: the silence skip, flushed-event injection, the call
/// itself (`call_node` borrows the buffers its [`Form`] names and runs the
/// node), and the status bookkeeping.
///
/// `#[inline(always)]` so each [`Form`] gets its own copy: the direct forms
/// pass literal one-element port arrays, and every per-port loop here then
/// folds to straight-line code. That, and not the borrow alone, is most of
/// what the direct forms save.
#[inline(always)]
fn node_op<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    ports: [&[u32]; 4],
    call_node: impl FnOnce(&Call<'_, '_>, &mut dyn Node, &mut OpState<'_, A, F, E>) -> Status,
) {
    node_op_with(u, h, st, ports, false, call_node);
}

/// Run a unit's fused param step (see the `param` module docs,
/// `src/param.rs`): each declared param, from its source this plan, into
/// the unit's param buffers. Off the hot path: only a unit with a driven
/// param, or one mid-declick, gets here.
#[inline(never)]
fn run_params<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &OpState<'_, A, F, E>,
) {
    let (rec, frames, plan) = (h.rec, h.frames, h.plan);
    let Unit { node, params, .. } = u;
    params.begin();
    let ports = &plan.param_ports[rec.params.range()];
    let mut next = ports.iter().peekable();
    let declared = rec.declared;
    // The state was sized from the unit's shape; a fade's two units declare
    // the same params (checked where it is asked for), so the two agree.
    let n = declared.len().min(params.port_count());
    for (k, &param) in declared.as_slice().iter().enumerate().take(n) {
        let base = node.param_base(k);
        match next.next_if(|p| p.port as usize == k) {
            Some(p) => {
                let events = st.events.ev(p.slot);
                params.port(k, param, frames, Some((p.from, events)), base);
            }
            None => params.port(k, param, frames, None, base),
        }
    }
}

/// [`node_op`], with the unit's params already run this block when
/// `params_done` (a crossfade runs them once for both units).
#[inline(always)]
fn node_op_with<A: AudioSlots, F: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    [ain, aout, ein, eout]: [&[u32]; 4],
    params_done: bool,
    call_node: impl FnOnce(&Call<'_, '_>, &mut dyn Node, &mut OpState<'_, A, F, E>) -> Status,
) {
    let (rec, frames) = (h.rec, h.frames);
    let unit = rec.unit;
    // Flushed events and scheduled commands only ever go to an event input.
    let injected = !ein.is_empty() && st.has_inject[st.at];
    // A bypassed node is sent no events but the closes of what it holds
    // (`mode.rs`): its scheduled commands are dropped.
    let discards = !ein.is_empty() && u.mode.discards_events();
    let scheduled = !ein.is_empty() && !discards && st.has_due[st.at];

    let (in_silent, in_constant) = in_masks(ain, &*st.flags);
    let quiet_inputs = !injected
        && !scheduled
        && in_silent.covers(ain.len())
        && ein.iter().all(|&s| st.events.ev(s).is_empty());

    // See the module docs: a node with event inputs parks only on its own
    // say-so (`Status::Idle`); one without parks when its inputs, its last
    // output and its latency plus tail agree.
    let skip = quiet_inputs
        && if ein.is_empty() {
            !ain.is_empty()
                && u.last_quiet
                && tail_elapsed(
                    match u.reported {
                        None => rec.park_after,
                        Some(t) => Tail::Finite(rec.latency).then(t),
                    },
                    u.quiet,
                )
        } else {
            u.last_idle
        };
    // A bypassed node is called until it has been sent the close of what
    // it holds, whatever it says of itself.
    let skip = skip && !(discards && u.mode.holds_input());
    u.quiet = if quiet_inputs {
        u.quiet.saturating_add(frames as u64)
    } else {
        0
    };
    // The fused param step: only for a unit with a modulated param, or one
    // still declicking off one; every other unit pays this one branch. Run
    // whether or not the node is then skipped, so its ramps and declick
    // advance with the timeline as the reference's do.
    if !params_done && (rec.params.len != 0 || u.params.busy()) {
        run_params(u, h, st);
    }
    if skip {
        for &s in aout {
            st.arena.slot_mut(s, frames).fill(0.0);
            st.flags.set(s, SILENT | CONSTANT);
        }
        for &s in eout {
            st.events.ev_mut(s).clear();
        }
        return;
    }

    if injected {
        // Flushed events keep their spacing where it fits in this block;
        // what does not is clamped to its last frame. Merged with this
        // block's own events, ties to the flushed ones (they are older).
        for inj in st.inject.iter_mut().filter(|i| i.live && i.unit == unit) {
            for e in &mut inj.events {
                e.offset = e.offset.clamp_to(frames);
            }
            let slot = ein[inj.port as usize];
            inj.merged.clear();
            let cap_total = inj.merged.capacity();
            merge_into(
                &[&inj.events, st.events.ev(slot)],
                &mut inj.merged,
                cap_total,
            );
        }
    }

    // Scheduled commands landing on this node this block: one more source
    // per port, after everything else, merged into the overlay buffer.
    let mut views = [(0u16, 0u32, 0u32); MAX_PORTS];
    let mut n_views = 0;
    if scheduled {
        let (inject, events) = (&*st.inject, &*st.events);
        let base = |port: u16| -> &[Event] {
            inject
                .iter()
                .find(|i| i.live && i.unit == unit && i.port == port)
                .map_or(&events.ev(ein[port as usize])[..], |i| &i.merged)
        };
        let (n, lost) = st.due.overlay(unit, base, st.overlay, &mut views);
        n_views = n;
        st.dropped.at_node(rec.unit, lost);
    }
    if discards {
        // Every event input is replaced by what it must close, as far as
        // the overlay takes (the rest next block).
        st.overlay.clear();
        for (port, held) in u.mode.held_in().iter_mut().enumerate() {
            let from = st.overlay.len() as u32;
            held.drain_into(st.overlay);
            views[n_views] = (port as u16, from, st.overlay.len() as u32);
            n_views += 1;
        }
        st.has_due[st.at] = false;
    } else if !u.mode.held_in().is_empty() {
        // What each event input is sent, as the node will see it: the
        // port's slot, or what flushed events and scheduled commands made
        // of it. A bypass closes what is held (`mode.rs`).
        for (port, held) in u.mode.held_in().iter_mut().enumerate() {
            let view = views[..n_views].iter().find(|v| usize::from(v.0) == port);
            let flushed = st
                .inject
                .iter()
                .find(|i| i.live && i.unit == unit && usize::from(i.port) == port);
            let events: &[Event] = match (view, flushed) {
                (Some(&(_, a, b)), _) => &st.overlay[a as usize..b as usize],
                (None, Some(i)) if injected => &i.merged,
                _ => &st.events.ev(ein[port])[..],
            };
            held.observe(events);
        }
    }

    // The table only for a unit whose step delivered frames this block;
    // every other node reads `Base` for every param (`Io::param`).
    let table;
    let params: &[ParamInput<'_>] = if u.params.busy() {
        table = u.params.inputs(frames);
        &table[..u.params.port_count()]
    } else {
        &[]
    };
    let reported = Cell::new(None);
    let call = Call {
        reported: &reported,
        env: h.env,
        history: h.history,
        max: h.max,
        frames,
        rec,
        silent: in_silent,
        constant: in_constant,
        cap: rec.event_capacity.map_or(h.cap, |n| n.get() as usize),
        injected,
        scheduled: &views[..n_views],
        params,
    };
    let status = call_node(&call, &mut *u.node, st);
    // This call's report, or the ceiling when it made none.
    u.reported = reported
        .get()
        .and_then(|r| crate::node::narrow_tail(rec.tail, r));
    if scheduled {
        st.has_due[st.at] = false;
    }
    if injected {
        for inj in st.inject.iter_mut().filter(|i| i.unit == unit) {
            inj.events.clear();
            inj.merged.clear();
            inj.live = false;
        }
        st.has_inject[st.at] = false;
    }

    u.last_idle = status == Status::Idle;
    finish(
        status,
        frames,
        ain,
        aout,
        rec.in_place,
        &mut *st.arena,
        st.flags,
    );
    // A node with no outputs claims nothing by being "all silent": it is a
    // sink, called for its side effects, and a skip would starve them.
    u.last_quiet = !(aout.is_empty() && eout.is_empty())
        && aout.iter().all(|&s| st.flags.get(s) & SILENT != 0)
        && eout.iter().all(|&s| st.events.ev(s).is_empty());
    if u.keys.is_closing() {
        // A replace took over from a unit that held keys: close them ahead
        // of this block's events, as many as the slot takes; the rest next
        // block.
        for (port, &s) in eout.iter().enumerate() {
            u.keys.close_port(port, st.events.ev_mut(s));
        }
        u.keys.settle();
    }
    for (port, &s) in eout.iter().enumerate() {
        u.keys.observe(port, st.events.ev(s));
    }
}

/// A [`Form::Direct`] node op with `I` inputs and `O` outputs: its port
/// slots as fixed-size arrays, so `node_op`'s per-port loops fold as they do
/// for the one-channel forms, and its buffers borrowed by
/// [`Arena::direct`].
#[inline(always)]
fn direct_op<
    const I: usize,
    const O: usize,
    A: AudioSlots,
    F: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, F, E>,
    d: &Direct,
    slots: &[u32],
) {
    let [ain, aout, _, _] = h.rec.ports(slots);
    let ain: &[u32; I] = ain.try_into().expect("a direct record's input count");
    let aout: &[u32; O] = aout.try_into().expect("a direct record's output count");
    let frames = h.frames;
    node_op(u, h, st, [ain, aout, &[], &[]], |call, node, st| {
        let (ins, mut outs) = st.arena.direct::<I, O>(frames, d);
        call.process(node, &ins, &mut outs)
    });
}

/// Event sources a general node call reads besides its slots: flushed
/// events (`inject`) and scheduled commands merged in (`overlay`).
#[derive(Clone, Copy)]
struct Extra<'s> {
    inject: &'s [Inject],
    overlay: &'s [Event],
}

/// One node call's constants, shared by every borrow form.
struct Call<'p, 'e> {
    env: &'e Env,
    history: &'e TransportHistory,
    max: MaxBlock,
    frames: usize,
    rec: &'p NodeRec,
    silent: SilenceMask,
    constant: ConstantMask,
    /// Events each of the node's event output writers accepts: its declared
    /// `Shape::event_capacity`, or the executor's default.
    cap: usize,
    /// Whether flushed events wait for this call (see the module docs).
    injected: bool,
    /// `(port, start, end)` into the overlay buffer: ports whose events this
    /// block include scheduled commands.
    scheduled: &'p [(u16, u32, u32)],
    /// What each declared param reads this block.
    params: &'p [ParamInput<'p>],
    /// Where the node's [`Io::report_tail`] lands.
    reported: &'p Cell<Option<Tail>>,
}

impl<'p> Call<'p, '_> {
    /// Run an event-free node on buffers already borrowed.
    #[inline]
    fn process<'a>(
        &self,
        node: &mut dyn Node,
        ins: &'a [&'a [f32]],
        outs: &'a mut [&'a mut [f32]],
    ) -> Status
    where
        'p: 'a,
    {
        let io = Io::new(
            self.max,
            self.frames,
            ins,
            outs,
            self.silent,
            self.constant,
            self.rec.in_place,
            &[],
            &mut [],
        )
        .with_params(self.params)
        .with_tail(self.reported);
        node.process(&self.cx(), io)
    }

    /// Built at the call, not kept in `Call`: the node takes it by
    /// reference, so whatever holds it is spilled to the stack.
    #[inline]
    fn cx(&self) -> Cx<'_> {
        Cx {
            env: self.env,
            arrival: self.rec.arrival,
            history: self.history,
        }
    }

    /// Borrow an event-free node's audio buffers by its presorted requests
    /// (`borrows` is the plan's whole list; the record names its run) and
    /// run it. `N` must hold the wider of its two sides.
    fn run_audio<const N: usize, A: AudioSlots>(
        &self,
        node: &mut dyn Node,
        arena: &mut A,
        borrows: &[(u32, Role)],
    ) -> Status {
        let (n_in, n_out) = (self.rec.ain as usize, self.rec.aout as usize);
        debug_assert!(n_in <= N && n_out <= N);
        let mut ins: [&[f32]; N] = [&[]; N];
        let mut outs: [&mut [f32]; N] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(
            self.frames,
            &borrows[self.rec.borrows.range()],
            &mut ins,
            &mut outs,
        );
        self.process(node, &ins[..n_in], &mut outs[..n_out])
    }

    /// Borrow a node's audio and event buffers and run it. `N` must hold the
    /// wider audio side and `M` the wider event side. Returns the node's
    /// status, and charges the events its writers refused to `dropped`, by
    /// port.
    fn run<const N: usize, const M: usize, A: AudioSlots, E: EventSlots + ?Sized>(
        &self,
        node: &mut dyn Node,
        arena: &mut A,
        events: &mut E,
        borrows: &[(u32, Role)],
        extra: Extra<'_>,
        dropped: &mut Dropped,
    ) -> Status {
        let Extra { inject, overlay } = extra;
        let rec = self.rec;
        let frames = self.frames;
        let (n_in, n_out) = (rec.ain as usize, rec.aout as usize);
        let (e_in, e_out) = (rec.ein as usize, rec.eout as usize);
        debug_assert!(n_in.max(n_out) <= N && e_in.max(e_out) <= M);

        let mut ins: [&[f32]; N] = [&[]; N];
        let mut outs: [&mut [f32]; N] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(frames, &borrows[rec.borrows.range()], &mut ins, &mut outs);

        let mut evin: [SortedEvents<'_>; M] = [SortedEvents::EMPTY; M];
        let mut evout_bufs: [Option<&mut Vec<Event>>; M] = std::array::from_fn(|_| None);
        events.borrow_sorted(
            &borrows[rec.event_borrows.range()],
            |port, v| evin[port as usize] = SortedEvents::trusted(v, frames),
            |port, v| evout_bufs[port as usize] = Some(v),
        );
        if self.injected {
            for inj in inject.iter().filter(|i| i.live && i.unit == rec.unit) {
                evin[inj.port as usize] = SortedEvents::trusted(&inj.merged, frames);
            }
        }
        for &(port, a, b) in self.scheduled {
            evin[port as usize] = SortedEvents::trusted(&overlay[a as usize..b as usize], frames);
        }
        // One count per port, so a loss is charged to the port that had it.
        let drops: [PortCount; M] = std::array::from_fn(|_| PortCount::default());
        let mut evout: [EventWriter<'_>; M] = std::array::from_fn(|_| EventWriter::detached());
        for ((w, b), d) in evout
            .iter_mut()
            .zip(evout_bufs.iter_mut())
            .zip(&drops)
            .take(e_out)
        {
            let b = b.take().expect("event output borrowed");
            b.clear();
            *w = EventWriter::new(b, self.cap, frames as u32, d);
        }
        let unplaced = Cell::new(0u32);
        let io = Io::new(
            self.max,
            frames,
            &ins[..n_in],
            &mut outs[..n_out],
            self.silent,
            self.constant,
            rec.in_place,
            &evin[..e_in],
            &mut evout[..e_out],
        )
        .with_params(self.params)
        .with_lost(&unplaced)
        .with_tail(self.reported);
        let status = node.process(&self.cx(), io);
        if unplaced.get() != 0 {
            dropped.at_node(rec.unit, unplaced.get());
        }
        for (port, d) in drops.iter().take(e_out).enumerate() {
            let (refused, recovered) = (d.refused.get(), d.recovered.get());
            if refused != 0 || recovered != 0 {
                dropped.at_port(rec.unit, port as u16, refused, recovered);
            }
        }
        status
    }
}

/// Apply a node's [`Status`] to its output slots and their flags.
#[inline(always)]
fn finish<A: AudioSlots, F: SlotFlags + ?Sized>(
    status: Status,
    frames: usize,
    ain: &[u32],
    aout: &[u32],
    in_place: InPlaceMask,
    arena: &mut A,
    flags: &mut F,
) {
    match status {
        Status::Modified => {
            for &s in aout {
                flags.set(s, 0);
            }
        }
        Status::Masked {
            silent: sm,
            constant: cm,
        } => {
            for (c, &s) in aout.iter().enumerate() {
                flags.set(s, flag(sm.get(c), cm.get(c) || sm.get(c)));
            }
        }
        Status::Silent | Status::Idle => {
            for &s in aout {
                arena.slot_mut(s, frames).fill(0.0);
                flags.set(s, SILENT | CONSTANT);
            }
        }
        Status::Constant => {
            for &s in aout {
                let buf = arena.slot_mut(s, frames);
                let v = buf[0];
                buf.fill(v);
                flags.set(s, flag(v == 0.0 && v.is_sign_positive(), true));
            }
        }
        Status::Bypass => {
            for (c, &s) in aout.iter().enumerate() {
                match ain.get(c) {
                    Some(_) if in_place.get(c) => {}
                    Some(&i) => {
                        arena.copy_slot(i, s);
                        flags.set(s, flags.get(i));
                    }
                    None => {
                        arena.slot_mut(s, frames).fill(0.0);
                        flags.set(s, SILENT | CONSTANT);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tutti_types::{At, ChannelLayout, SampleRate};

    use super::*;
    use crate::event::EventKind;
    use tutti_ump::Ump;

    /// Counts the events it receives.
    struct Count(Arc<AtomicUsize>);

    impl Node for Count {
        fn shape(&self) -> crate::node::Shape {
            crate::node::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
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
    /// a concurrent editor can cause — see `tests/commands.rs` for the
    /// threaded version) is reproduced by running the executor's command
    /// pass before it applies the queued commit. The command must wait, not
    /// be judged against the old plan (where its node does not exist), and
    /// land once the commit does.
    ///
    /// Mutation: drop the `plan_seq > applied` hold in `CommandRx::gather`
    /// → the command is counted unrouted in the staged pass → fails.
    #[test]
    fn a_command_seen_before_its_commit_waits_for_it() {
        let prepare = Prepare::new(SampleRate(48_000.0), Samples(64));
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
            frame: exec.frame,
            sample_rate: prepare.sample_rate(),
            block_len: Samples(64),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: crate::PlayClock::ZERO,
        };
        let mut has_due = vec![false; plan.units.len()];
        exec.commands
            .gather(&env, &plan, exec.applied, &mut has_due);
        assert_eq!(
            exec.unrouted_command_count(),
            0,
            "judged against a stale plan"
        );
        assert_eq!(ed.commands_outstanding(), 1, "it waits");

        // Then the commit lands, and the command with it.
        exec.process(64, &Transport::default(), &[], &mut []);
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        assert_eq!(ed.commands_outstanding(), 0);
        assert_eq!(exec.unrouted_command_count(), 0);
    }
}
