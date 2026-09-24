//! The runtime's audio side: [`Executor`], and the commit box that carries an
//! edit to it over a queue and the retired state back.
//!
//! Doc 013 §4. Units exist exactly once, in the executor's store. An edit
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
//! control thread. If the executor is dropped with commits still queued, the
//! rings go with it and those commits are dropped on the thread that drops
//! the executor — the control thread in practice, never the callback. If the
//! editor is dropped, the executor keeps running its current plan; boxes it
//! returns afterwards wait in the return ring until the executor is dropped.
//!
//! Applying runs under the [`AudioThread::enter`] marker, so a unit or a
//! commit dropped there panics in a debug build. The marker's coverage of
//! applying is **partial** in this phase: applying *allocates* (it builds the
//! new arena and delay state) and frees transient allocations of its own —
//! the key maps it builds to carry state across, a ring's old buffer on a
//! retune, the flush lists — which are plain `Vec`s and `BTreeMap`s the
//! marker does not see. Moving that work to the control side, so applying
//! only swaps pointers, is the remaining Phase 2 work. Every type crossing
//! the queue is already `Send` (units, commits) or `Send + Sync` (the plan).
//!
//! # The serial executor
//!
//! Walks [`Plan::ops`] in order, under a flush-to-zero guard
//! ([`ScopedNoDenormals`]). Per block it never allocates when no commit is
//! pending (see `tests/rt_no_alloc.rs`) and hands every node the **whole
//! block** and its sorted events.
//!
//! **A node op is one record.** The compiler lowers each `Op::Node` into a
//! `NodeRec` (`plan.rs`): store index, generation, arrival, tail, the port
//! slots, the buffer borrow requests **already sorted**, and a `Form` that
//! picks the borrow. The verifier checks each record against its op, so a
//! call does one indexed load where it used to do six, and
//! never sorts. The commonest shapes — at most one audio input and one
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
//!   every channel), and its declared tail has elapsed since its inputs went
//!   quiet. A node never called yet is not quiet.
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

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_types::{AudioThread, Frame, NodeKey, Samples, ScopedNoDenormals, Tail};

use crate::arena::{borrow_disjoint, borrow_sorted, Arena, Role};
use crate::command::{overlay_capacity, CommandRx};
use crate::event::{merge_into, Event, EventWriter, SortedEvents};
use crate::io::Io;
use crate::kernels::{AudioRing, EventFifo};
use crate::node::{
    ConstantMask, Cx, Env, InPlaceMask, MaxBlock, Node, Prepare, SilenceMask, Status, Transport,
    MAX_PORTS,
};
use crate::plan::{DelayKey, Delta, Direct, FeedbackKey, Form, NodeRec, Op, Plan, UnitIdx};
use crate::spec::EventIn;

/// Events one event slot holds per block, unless configured otherwise.
pub const DEFAULT_EVENT_CAPACITY: usize = 512;

/// Commits that may be out at once: sent and not yet drained back. The
/// queue holds this many; the return ring one more.
pub const QUEUE_CAPACITY: usize = 4;

/// A unit, owned. Crate-private, so the only code that can replace or drop
/// the box is this crate's; its drop is checked against the audio-thread
/// marker. (`tutti_types::Retire` is the generic, move-only form; this crate
/// must *run* its units, so it keeps a box it can hand `&mut` out of.)
pub(crate) struct NodeBox(Box<dyn Node>);

impl NodeBox {
    pub(crate) fn new(node: Box<dyn Node>) -> Self {
        Self(node)
    }
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
        AudioThread::check_not_current("a unit");
    }
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
    plan: Option<Arc<Plan>>,
    delta: Delta,
    incoming: Vec<(UnitIdx, u32, NodeBox)>,
    retired: Vec<(NodeKey, NodeBox)>,
    old_state: Option<State>,
}

impl Drop for Commit {
    fn drop(&mut self) {
        AudioThread::check_not_current("Commit");
    }
}

impl Commit {
    pub(crate) fn build(
        seq: u64,
        plan: Arc<Plan>,
        delta: Delta,
        mut units: BTreeMap<NodeKey, Box<dyn Node>>,
    ) -> Box<Commit> {
        let wanted = delta
            .insert
            .iter()
            .copied()
            .chain(delta.replace.iter().map(|&(_, new)| new));
        let incoming = wanted
            .map(|p| {
                let unit = units
                    .remove(&p.key)
                    .unwrap_or_else(|| panic!("no unit supplied for node {}", p.key.0));
                (p.idx, p.gen, NodeBox::new(unit))
            })
            .collect();
        assert!(
            units.is_empty(),
            "units supplied for nodes the delta does not place: {:?}",
            units.keys().collect::<Vec<_>>()
        );
        // Reserved here so applying moves retirees in without growing.
        let retired = Vec::with_capacity(delta.retire.len() + delta.replace.len());
        Box::new(Self {
            seq,
            plan: Some(plan),
            delta,
            incoming,
            retired,
            old_state: None,
        })
    }

    /// The units applying removed, by key.
    pub(crate) fn retired(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.retired.iter().map(|(k, _)| *k)
    }
}

/// The queue pair between an editor and its executor.
pub(crate) struct Channels {
    pub(crate) to_executor: HeapProd<Box<Commit>>,
    pub(crate) returned: HeapCons<Box<Commit>>,
}

/// Build the queue pair: the editor's ends and the executor's.
pub(crate) fn channels() -> (Channels, HeapCons<Box<Commit>>, HeapProd<Box<Commit>>) {
    let (tx, rx) = HeapRb::<Box<Commit>>::new(QUEUE_CAPACITY).split();
    let (back_tx, back_rx) = HeapRb::<Box<Commit>>::new(QUEUE_CAPACITY + 1).split();
    (
        Channels {
            to_executor: tx,
            returned: back_rx,
        },
        rx,
        back_tx,
    )
}

struct Unit {
    gen: u32,
    node: NodeBox,
    /// Consecutive frames of fully silent input, saturating.
    quiet: u64,
    /// Whether the last call (or skip) left every output silent and wrote no
    /// event.
    last_quiet: bool,
    /// Whether the last call returned [`Status::Idle`].
    last_idle: bool,
}

enum Ring {
    Audio(AudioRing),
    Event(EventFifo),
}

/// Events flushed from a delay that went away, waiting for their sink's next
/// call, at their offsets relative to the earliest (see the module docs).
struct Inject {
    unit: u32,
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
    /// built with a load and two shifts per channel.
    flags: Vec<u8>,
    events: Vec<Vec<Event>>,
    rings: Vec<Option<Ring>>,
    audio_fb: Vec<Option<AudioRing>>,
    event_fb: Vec<Option<EventFifo>>,
    inject: Vec<Inject>,
    has_inject: Vec<bool>,
    /// Plan units with a scheduled command landing this block.
    has_due: Vec<bool>,
    /// Where a node's scheduled events are merged into its inputs; sized
    /// when applying so the merge never grows (`command::overlay_capacity`).
    overlay: Vec<Event>,
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
            inject: Vec::new(),
            has_inject: Vec::new(),
            has_due: Vec::new(),
            overlay: Vec::new(),
        }
    }
}

/// The serial plan executor. Built with its [`Editor`](crate::Editor) by
/// [`Editor::new`](crate::Editor::new): the two share one [`Prepare`] and
/// one queue pair, and the executor applies that editor's commits, in order,
/// at the start of each block.
pub struct Executor {
    prepare: Prepare,
    event_cap: usize,
    queue: HeapCons<Box<Commit>>,
    back: HeapProd<Box<Commit>>,
    plan: Option<Arc<Plan>>,
    store: Vec<Option<Unit>>,
    state: State,
    frame: Frame,
    dropped: u64,
    commands: CommandRx,
    /// `seq` of the last commit applied.
    applied: u64,
}

fn tail_elapsed(tail: Tail, quiet: u64) -> bool {
    match tail {
        Tail::None => true,
        Tail::Finite(n) => quiet >= n.get() as u64,
        Tail::Unknown | Tail::Unbounded => false,
    }
}

impl Executor {
    pub(crate) fn new(
        prepare: Prepare,
        cap: usize,
        queue: HeapCons<Box<Commit>>,
        back: HeapProd<Box<Commit>>,
        commands: CommandRx,
    ) -> Self {
        Self {
            prepare,
            event_cap: cap,
            queue,
            back,
            plan: None,
            store: Vec::new(),
            state: State::empty(prepare.max_block().get()),
            frame: Frame::ZERO,
            dropped: 0,
            commands,
            applied: 0,
        }
    }

    /// What this executor, and every unit it runs, is prepared for.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
    }

    /// The plan running now.
    pub fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// Frames rendered so far.
    pub fn frame(&self) -> Frame {
        self.frame
    }

    /// Events refused so far: writer overflow, and delay or feedback FIFO
    /// overflow (which never drops a note-off while anything else can go).
    /// A merge never drops: its slot holds all its inputs.
    pub fn dropped_events(&self) -> u64 {
        self.dropped
    }

    /// Scheduled commands that were already past due when the executor
    /// first saw them, and so landed at offset 0 of that block instead of on
    /// their frame. Never dropped — see [`Editor::schedule`](crate::Editor::schedule).
    pub fn late_commands(&self) -> u64 {
        self.commands.late()
    }

    /// Scheduled commands whose node or event port was gone by the time they
    /// fell due, so there was nowhere to deliver them.
    pub fn unrouted_commands(&self) -> u64 {
        self.commands.unrouted()
    }

    /// Apply every queued commit, in the order the editor sent them, and send
    /// each box back. [`process`](Self::process) calls this first; it is
    /// public so a caller can install a commit without rendering.
    ///
    /// Delay rings, feedback state and units carry over by key (see
    /// [`DelayKey`] and [`FeedbackKey`]). An event delay or event feedback
    /// whose key disappears flushes its pending events to its sink when the
    /// sink survives — see the module docs for where they land.
    ///
    /// # Panics
    ///
    /// If a plan was compiled for a different [`Prepare`] than this
    /// executor's (only possible through
    /// [`Editor::package`](crate::Editor::package)).
    pub fn apply_pending(&mut self) {
        let _rt = AudioThread::enter();
        while let Some(mut commit) = self.queue.try_pop() {
            self.apply(&mut commit);
            if let Err(full) = self.back.try_push(commit) {
                // Unreachable by the credit count (module docs). Leak rather
                // than free on the audio thread.
                debug_assert!(false, "the return ring is full");
                std::mem::forget(full);
            }
        }
    }

    fn apply(&mut self, c: &mut Commit) {
        let plan = c
            .plan
            .take()
            .expect("a commit carries its plan until applied");
        assert_eq!(
            plan.prepare, self.prepare,
            "a plan compiled for another Prepare: every unit is prepared for the executor's"
        );

        for p in &c.delta.retire {
            if let Some(u) = self.store.get_mut(p.idx.0 as usize).and_then(Option::take) {
                debug_assert_eq!(u.gen, p.gen, "retiring the unit the delta named");
                c.retired.push((p.key, u.node));
            }
        }
        for (old, _) in &c.delta.replace {
            if let Some(u) = self
                .store
                .get_mut(old.idx.0 as usize)
                .and_then(Option::take)
            {
                c.retired.push((old.key, u.node));
            }
        }
        if self.store.len() < c.delta.store_len as usize {
            self.store.resize_with(c.delta.store_len as usize, || None);
        }
        for (idx, gen, node) in c.incoming.drain(..) {
            let slot = &mut self.store[idx.0 as usize];
            debug_assert!(slot.is_none(), "store index {} is occupied", idx.0);
            *slot = Some(Unit {
                gen,
                node,
                quiet: 0,
                last_quiet: false,
                last_idle: false,
            });
        }

        self.applied = c.seq;
        let old_plan = self.plan.take();
        let new_state = self.rebuild(&plan, old_plan.as_deref());
        let old_state = std::mem::replace(&mut self.state, new_state);
        c.old_state = Some(old_state);
        self.plan = Some(plan);
        c.plan = old_plan;
    }

    /// The state for `plan`, carrying what survives out of `self.state`
    /// (which keeps what does not, to be retired with it).
    fn rebuild(&mut self, plan: &Plan, old_plan: Option<&Plan>) -> State {
        let cap = self.event_cap;
        let max_block = self.prepare.max_block().get();
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
        let rings = plan
            .delays
            .iter()
            .map(|d| {
                let carried = old_delays
                    .get(&d.key)
                    .and_then(|&i| old.rings.get_mut(i))
                    .and_then(Option::take);
                Some(match (d.key, carried) {
                    (DelayKey::Event { .. }, Some(Ring::Event(mut f))) => {
                        f.retune(d.len);
                        Ring::Event(f)
                    }
                    (DelayKey::Event { .. }, _) => {
                        Ring::Event(EventFifo::sized(d.len, cap, max_block))
                    }
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
                    .and_then(|&i| old.audio_fb.get_mut(i))
                    .and_then(Option::take);
                Some(carried.unwrap_or_else(|| AudioRing::new(f.key.delay())))
            })
            .collect();
        let event_fb = plan
            .event_feedback
            .iter()
            .map(|f| {
                let carried = old_efb
                    .get(&f.key)
                    .and_then(|&i| old.event_fb.get_mut(i))
                    .and_then(Option::take);
                Some(carried.unwrap_or_else(|| EventFifo::sized(f.key.delay(), cap, max_block)))
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
        let mut has_inject = vec![false; plan.units.len()];
        let flushed_total: usize = flushed.values().map(Vec::len).sum();
        let widest = plan.event_slot_weight.iter().copied().max().unwrap_or(1) as usize;
        let inject = flushed
            .into_iter()
            .map(|((unit, port), mut events)| {
                has_inject[unit as usize] = true;
                // Several flushes into one sink: one sorted list, ties in
                // flush order (stable).
                events.sort_by_key(|e| e.offset);
                let merged = Vec::with_capacity(events.len() + cap * widest);
                Inject {
                    unit,
                    port,
                    events,
                    merged,
                    live: true,
                }
            })
            .collect();

        State {
            arena: Arena::new(plan.audio_slots as usize, max_block),
            flags: {
                let mut v = vec![0u8; plan.audio_slots as usize];
                v[0] = SILENT | CONSTANT;
                v
            },
            events: plan
                .event_slot_weight
                .iter()
                .map(|&w| Vec::with_capacity(cap * w.max(1) as usize))
                .collect(),
            rings,
            audio_fb,
            event_fb,
            inject,
            has_inject,
            has_due: vec![false; plan.units.len()],
            overlay: Vec::with_capacity(overlay_capacity(plan, cap, flushed_total)),
        }
    }

    /// Render one block of `frames` into `outputs`, reading `inputs`.
    ///
    /// `inputs` must have a slice per global input channel the plan reads, and
    /// `outputs` one per global output channel; every slice at least `frames`
    /// long. Applies queued commits first ([`apply_pending`](Self::apply_pending));
    /// with none queued it never allocates. Marks the thread with
    /// [`AudioThread::enter`] for the duration.
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
        let max = self.prepare.max_block();
        assert!(
            frames > 0 && frames <= max.get(),
            "block of {frames} frames against a max of {}",
            max.get()
        );
        let _rt = AudioThread::enter();
        let _ftz = ScopedNoDenormals::new();
        self.apply_pending();
        let Self {
            prepare,
            event_cap,
            queue: _,
            back: _,
            plan,
            store,
            state,
            frame,
            dropped,
            commands,
            applied,
        } = self;
        let Some(plan) = plan.as_ref() else {
            for o in outputs.iter_mut() {
                o[..frames].fill(0.0);
            }
            *frame += Samples(frames);
            return;
        };
        let State {
            arena,
            flags,
            events,
            rings,
            audio_fb,
            event_fb,
            inject,
            has_inject,
            has_due,
            overlay,
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
        let env = Env {
            frame: *frame,
            sample_rate: prepare.sample_rate(),
            block_len: Samples(frames),
            transport: *transport,
        };
        commands.gather(&env, plan, *applied, has_due);

        // Feedback reads happen before any op: the delay is a whole
        // `MaxBlock`, so nothing this block's captures queue is due yet.
        for (f, spec) in audio_fb.iter().zip(&plan.audio_feedback) {
            let ring = f.as_ref().expect("built by apply");
            let is_silent = ring.peek_oldest(arena.slot_mut(spec.slot, frames));
            flags[spec.slot as usize] = flag(is_silent, is_silent);
        }
        for (f, spec) in event_fb.iter_mut().zip(&plan.event_feedback) {
            let fifo = f.as_mut().expect("built by apply");
            let out = &mut events[spec.slot as usize];
            out.clear();
            fifo.pop_due(out, frames);
        }

        for op in &plan.ops {
            match *op {
                Op::GlobalIn { channel, dst } => {
                    arena
                        .slot_mut(dst, frames)
                        .copy_from_slice(&inputs[channel as usize][..frames]);
                    flags[dst as usize] = 0;
                }
                Op::Delay { delay, src, dst } => {
                    let Some(Ring::Audio(ring)) = &mut rings[delay as usize] else {
                        unreachable!("audio delay on an event ring")
                    };
                    let src_silent = flags[src as usize] & SILENT != 0;
                    let out_silent = if src == dst {
                        ring.run_in_place(arena.slot_mut(dst, frames), src_silent)
                    } else {
                        let (s, d) = arena.pair(src, dst, frames);
                        ring.run(s, d, src_silent)
                    };
                    flags[dst as usize] = flag(out_silent, out_silent);
                }
                Op::EventDelay { delay, src, dst } => {
                    let Some(Ring::Event(fifo)) = &mut rings[delay as usize] else {
                        unreachable!("event delay on an audio ring")
                    };
                    let (input, output) = event_pair(events, src, dst);
                    output.clear();
                    *dropped += fifo.run(input, output, frames) as u64;
                }
                Op::EventMerge { srcs, dst } => {
                    let list = &plan.event_list[srcs.range()];
                    let mut reqs = [(0u32, Role::Read(0)); MAX_PORTS + 1];
                    for (i, &s) in list.iter().enumerate() {
                        reqs[i] = (s, Role::Read(i as u8));
                    }
                    reqs[list.len()] = (dst, Role::Write(0));
                    let mut ins: [&[Event]; MAX_PORTS] = [&[]; MAX_PORTS];
                    let mut output: Option<&mut Vec<Event>> = None;
                    borrow_disjoint(
                        events,
                        1,
                        &mut reqs[..=list.len()],
                        |port, v| ins[port as usize] = &v[0],
                        |_, v| output = Some(&mut v[0]),
                    );
                    let output = output.expect("dst borrowed");
                    output.clear();
                    // The slot holds all its inputs (`event_slot_weight`), so
                    // this never drops; counted anyway, in case it ever does.
                    let room = output.capacity();
                    *dropped += merge_into(&ins[..list.len()], output, room) as u64;
                }
                Op::Node { unit, .. } => {
                    // One record per node op, lowered at compile time and
                    // checked by the verifier: store index, generation,
                    // arrival, tail, port slots and presorted borrows.
                    let rec = &plan.nodes.recs[unit as usize];
                    let u = store[rec.store as usize]
                        .as_mut()
                        .expect("the delta placed every unit the plan runs");
                    debug_assert_eq!(u.gen, rec.gen, "unit generation matches the plan");
                    let head = Head {
                        rec,
                        frames,
                        max,
                        cap,
                        env: &env,
                    };
                    let mut st = OpState {
                        arena,
                        flags,
                        events,
                        inject,
                        has_inject,
                        has_due,
                        overlay,
                        commands,
                        dropped,
                    };
                    let borrows = &plan.nodes.borrows[..];
                    // Each arm hands `node_op` its port slots. For the three
                    // direct forms they are literal one-element arrays, so
                    // once `node_op` is inlined every per-port loop in it —
                    // masks, skip, finish — folds to straight-line code.
                    match rec.form {
                        Form::Source { out } => {
                            node_op(
                                u,
                                &head,
                                &mut st,
                                [&[], &[out], &[], &[]],
                                |call, node, st| {
                                    let mut outs = [st.arena.slot_mut(out, frames)];
                                    (call.process(node, &[], &mut outs), 0)
                                },
                            );
                        }
                        Form::Split { input, out } => node_op(
                            u,
                            &head,
                            &mut st,
                            [&[input], &[out], &[], &[]],
                            |call, node, st| {
                                let (i, o) = st.arena.pair(input, out, frames);
                                (call.process(node, &[i], &mut [o]), 0)
                            },
                        ),
                        Form::InPlace { slot } => node_op(
                            u,
                            &head,
                            &mut st,
                            [&[slot], &[slot], &[], &[]],
                            |call, node, st| {
                                let mut outs = [st.arena.slot_mut(slot, frames)];
                                (call.process(node, &[&[]], &mut outs), 0)
                            },
                        ),
                        // The stereo shapes: fixed widths, so `direct_op`
                        // gets the same straight-line call path.
                        Form::Direct(d) => {
                            let slots = &plan.nodes.slots[..];
                            match (d.ins, d.outs) {
                                (0, 2) => direct_op::<0, 2>(u, &head, &mut st, &d, slots),
                                (1, 2) => direct_op::<1, 2>(u, &head, &mut st, &d, slots),
                                (2, 1) => direct_op::<2, 1>(u, &head, &mut st, &d, slots),
                                (2, 2) => direct_op::<2, 2>(u, &head, &mut st, &d, slots),
                                _ => unreachable!("rule 7 admits only `Direct::SHAPES`"),
                            }
                        }
                        // Port tables are stack arrays; pick the smallest
                        // bucket that fits so a two-port node does not
                        // initialise 64 entries per call. An event-free node
                        // builds no event table at all, whatever its width.
                        Form::Audio => {
                            let ports = rec.ports(&plan.nodes.slots);
                            node_op(u, &head, &mut st, ports, |call, node, st| {
                                let status = match rec.ain.max(rec.aout) {
                                    0..=4 => call.run_audio::<4>(node, st.arena, borrows),
                                    5..=16 => call.run_audio::<16>(node, st.arena, borrows),
                                    _ => call.run_audio::<MAX_PORTS>(node, st.arena, borrows),
                                };
                                (status, 0)
                            });
                        }
                        Form::General => {
                            let ports = rec.ports(&plan.nodes.slots);
                            node_op(u, &head, &mut st, ports, |call, node, st| {
                                let a = rec.ain.max(rec.aout);
                                let e = rec.ein.max(rec.eout);
                                let (inject, overlay) = (&*st.inject, &st.overlay[..]);
                                let extra = Extra { inject, overlay };
                                let (arena, events) = (&mut *st.arena, &mut *st.events);
                                match (a, e) {
                                    (0..=4, 0..=4) => {
                                        call.run::<4, 4>(node, arena, events, borrows, extra)
                                    }
                                    (0..=16, 0..=4) => {
                                        call.run::<16, 4>(node, arena, events, borrows, extra)
                                    }
                                    _ => call.run::<MAX_PORTS, MAX_PORTS>(
                                        node, arena, events, borrows, extra,
                                    ),
                                }
                            });
                        }
                    }
                }
                Op::Output {
                    channel,
                    src,
                    delay,
                } => {
                    let out = &mut outputs[channel as usize][..frames];
                    let s = arena.slot(src, frames);
                    match delay {
                        Some(d) => {
                            let Some(Ring::Audio(ring)) = &mut rings[d as usize] else {
                                unreachable!("output delay on an event ring")
                            };
                            ring.run(s, out, flags[src as usize] & SILENT != 0);
                        }
                        None => out.copy_from_slice(s),
                    }
                }
                Op::Capture { feedback, src } => {
                    let ring = audio_fb[feedback as usize]
                        .as_mut()
                        .expect("built by apply");
                    ring.push(arena.slot(src, frames), flags[src as usize] & SILENT != 0);
                }
                Op::EventCapture { feedback, src } => {
                    let fifo = event_fb[feedback as usize]
                        .as_mut()
                        .expect("built by apply");
                    *dropped += fifo.push(&events[src as usize]) as u64;
                }
            }
        }

        for f in event_fb.iter_mut().flatten() {
            f.advance(frames);
        }
        *frame += Samples(frames);
    }
}

/// Per audio slot: every sample of the block is exact `0.0`.
const SILENT: u8 = 1;
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
fn in_masks(ain: &[u32], flags: &[u8]) -> (SilenceMask, ConstantMask) {
    let (mut silent, mut constant) = (0u64, 0u64);
    for (c, &s) in (0..64u32).zip(ain) {
        let f = u64::from(flags[s as usize]);
        silent |= (f & u64::from(SILENT)) << c;
        constant |= ((f & u64::from(CONSTANT)) >> 1) << c;
    }
    (SilenceMask(silent), ConstantMask(constant))
}

/// A node op's constants: its record and the block's.
struct Head<'p, 'e> {
    rec: &'p NodeRec,
    frames: usize,
    max: MaxBlock,
    cap: usize,
    env: &'e Env,
}

/// What a node op may write besides its own unit.
struct OpState<'s> {
    arena: &'s mut Arena,
    flags: &'s mut [u8],
    events: &'s mut [Vec<Event>],
    inject: &'s mut [Inject],
    has_inject: &'s mut [bool],
    /// Plan units with a scheduled command landing this block.
    has_due: &'s mut [bool],
    /// Where a node's scheduled events are merged into its inputs.
    overlay: &'s mut Vec<Event>,
    commands: &'s CommandRx,
    dropped: &'s mut u64,
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
fn node_op(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_>,
    [ain, aout, ein, eout]: [&[u32]; 4],
    call_node: impl FnOnce(&Call<'_, '_>, &mut dyn Node, &mut OpState<'_>) -> (Status, u32),
) {
    let (rec, frames) = (h.rec, h.frames);
    let unit = rec.unit;
    // Flushed events and scheduled commands only ever go to an event input.
    let injected = !ein.is_empty() && st.has_inject[unit as usize];
    let scheduled = !ein.is_empty() && st.has_due[unit as usize];

    let (in_silent, in_constant) = in_masks(ain, st.flags);
    let quiet_inputs = !injected
        && !scheduled
        && in_silent.covers(ain.len())
        && ein.iter().all(|&s| st.events[s as usize].is_empty());

    // See the module docs: a node with event inputs parks only on its own
    // say-so (`Status::Idle`); one without parks when its inputs, its last
    // output and its tail agree.
    let skip = quiet_inputs
        && if ein.is_empty() {
            !ain.is_empty() && u.last_quiet && tail_elapsed(rec.tail, u.quiet)
        } else {
            u.last_idle
        };
    u.quiet = if quiet_inputs {
        u.quiet.saturating_add(frames as u64)
    } else {
        0
    };
    if skip {
        for &s in aout {
            st.arena.slot_mut(s, frames).fill(0.0);
            st.flags[s as usize] = SILENT | CONSTANT;
        }
        for &s in eout {
            st.events[s as usize].clear();
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
                &[&inj.events, &st.events[slot as usize]],
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
                .map_or(&events[ein[port as usize] as usize][..], |i| &i.merged)
        };
        let (n, lost) = st.commands.overlay(unit, base, st.overlay, &mut views);
        n_views = n;
        *st.dropped += u64::from(lost);
    }

    let call = Call {
        env: h.env,
        max: h.max,
        frames,
        rec,
        silent: in_silent,
        constant: in_constant,
        cap: h.cap,
        injected,
        scheduled: &views[..n_views],
    };
    let (status, drops) = call_node(&call, &mut *u.node, st);
    *st.dropped += drops as u64;
    if scheduled {
        st.has_due[unit as usize] = false;
    }
    if injected {
        for inj in st.inject.iter_mut().filter(|i| i.unit == unit) {
            inj.events.clear();
            inj.merged.clear();
            inj.live = false;
        }
        st.has_inject[unit as usize] = false;
    }

    u.last_idle = status == Status::Idle;
    finish(status, frames, ain, aout, rec.in_place, st.arena, st.flags);
    u.last_quiet = aout.iter().all(|&s| st.flags[s as usize] & SILENT != 0)
        && eout.iter().all(|&s| st.events[s as usize].is_empty());
}

/// A [`Form::Direct`] node op with `I` inputs and `O` outputs: its port
/// slots as fixed-size arrays, so `node_op`'s per-port loops fold as they do
/// for the one-channel forms, and its buffers borrowed by
/// [`Arena::direct`].
#[inline(always)]
fn direct_op<const I: usize, const O: usize>(
    u: &mut Unit,
    h: &Head<'_, '_>,
    st: &mut OpState<'_>,
    d: &Direct,
    slots: &[u32],
) {
    let [ain, aout, _, _] = h.rec.ports(slots);
    let ain: &[u32; I] = ain.try_into().expect("a direct record's input count");
    let aout: &[u32; O] = aout.try_into().expect("a direct record's output count");
    let frames = h.frames;
    node_op(u, h, st, [ain, aout, &[], &[]], |call, node, st| {
        let (ins, mut outs) = st.arena.direct::<I, O>(frames, d);
        (call.process(node, &ins, &mut outs), 0)
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
    max: MaxBlock,
    frames: usize,
    rec: &'p NodeRec,
    silent: SilenceMask,
    constant: ConstantMask,
    cap: usize,
    /// Whether flushed events wait for this call (see the module docs).
    injected: bool,
    /// `(port, start, end)` into the overlay buffer: ports whose events this
    /// block include scheduled commands.
    scheduled: &'p [(u16, u32, u32)],
}

impl Call<'_, '_> {
    /// Run an event-free node on buffers already borrowed.
    #[inline]
    fn process<'a>(
        &self,
        node: &mut dyn Node,
        ins: &'a [&'a [f32]],
        outs: &'a mut [&'a mut [f32]],
    ) -> Status {
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
        );
        node.process(&self.cx(), io)
    }

    /// Built at the call, not kept in `Call`: the node takes it by
    /// reference, so whatever holds it is spilled to the stack.
    #[inline]
    fn cx(&self) -> Cx<'_> {
        Cx {
            env: self.env,
            arrival: self.rec.arrival,
        }
    }

    /// Borrow an event-free node's audio buffers by its presorted requests
    /// (`borrows` is the plan's whole list; the record names its run) and
    /// run it. `A` must hold the wider of its two sides.
    fn run_audio<const A: usize>(
        &self,
        node: &mut dyn Node,
        arena: &mut Arena,
        borrows: &[(u32, Role)],
    ) -> Status {
        let (n_in, n_out) = (self.rec.ain as usize, self.rec.aout as usize);
        debug_assert!(n_in <= A && n_out <= A);
        let mut ins: [&[f32]; A] = [&[]; A];
        let mut outs: [&mut [f32]; A] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(
            self.frames,
            &borrows[self.rec.borrows.range()],
            &mut ins,
            &mut outs,
        );
        self.process(node, &ins[..n_in], &mut outs[..n_out])
    }

    /// Borrow a node's audio and event buffers and run it. `A` must hold the
    /// wider audio side and `E` the wider event side. Returns the node's
    /// status and how many events its writers refused.
    fn run<const A: usize, const E: usize>(
        &self,
        node: &mut dyn Node,
        arena: &mut Arena,
        events: &mut [Vec<Event>],
        borrows: &[(u32, Role)],
        extra: Extra<'_>,
    ) -> (Status, u32) {
        let Extra { inject, overlay } = extra;
        let rec = self.rec;
        let frames = self.frames;
        let (n_in, n_out) = (rec.ain as usize, rec.aout as usize);
        let (e_in, e_out) = (rec.ein as usize, rec.eout as usize);
        debug_assert!(n_in.max(n_out) <= A && e_in.max(e_out) <= E);

        let mut ins: [&[f32]; A] = [&[]; A];
        let mut outs: [&mut [f32]; A] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(frames, &borrows[rec.borrows.range()], &mut ins, &mut outs);

        let mut evin: [SortedEvents<'_>; E] = [SortedEvents::EMPTY; E];
        let mut evout_bufs: [Option<&mut Vec<Event>>; E] = std::array::from_fn(|_| None);
        borrow_sorted(
            events,
            1,
            &borrows[rec.event_borrows.range()],
            |port, v| evin[port as usize] = SortedEvents::trusted(&v[0], frames),
            |port, v| evout_bufs[port as usize] = Some(&mut v[0]),
        );
        if self.injected {
            for inj in inject.iter().filter(|i| i.live && i.unit == rec.unit) {
                evin[inj.port as usize] = SortedEvents::trusted(&inj.merged, frames);
            }
        }
        for &(port, a, b) in self.scheduled {
            evin[port as usize] = SortedEvents::trusted(&overlay[a as usize..b as usize], frames);
        }
        let drops = Cell::new(0u32);
        let mut evout: [EventWriter<'_>; E] = std::array::from_fn(|_| EventWriter::detached());
        for (w, b) in evout.iter_mut().zip(evout_bufs.iter_mut()).take(e_out) {
            let b = b.take().expect("event output borrowed");
            b.clear();
            *w = EventWriter::new(b, self.cap, frames as u32, &drops);
        }
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
        );
        let status = node.process(&self.cx(), io);
        (status, drops.get())
    }
}

/// Event slot `src` for reading and `dst` for writing, at once.
fn event_pair(events: &mut [Vec<Event>], src: u32, dst: u32) -> (&[Event], &mut Vec<Event>) {
    let mut reqs = [(src, Role::Read(0)), (dst, Role::Write(0))];
    let mut input: &[Event] = &[];
    let mut output: Option<&mut Vec<Event>> = None;
    borrow_disjoint(
        events,
        1,
        &mut reqs,
        |_, v| input = &v[0],
        |_, v| output = Some(&mut v[0]),
    );
    (input, output.expect("dst borrowed"))
}

/// Apply a node's [`Status`] to its output slots and their flags.
#[inline(always)]
fn finish(
    status: Status,
    frames: usize,
    ain: &[u32],
    aout: &[u32],
    in_place: InPlaceMask,
    arena: &mut Arena,
    flags: &mut [u8],
) {
    match status {
        Status::Modified => {
            for &s in aout {
                flags[s as usize] = 0;
            }
        }
        Status::Masked {
            silent: sm,
            constant: cm,
        } => {
            for (c, &s) in aout.iter().enumerate() {
                flags[s as usize] = flag(sm.get(c), cm.get(c) || sm.get(c));
            }
        }
        Status::Silent | Status::Idle => {
            for &s in aout {
                arena.slot_mut(s, frames).fill(0.0);
                flags[s as usize] = SILENT | CONSTANT;
            }
        }
        Status::Constant => {
            for &s in aout {
                let buf = arena.slot_mut(s, frames);
                let v = buf[0];
                buf.fill(v);
                flags[s as usize] = flag(v == 0.0 && v.is_sign_positive(), true);
            }
        }
        Status::Bypass => {
            for (c, &s) in aout.iter().enumerate() {
                match ain.get(c) {
                    Some(_) if in_place.get(c) => {}
                    Some(&i) => {
                        arena.copy_slot(i, s);
                        flags[s as usize] = flags[i as usize];
                    }
                    None => {
                        arena.slot_mut(s, frames).fill(0.0);
                        flags[s as usize] = SILENT | CONSTANT;
                    }
                }
            }
        }
    }
}
