//! The runtime's audio side: [`Executor`], and the [`Commit`] box that carries
//! an edit to it and the retired state back.
//!
//! Doc 013 §4, phase-1 form. The *shape* is phase 2's: units exist exactly
//! once, in the executor's store; a commit carries only the units that change
//! plus the new plan, inside a [`Retire`] box; [`Executor::apply`] swaps
//! pointers and hands **the same box** back holding everything it replaced —
//! the previous plan, the retired units, and the previous arena, event slots
//! and delay state — so nothing it replaced is freed on its side. `apply` and
//! [`process`](Executor::process) both mark the thread with
//! [`AudioThread::enter`]: a unit or a commit dropped inside either panics in
//! a debug build instead of freeing on the audio thread.
//!
//! What phase 1 does not do yet is put a thread boundary in the middle: here
//! `apply` runs on the caller's thread and *allocates* (it builds the new
//! arena and delay state). Phase 2 moves that half to the control side, ships
//! the prepared state in the commit, and delivers the box over an SPSC ring
//! whose return push cannot fail because the control side holds at most
//! [`MAX_IN_FLIGHT`](crate::MAX_IN_FLIGHT) commits (`Editor` enforces it
//! now). Every type crossing that boundary is already `Send` (units, commits)
//! or `Send + Sync` (the plan).
//!
//! # The serial executor
//!
//! Walks [`Plan::ops`] in order, under a flush-to-zero guard
//! ([`ScopedNoDenormals`]). Per block it never allocates (see
//! `tests/rt_no_alloc.rs`) and hands every node the **whole block** and its
//! sorted events.
//!
//! **It does not split blocks at a loop wrap.** The whole-block promise is
//! what keeps an out-of-process plugin's declared latency constant, so a
//! transport that loops inside a block is reported through
//! [`Transport::looping`](crate::Transport) and a node that cares computes
//! the wrap position itself.
//!
//! # The silence skip
//!
//! A node is not called — its outputs are written as silence — only when all
//! three hold:
//!
//! 1. its audio inputs are flagged silent and its event inputs are empty;
//! 2. its **previous call left it quiet**: every audio output flagged silent
//!    and no event written (a node never called yet is not quiet);
//! 3. its declared tail has elapsed since its inputs went quiet.
//!
//! (2) is what keeps a held note sounding: a synth fed one note-on has quiet
//! inputs for as long as the note is held, and quiet inputs are not an idle
//! node.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

use tutti_types::{AudioThread, Guarded, NodeKey, Retire, Samples, ScopedNoDenormals, Tail};

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

// `&mut dyn Node` cannot be used to free the unit: an unsized value cannot be
// assigned, swapped or taken out of in safe Rust.
impl Guarded for dyn Node {}

/// A plan and the unit changes that go with it — the box the control side
/// sends the audio side, and gets back.
///
/// Before [`Executor::apply`] it holds the new plan and the incoming units;
/// after, everything the executor replaced. Either way it is freed on the
/// control side (see [`Editor::reclaim`](crate::Editor::reclaim)); dropping it
/// on the audio thread panics in a debug build.
#[must_use = "a commit must be applied, and the box it returns reclaimed"]
pub struct Commit {
    pub(crate) editor: u64,
    pub(crate) applied: bool,
    plan: Option<Arc<Plan>>,
    delta: Delta,
    incoming: Vec<(UnitIdx, u32, Retire<dyn Node>)>,
    retired: Vec<(NodeKey, Retire<dyn Node>)>,
    old_state: Option<State>,
}

// Every field is private to this crate, and `Commit`'s own drop is checked,
// so `&mut Commit` cannot free anything unnoticed on the audio thread.
impl Guarded for Commit {}

impl Drop for Commit {
    fn drop(&mut self) {
        AudioThread::check_not_current("Commit");
    }
}

impl Commit {
    /// Assemble a commit from a compiled plan, its delta, and one prepared unit
    /// for every placement the delta inserts or replaces. Control side.
    ///
    /// A commit built here belongs to no [`Editor`](crate::Editor); the usual
    /// path is [`Editor::commit`](crate::Editor::commit).
    ///
    /// # Panics
    ///
    /// If `units` does not supply exactly the inserted and replaced keys.
    pub fn new(
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node>>,
    ) -> Retire<Commit> {
        Self::for_editor(0, plan, delta, units)
    }

    pub(crate) fn for_editor(
        editor: u64,
        plan: Plan,
        delta: Delta,
        mut units: BTreeMap<NodeKey, Box<dyn Node>>,
    ) -> Retire<Commit> {
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
                (p.idx, p.gen, Retire::from_box(unit))
            })
            .collect();
        assert!(
            units.is_empty(),
            "units supplied for nodes the delta does not place: {:?}",
            units.keys().collect::<Vec<_>>()
        );
        // Reserved here so `apply` moves retirees in without growing.
        let retired = Vec::with_capacity(delta.retire.len() + delta.replace.len());
        Retire::new(Self {
            editor,
            applied: false,
            plan: Some(Arc::new(plan)),
            delta,
            incoming,
            retired,
            old_state: None,
        })
    }

    /// The plan this box carries: the new one before `apply`, the previous
    /// one (if any) after.
    pub fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// The unit changes.
    pub fn delta(&self) -> &Delta {
        &self.delta
    }

    /// Whether an executor has applied it.
    pub fn is_applied(&self) -> bool {
        self.applied
    }

    /// The units `apply` removed, by key.
    pub fn retired(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.retired.iter().map(|(k, _)| *k)
    }
}

struct Unit {
    gen: u32,
    node: Retire<dyn Node>,
    /// Consecutive frames of fully silent input, saturating.
    quiet: u64,
    /// Whether the last call (or skip) left every output silent and wrote no
    /// event. See the module docs on the silence skip.
    last_quiet: bool,
}

enum Ring {
    Audio(AudioRing),
    Event(EventFifo),
}

/// Events flushed from a delay that went away, waiting for their sink's next
/// call. `events` is sized so the sink's own slot events fit after them.
struct Inject {
    unit: u32,
    port: u16,
    events: Vec<Event>,
    live: bool,
}

/// Everything the executor rebuilds on `apply`: the arenas, the delay and
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
/// [`Editor::new`](crate::Editor::new), so the two share one [`Prepare`].
pub struct Executor {
    prepare: Prepare,
    event_cap: usize,
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
    pub(crate) fn new(prepare: Prepare, cap: usize) -> Self {
        Self {
            prepare,
            event_cap: cap,
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

    /// Events refused so far: writer overflow, merge overflow, and delay or
    /// feedback FIFO overflow (which never drops a note-off while anything
    /// else can go).
    pub fn dropped_events(&self) -> u64 {
        self.dropped
    }

    /// Install `commit` and hand the box back holding what it replaced.
    ///
    /// Delay rings, feedback state and units carry over by key (see
    /// [`DelayKey`] and [`FeedbackKey`]). An event delay or event feedback
    /// whose key disappears flushes its pending events to its sink, at offset
    /// 0 of the next block, when the sink survives.
    ///
    /// # Panics
    ///
    /// If the commit was already applied, or its plan was compiled for a
    /// different [`Prepare`] than this executor's.
    #[must_use = "the returned box holds the replaced state; reclaim it on the control side"]
    pub fn apply(&mut self, mut commit: Retire<Commit>) -> Retire<Commit> {
        let _rt = AudioThread::enter();
        let c = commit.get_mut();
        assert!(!c.applied, "a commit is applied once");
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
            });
        }

        let old_plan = self.plan.take();
        let new_state = self.rebuild(&plan, old_plan.as_deref());
        let old_state = std::mem::replace(&mut self.state, new_state);
        c.old_state = Some(old_state);
        self.plan = Some(plan);
        c.plan = old_plan;
        c.applied = true;
        commit
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

        // Feedback delays, by key: exactly `MaxBlock` long.
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
                Some(carried.unwrap_or_else(|| AudioRing::new(Samples(max_block))))
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
                Some(
                    carried.unwrap_or_else(|| EventFifo::sized(Samples(max_block), cap, max_block)),
                )
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
        let mut take_into = |at: EventIn, events: &mut dyn Iterator<Item = Event>| {
            if let Some(u) = sink_of(at) {
                flushed
                    .entry((u, at.port))
                    .or_default()
                    .extend(events.map(|e| Event { offset: 0, ..e }));
            }
        };
        if let Some(op) = old_plan {
            for inj in old.inject.iter().filter(|i| i.live) {
                let at = EventIn {
                    node: op.units[inj.unit as usize].key,
                    port: inj.port,
                };
                take_into(at, &mut inj.events.iter().copied());
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
                    take_into(at, &mut f.pending());
                }
            }
            for (k, &i) in &old_efb {
                if let (FeedbackKey::Event { at, .. }, Some(Some(f))) = (k, old.event_fb.get(i)) {
                    take_into(*at, &mut f.pending());
                }
            }
        }
        let mut has_inject = vec![false; plan.units.len()];
        let inject = flushed
            .into_iter()
            .map(|((unit, port), mut events)| {
                has_inject[unit as usize] = true;
                events.reserve(cap);
                Inject {
                    unit,
                    port,
                    events,
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
            events: (0..plan.event_slots)
                .map(|_| Vec::with_capacity(cap))
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
    /// long. Never allocates, and marks the thread with
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
        let Self {
            prepare,
            event_cap,
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
                    *dropped += merge_into(&ins[..list.len()], output, cap) as u64;
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

                    let skip = (!ain.is_empty() || !ein.is_empty())
                        && quiet_inputs
                        && u.last_quiet
                        && tail_elapsed(pu.shape.tail, u.quiet);
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
                        // Flushed events first (they are older), then this
                        // block's; both sorted, so the result is.
                        for inj in inject.iter_mut().filter(|i| i.live && i.unit == unit) {
                            let slot = ein[inj.port as usize];
                            inj.events.extend_from_slice(&events[slot as usize]);
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
                    let node = u.node.get_mut();
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
                            inj.live = false;
                        }
                        has_inject[unit as usize] = false;
                    }

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
            evin[inj.port as usize] = SortedEvents::trusted(&inj.events, frames);
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
        Status::Silent => {
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
