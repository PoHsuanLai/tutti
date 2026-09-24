//! The runtime's audio side: [`Executor`], and the [`Commit`] box that carries
//! an edit to it and the retired state back.
//!
//! Doc 013 §4, phase-1 form. The *shape* is phase 2's: units exist exactly
//! once, in the executor's store; a commit carries only the units that change
//! plus the new plan, inside a [`Retire`] box; [`Executor::apply`] swaps
//! pointers and hands **the same box** back holding the previous plan and the
//! units it removed, so nothing it replaced is freed on its side. Every unit
//! in the store is itself a `Retire`, so a unit dropped inside
//! [`process`](Executor::process) — which marks the thread with
//! [`AudioThread::enter`] — panics in a debug build instead of freeing on the
//! audio thread.
//!
//! What phase 1 does not do yet is put a thread boundary in the middle: here
//! `apply` runs on the caller's thread and is allowed to allocate (it builds
//! the new arena and retunes rings). Phase 2 moves that half to the control
//! side, ships the prepared state in the commit, and delivers the box over an
//! SPSC ring whose return push cannot fail because the control side holds at
//! most [`MAX_IN_FLIGHT`](crate::MAX_IN_FLIGHT) commits (`Editor` enforces
//! it now). Every type crossing that boundary is already `Send` (units,
//! commits) or `Send + Sync` (the plan).
//!
//! # The serial executor
//!
//! Walks [`Plan::ops`] in order. Per block it never allocates (see
//! `tests/rt_no_alloc.rs`), never sub-chunks (a node gets the whole block and
//! its sorted events), and skips a node whose inputs are silent, whose event
//! inputs are empty and whose declared tail has elapsed — writing silence for
//! it instead.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

use tutti_types::{AudioThread, NodeKey, Retire, Samples, Tail};

use crate::arena::{borrow_disjoint, Arena, Role};
use crate::event::{merge_into, Event, EventWriter, SortedEvents};
use crate::io::Io;
use crate::kernels::{AudioRing, EventFifo};
use crate::node::{
    ConstantMask, Cx, Env, InPlaceMask, MaxBlock, Node, Prepare, SilenceMask, Status, Transport,
    MAX_PORTS,
};
use crate::plan::{DelayKey, Delta, FeedbackKey, Op, Plan, UnitIdx};

/// Events one event slot holds per block, unless configured otherwise.
pub const DEFAULT_EVENT_CAPACITY: usize = 512;

/// A plan and the unit changes that go with it — the box the control side
/// sends the audio side, and gets back.
///
/// Before [`Executor::apply`] it holds the new plan and the incoming units;
/// after, the previous plan and the retired units. Either way it is freed on
/// the control side (see [`Editor::reclaim`](crate::Editor::reclaim)).
pub struct Commit {
    plan: Option<Arc<Plan>>,
    delta: Delta,
    incoming: Vec<(UnitIdx, u32, Retire<dyn Node>)>,
    retired: Vec<(NodeKey, Retire<dyn Node>)>,
}

impl Commit {
    /// Assemble a commit from a compiled plan, its delta, and one prepared unit
    /// for every placement the delta inserts or replaces. Control side.
    ///
    /// # Panics
    ///
    /// If `units` does not supply exactly the inserted and replaced keys.
    pub fn new(
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
            plan: Some(Arc::new(plan)),
            delta,
            incoming,
            retired,
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
}

enum Ring {
    Audio(AudioRing),
    Event(EventFifo),
}

/// The serial plan executor.
pub struct Executor {
    prepare: Prepare,
    event_cap: usize,
    plan: Option<Arc<Plan>>,
    store: Vec<Option<Unit>>,
    arena: Arena,
    silent: Vec<bool>,
    constant: Vec<bool>,
    events: Vec<Vec<Event>>,
    rings: Vec<Ring>,
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
    /// An executor with no plan, prepared for `prepare`.
    pub fn new(prepare: Prepare) -> Self {
        Self::with_event_capacity(prepare, DEFAULT_EVENT_CAPACITY)
    }

    /// As [`new`](Self::new), with `cap` events per event slot per block.
    pub fn with_event_capacity(prepare: Prepare, cap: usize) -> Self {
        Self {
            prepare,
            event_cap: cap,
            plan: None,
            store: Vec::new(),
            arena: Arena::new(1, prepare.max_block().get()),
            silent: vec![true],
            constant: vec![true],
            events: Vec::new(),
            rings: Vec::new(),
            frame: 0,
            dropped: 0,
        }
    }

    /// The plan running now.
    pub fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// Frames rendered so far.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Events refused so far: writer overflow, merge overflow, delay overflow.
    pub fn dropped_events(&self) -> u64 {
        self.dropped
    }

    /// Install `commit` and hand the box back holding what it replaced.
    ///
    /// Delay rings, feedback slots and units carry over by key; see the
    /// [module docs](self) for which half of this moves to the control
    /// thread in phase 2.
    pub fn apply(&mut self, mut commit: Retire<Commit>) -> Retire<Commit> {
        let c = &mut *commit;
        let plan = c
            .plan
            .take()
            .expect("a commit carries its plan until applied");

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
            });
        }

        // Rings: carried by key, retuned if their length changed.
        let old_plan = self.plan.take();
        let mut old_rings: BTreeMap<DelayKey, Ring> = match &old_plan {
            Some(p) => p
                .delays
                .iter()
                .map(|d| d.key)
                .zip(std::mem::take(&mut self.rings))
                .collect(),
            None => BTreeMap::new(),
        };
        self.rings = plan
            .delays
            .iter()
            .map(|d| match (d.key, old_rings.remove(&d.key)) {
                (DelayKey::Event { .. }, Some(Ring::Event(mut f))) => {
                    f.retune(d.len);
                    Ring::Event(f)
                }
                (DelayKey::Event { .. }, _) => Ring::Event(EventFifo::new(d.len, self.event_cap)),
                (_, Some(Ring::Audio(mut r))) => {
                    r.retune(d.len);
                    Ring::Audio(r)
                }
                (_, _) => Ring::Audio(AudioRing::new(d.len)),
            })
            .collect();

        // Arena: fresh, with feedback slots carried by key.
        let max_block = self.prepare.max_block().get();
        let mut arena = Arena::new(plan.audio_slots as usize, max_block);
        let mut silent = vec![false; plan.audio_slots as usize];
        let mut constant = vec![false; plan.audio_slots as usize];
        silent[0] = true;
        constant[0] = true;
        let mut events: Vec<Vec<Event>> = (0..plan.event_slots)
            .map(|_| Vec::with_capacity(self.event_cap))
            .collect();
        let old_fb: BTreeMap<FeedbackKey, u32> = old_plan
            .iter()
            .flat_map(|p| p.audio_feedback.iter().chain(&p.event_feedback))
            .map(|f| (f.key, f.slot))
            .collect();
        for f in &plan.audio_feedback {
            match old_fb.get(&f.key) {
                Some(&s) => {
                    arena
                        .slot_mut(f.slot, max_block)
                        .copy_from_slice(self.arena.slot(s, max_block));
                    silent[f.slot as usize] = self.silent[s as usize];
                    constant[f.slot as usize] = self.constant[s as usize];
                }
                None => {
                    silent[f.slot as usize] = true;
                    constant[f.slot as usize] = true;
                }
            }
        }
        for f in &plan.event_feedback {
            if let Some(&s) = old_fb.get(&f.key) {
                let old = &self.events[s as usize];
                events[f.slot as usize].extend_from_slice(old);
            }
        }
        self.arena = arena;
        self.silent = silent;
        self.constant = constant;
        self.events = events;
        self.plan = Some(plan);
        c.plan = old_plan;
        commit
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
        let Self {
            prepare,
            event_cap,
            plan,
            store,
            arena,
            silent,
            constant,
            events,
            rings,
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
        let cap = *event_cap;
        let env = Env {
            frame: *frame,
            sample_rate: prepare.sample_rate(),
            block_len: Samples(frames),
            transport: *transport,
        };

        // Last block's feedback events must fit this block.
        for f in &plan.event_feedback {
            events[f.slot as usize].retain(|e| (e.offset as usize) < frames);
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
                    let Ring::Audio(ring) = &mut rings[delay as usize] else {
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
                    let Ring::Event(fifo) = &mut rings[delay as usize] else {
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
                    let quiet_inputs = in_silent.covers(ain.len())
                        && ein.iter().all(|&s| events[s as usize].is_empty());

                    let skip = (!ain.is_empty() || !ein.is_empty())
                        && quiet_inputs
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
                    };
                    let node = &mut *u.node;
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

                    finish(status, frames, ain, aout, in_place, arena, silent, constant);
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
                            let Ring::Audio(ring) = &mut rings[d as usize] else {
                                unreachable!("output delay on an event ring")
                            };
                            ring.run(s, out, silent[src as usize]);
                        }
                        None => out.copy_from_slice(s),
                    }
                }
                Op::Capture { feedback, src } => {
                    let dst = plan.audio_feedback[feedback as usize].slot;
                    arena.copy_slot(src, dst);
                    // Past this block's end the slot must read silence, in
                    // case the next block is longer.
                    arena.slot_mut(dst, max.get())[frames..].fill(0.0);
                    silent[dst as usize] = silent[src as usize];
                    constant[dst as usize] = silent[src as usize];
                }
                Op::EventCapture { feedback, src } => {
                    let dst = plan.event_feedback[feedback as usize].slot;
                    let (input, output) = event_pair(events, src, dst);
                    output.clear();
                    output.extend_from_slice(input);
                }
            }
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
