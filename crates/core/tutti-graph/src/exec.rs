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
use tutti_types::{AudioThread, NodeKey, Samples, ScopedNoDenormals, Tail};

use crate::arena::{borrow_disjoint, Arena, Role};
use crate::event::{merge_into, Event, EventWriter, SortedEvents};
use crate::io::Io;
use crate::kernels::{AudioRing, EventFifo};
use crate::node::{
    ConstantMask, Cx, Env, InPlaceMask, MaxBlock, Node, Prepare, SilenceMask, Status, Transport,
    MAX_PORTS,
};
use crate::plan::{DelayKey, Delta, FeedbackKey, Op, Plan, UnitIdx};
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
    silent: Vec<bool>,
    constant: Vec<bool>,
    events: Vec<Vec<Event>>,
    rings: Vec<Option<Ring>>,
    audio_fb: Vec<Option<AudioRing>>,
    event_fb: Vec<Option<EventFifo>>,
    inject: Vec<Inject>,
    has_inject: Vec<bool>,
}

impl State {
    fn empty(max_block: usize) -> Self {
        Self {
            arena: Arena::new(1, max_block),
            silent: vec![true],
            constant: vec![true],
            events: Vec::new(),
            rings: Vec::new(),
            audio_fb: Vec::new(),
            event_fb: Vec::new(),
            inject: Vec::new(),
            has_inject: Vec::new(),
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
    frame: u64,
    dropped: u64,
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
    ) -> Self {
        Self {
            prepare,
            event_cap: cap,
            queue,
            back,
            plan: None,
            store: Vec::new(),
            state: State::empty(prepare.max_block().get()),
            frame: 0,
            dropped: 0,
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
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Events refused so far: writer overflow, and delay or feedback FIFO
    /// overflow (which never drops a note-off while anything else can go).
    /// A merge never drops: its slot holds all its inputs.
    pub fn dropped_events(&self) -> u64 {
        self.dropped
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
            silent: {
                let mut v = vec![false; plan.audio_slots as usize];
                v[0] = true;
                v
            },
            constant: {
                let mut v = vec![false; plan.audio_slots as usize];
                v[0] = true;
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
        } = self;
        let Some(plan) = plan.as_ref() else {
            for o in outputs.iter_mut() {
                o[..frames].fill(0.0);
            }
            *frame += frames as u64;
            return;
        };
        let State {
            arena,
            silent,
            constant,
            events,
            rings,
            audio_fb,
            event_fb,
            inject,
            has_inject,
        } = state;
        let cap = *event_cap;
        let env = Env {
            frame: *frame,
            sample_rate: prepare.sample_rate(),
            block_len: Samples(frames),
            transport: *transport,
        };

        // Feedback reads happen before any op: the delay is a whole
        // `MaxBlock`, so nothing this block's captures queue is due yet.
        for (f, spec) in audio_fb.iter().zip(&plan.audio_feedback) {
            let ring = f.as_ref().expect("built by apply");
            let is_silent = ring.peek_oldest(arena.slot_mut(spec.slot, frames));
            silent[spec.slot as usize] = is_silent;
            constant[spec.slot as usize] = is_silent;
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
                    silent[dst as usize] = false;
                    constant[dst as usize] = false;
                }
                Op::Delay { delay, src, dst } => {
                    let Some(Ring::Audio(ring)) = &mut rings[delay as usize] else {
                        unreachable!("audio delay on an event ring")
                    };
                    let src_silent = silent[src as usize];
                    let out_silent = if src == dst {
                        ring.run_in_place(arena.slot_mut(dst, frames), src_silent)
                    } else {
                        let (s, d) = arena.pair(src, dst, frames);
                        ring.run(s, d, src_silent)
                    };
                    silent[dst as usize] = out_silent;
                    constant[dst as usize] = out_silent;
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
                Op::Node {
                    unit,
                    audio_in,
                    audio_out,
                    event_in,
                    event_out,
                    in_place,
                } => {
                    let pu = &plan.units[unit as usize];
                    let u = store[pu.idx.0 as usize]
                        .as_mut()
                        .expect("the delta placed every unit the plan runs");
                    debug_assert_eq!(u.gen, pu.gen, "unit generation matches the plan");
                    let ain = &plan.audio_list[audio_in.range()];
                    let aout = &plan.audio_list[audio_out.range()];
                    let ein = &plan.event_list[event_in.range()];
                    let eout = &plan.event_list[event_out.range()];
                    let injected = has_inject[unit as usize];

                    let mut in_silent = SilenceMask::NONE;
                    let mut in_constant = ConstantMask::NONE;
                    for (c, &s) in ain.iter().enumerate() {
                        if silent[s as usize] {
                            in_silent = in_silent.with(c);
                        }
                        if constant[s as usize] {
                            in_constant = in_constant.with(c);
                        }
                    }
                    let quiet_inputs = !injected
                        && in_silent.covers(ain.len())
                        && ein.iter().all(|&s| events[s as usize].is_empty());

                    // See the module docs: a node with event inputs parks only
                    // on its own say-so (`Status::Idle`); one without parks
                    // when its inputs, its last output and its tail agree.
                    let skip = quiet_inputs
                        && if ein.is_empty() {
                            !ain.is_empty() && u.last_quiet && tail_elapsed(pu.shape.tail, u.quiet)
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
                            arena.slot_mut(s, frames).fill(0.0);
                            silent[s as usize] = true;
                            constant[s as usize] = true;
                        }
                        for &s in eout {
                            events[s as usize].clear();
                        }
                        continue;
                    }

                    if injected {
                        // Flushed events keep their spacing where it fits in
                        // this block; what does not is clamped to its last
                        // frame. Merged with this block's own events, ties
                        // to the flushed ones (they are older).
                        for inj in inject.iter_mut().filter(|i| i.live && i.unit == unit) {
                            for e in &mut inj.events {
                                e.offset = e.offset.min(frames as u32 - 1);
                            }
                            let slot = ein[inj.port as usize];
                            inj.merged.clear();
                            let cap_total = inj.merged.capacity();
                            merge_into(
                                &[&inj.events, &events[slot as usize]],
                                &mut inj.merged,
                                cap_total,
                            );
                        }
                    }

                    let call = NodeCall {
                        cx: Cx {
                            env: &env,
                            arrival: pu.arrival,
                        },
                        max,
                        frames,
                        ain,
                        aout,
                        ein,
                        eout,
                        in_place,
                        silent: in_silent,
                        constant: in_constant,
                        cap,
                        unit,
                        inject: if injected { inject.as_slice() } else { &[] },
                    };
                    let node: &mut dyn Node = &mut *u.node;
                    // Port tables are stack arrays; pick the smallest bucket
                    // that fits so a two-port node does not initialise 128
                    // entries per call. Audio buckets count inputs + outputs
                    // (the borrow requests share one table), event buckets
                    // likewise.
                    let a = ain.len() + aout.len();
                    let e = ein.len() + eout.len();
                    let (status, drops) = match (a, e) {
                        (0..=4, 0) => call.run::<4, 0>(node, arena, events),
                        (0..=4, 1..=4) => call.run::<4, 4>(node, arena, events),
                        (0..=16, 0) => call.run::<16, 0>(node, arena, events),
                        (0..=16, 1..=4) => call.run::<16, 4>(node, arena, events),
                        _ => call.run::<{ 2 * MAX_PORTS }, { 2 * MAX_PORTS }>(node, arena, events),
                    };
                    *dropped += drops as u64;
                    if injected {
                        for inj in inject.iter_mut().filter(|i| i.unit == unit) {
                            inj.events.clear();
                            inj.merged.clear();
                            inj.live = false;
                        }
                        has_inject[unit as usize] = false;
                    }

                    u.last_idle = status == Status::Idle;
                    finish(status, frames, ain, aout, in_place, arena, silent, constant);
                    u.last_quiet = aout.iter().all(|&s| silent[s as usize])
                        && eout.iter().all(|&s| events[s as usize].is_empty());
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
                            ring.run(s, out, silent[src as usize]);
                        }
                        None => out.copy_from_slice(s),
                    }
                }
                Op::Capture { feedback, src } => {
                    let ring = audio_fb[feedback as usize]
                        .as_mut()
                        .expect("built by apply");
                    ring.push(arena.slot(src, frames), silent[src as usize]);
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
        *frame += frames as u64;
    }
}

/// One node call's inputs, gathered so the port tables can be sized by a
/// const bucket (`run::<A, E>`) instead of always by `MAX_PORTS`.
struct NodeCall<'p, 'e> {
    cx: Cx<'e>,
    max: MaxBlock,
    frames: usize,
    ain: &'p [u32],
    aout: &'p [u32],
    ein: &'p [u32],
    eout: &'p [u32],
    in_place: InPlaceMask,
    silent: SilenceMask,
    constant: ConstantMask,
    cap: usize,
    unit: u32,
    inject: &'p [Inject],
}

impl NodeCall<'_, '_> {
    /// Borrow the node's buffers out of the arenas and run it. `A` must hold
    /// `ain.len() + aout.len()` entries and `E` the event ports likewise.
    /// Returns the node's status and how many events its writers refused.
    fn run<const A: usize, const E: usize>(
        &self,
        node: &mut dyn Node,
        arena: &mut Arena,
        events: &mut [Vec<Event>],
    ) -> (Status, u32) {
        let (frames, ain, aout, ein, eout) =
            (self.frames, self.ain, self.aout, self.ein, self.eout);
        debug_assert!(ain.len() + aout.len() <= A && ein.len() + eout.len() <= E);

        let mut reqs = [(0u32, Role::Read(0)); A];
        let mut n = 0;
        for (c, &s) in ain.iter().enumerate() {
            if !self.in_place.get(c) {
                reqs[n] = (s, Role::Read(c as u8));
                n += 1;
            }
        }
        for (c, &s) in aout.iter().enumerate() {
            reqs[n] = (s, Role::Write(c as u8));
            n += 1;
        }
        let mut ins: [&[f32]; A] = [&[]; A];
        let mut outs: [&mut [f32]; A] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(frames, &mut reqs[..n], &mut ins, &mut outs);

        let mut ereqs = [(0u32, Role::Read(0)); E];
        let mut n = 0;
        for (c, &s) in ein.iter().enumerate() {
            ereqs[n] = (s, Role::Read(c as u8));
            n += 1;
        }
        for (c, &s) in eout.iter().enumerate() {
            ereqs[n] = (s, Role::Write(c as u8));
            n += 1;
        }
        let mut evin: [SortedEvents<'_>; E] = [SortedEvents::EMPTY; E];
        let mut evout_bufs: [Option<&mut Vec<Event>>; E] = std::array::from_fn(|_| None);
        borrow_disjoint(
            events,
            1,
            &mut ereqs[..n],
            |port, v| evin[port as usize] = SortedEvents::trusted(&v[0], frames),
            |port, v| evout_bufs[port as usize] = Some(&mut v[0]),
        );
        for inj in self.inject.iter().filter(|i| i.live && i.unit == self.unit) {
            evin[inj.port as usize] = SortedEvents::trusted(&inj.merged, frames);
        }
        let drops = Cell::new(0u32);
        let mut evout: [EventWriter<'_>; E] = std::array::from_fn(|_| EventWriter::detached());
        for (w, b) in evout.iter_mut().zip(evout_bufs.iter_mut()).take(eout.len()) {
            let b = b.take().expect("event output borrowed");
            b.clear();
            *w = EventWriter::new(b, self.cap, frames as u32, &drops);
        }
        let io = Io::new(
            self.max,
            frames,
            &ins[..ain.len()],
            &mut outs[..aout.len()],
            self.silent,
            self.constant,
            self.in_place,
            &evin[..ein.len()],
            &mut evout[..eout.len()],
        );
        let status = node.process(&self.cx, io);
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

/// Apply a node's [`Status`] to its output slots and their masks.
#[allow(clippy::too_many_arguments)]
fn finish(
    status: Status,
    frames: usize,
    ain: &[u32],
    aout: &[u32],
    in_place: InPlaceMask,
    arena: &mut Arena,
    silent: &mut [bool],
    constant: &mut [bool],
) {
    match status {
        Status::Modified => {
            for &s in aout {
                silent[s as usize] = false;
                constant[s as usize] = false;
            }
        }
        Status::Masked {
            silent: sm,
            constant: cm,
        } => {
            for (c, &s) in aout.iter().enumerate() {
                silent[s as usize] = sm.get(c);
                constant[s as usize] = cm.get(c) || sm.get(c);
            }
        }
        Status::Silent | Status::Idle => {
            for &s in aout {
                arena.slot_mut(s, frames).fill(0.0);
                silent[s as usize] = true;
                constant[s as usize] = true;
            }
        }
        Status::Constant => {
            for &s in aout {
                let buf = arena.slot_mut(s, frames);
                let v = buf[0];
                buf.fill(v);
                silent[s as usize] = v == 0.0 && v.is_sign_positive();
                constant[s as usize] = true;
            }
        }
        Status::Bypass => {
            for (c, &s) in aout.iter().enumerate() {
                match ain.get(c) {
                    Some(_) if in_place.get(c) => {}
                    Some(&i) => {
                        arena.copy_slot(i, s);
                        silent[s as usize] = silent[i as usize];
                        constant[s as usize] = constant[i as usize];
                    }
                    None => {
                        arena.slot_mut(s, frames).fill(0.0);
                        silent[s as usize] = true;
                        constant[s as usize] = true;
                    }
                }
            }
        }
    }
}
