//! The parallel executor: a block's tasks run across a [`Pool`]'s threads.
//!
//! Doc 013 §4, "Executor, parallel", and Phase 6. The plan already carries
//! everything a schedule needs, computed by the pure compiler: the ops fused
//! into **tasks** (chains run back to back on one thread), the task DAG as
//! CSR successor rows, each task's **activation count** (its distinct
//! predecessor tasks) and the DAG's widest level ([`Plan::task_width`]). None
//! of it is derived on the audio thread.
//!
//! # Per block
//!
//! 1. The caller (the audio thread) arms the block's [`TaskGraph`] — built
//!    from those tables when the plan was applied — and asks the pool to run
//!    one job on up to `task_width` participants, itself as participant 0.
//! 2. Every participant takes ready tasks and runs each task's ops through
//!    the same op code as the serial walk (`exec.rs`), on **claimed views**
//!    of the arena and event slots (`slots.rs`) and claimed units, delay
//!    rings, feedback state and output channels. The first successor a task
//!    makes ready runs inline on the same thread.
//! 3. The pool returns once every task has run and every helper has left the
//!    job; the caller sums the workers' dropped-event counts and ended fades.
//!
//! # Why the render is bit-identical to the serial one
//!
//! Every op reads only values its DAG predecessors wrote (the compiler's
//! op DAG), every slot is shared only between values whose lifetimes cannot
//! overlap under *any* order the DAG allows (the colouring pass, checked by
//! the verifier), and each unit, ring, feedback delay and output channel is
//! touched by exactly one op. So each op sees the inputs it sees serially,
//! whatever order the workers run it in, and does the same arithmetic. The
//! only cross-op sums (dropped events) are integer counts. The tests in
//! `tests/parallel.rs` hold the parallel render to the serial one, sample for
//! sample, across worker counts and block lengths.
//!
//! A node that shares mutable state with another node *outside* the graph's
//! ports (two nodes behind one `Arc<Mutex<_>>`, say) is not covered: the
//! executor orders only what the graph declares. Doc 013 states this as part
//! of the node contract.
//!
//! # Safety without `unsafe`
//!
//! This crate stays `#![forbid(unsafe_code)]`. The claims come from
//! `tutti_types::{SplitRw, SplitMut}`, whose borrows are checked at run time:
//! if two ops that share a slot ever ran at once (a compiler bug), the second
//! claim panics — it is never a second reference. The task dispatch
//! (`tutti_types::TaskGraph`) and the pool handshake
//! (`tutti_types::JobGate`, used by `tutti_core::WorkerPool`) are loom-checked
//! there.
//!
//! # Real time
//!
//! Per block the parallel path allocates nothing, takes no lock and makes no
//! system call on the audio thread except the pool's wake of its sleeping
//! helpers: the claim tables, the ready list and each worker's scratch (its
//! claim lists, its overlay buffer) are sized when a plan is applied. The
//! caller never waits for a helper that has not started; it waits only for
//! work a started helper is finishing. See `tests/rt_no_alloc.rs`.
//!
//! # Panics on a worker
//!
//! A node that panics on any participant aborts the block's task graph (so
//! the others stop taking tasks), and the caller panics once every
//! participant has left, naming the cause. The node's own panic message has
//! already been printed by the panic hook.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use tutti_types::{
    AudioThread, ClaimTable, Held, Participant, ScopedNoDenormals, SplitMut, SplitRw, TaskGraph,
};

use crate::arena::{Arena, Line};
use crate::command::Deliveries;
use crate::event::Event;
use crate::exec::{
    audio_ring, capture_op, delay_op, event_delay_op, event_merge_op, event_ring, global_in,
    node_dispatch, output_op, Head, Inject, OpState, Ring, Unit,
};
use crate::node::{Env, MaxBlock, MAX_PORTS};
use crate::param::MAX_PARAM_SOURCES;
use crate::plan::{Op, Plan, Span};
use crate::slots::{ArenaView, EventSlots, EventsView};

/// A pool of threads that runs one job at a time on several of them — what
/// the parallel executor asks of a worker pool. `tutti_core::WorkerPool`
/// implements it; a host with its own pool (or a test) can too.
///
/// # Contract
///
/// [`run`](Self::run) calls `job(0)` on the calling thread and `job(i)` on at
/// most [`participants`](Self::participants)` - 1` other threads, each `i`
/// distinct and in `1..participants`, and returns only once no call of `job`
/// is still running. It may run `job(0)` alone — when its threads are busy
/// with another caller's job, say — and the executor's job is correct with
/// any number of participants, one included. On the audio thread it must
/// not allocate, lock or wait for a thread that has not started the job;
/// waking sleeping threads is the one system call it may make.
pub trait Pool: Send + Sync + 'static {
    /// Participants a job can have, the caller included. At least 1.
    fn participants(&self) -> usize;

    /// Run `job` as described in the trait docs. `wake` is how many
    /// participants (the caller included) the job can keep busy at once: a
    /// pool need not wake more threads than that.
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync));
}

impl std::fmt::Debug for dyn Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("participants", &self.participants())
            .finish()
    }
}

/// One participant's scratch, sized when the plan is applied.
pub(crate) struct Worker {
    /// The audio slots an op's view holds.
    held_audio: Held,
    /// The event slots an op's view holds.
    held_events: Held,
    /// Where a node's scheduled events are merged into its inputs.
    overlay: Vec<Event>,
    /// Events refused this block.
    dropped: u64,
    /// Whether a crossfade ended this block.
    fade_ended: bool,
}

/// The parallel executor's per-plan state: the task graph and every claim
/// table, built when the plan is applied (with the rest of the executor's
/// state, `exec.rs`'s `rebuild`) for the pool's participant count.
pub(crate) struct Par {
    graph: TaskGraph,
    audio: ClaimTable,
    events: ClaimTable,
    units: ClaimTable,
    rings: ClaimTable,
    audio_fb: ClaimTable,
    event_fb: ClaimTable,
    inject: ClaimTable,
    /// Per plan unit, its run of the (unit-sorted) inject list.
    inject_spans: Vec<Span>,
    has_inject: ClaimTable,
    has_due: ClaimTable,
    outputs: ClaimTable,
    workers: Vec<Worker>,
    worker_claims: ClaimTable,
}

impl Par {
    /// For `plan`, with `inject` the executor's (unit-sorted) flushed-event
    /// list, `store_len` its unit store's length and `overlay` the overlay
    /// capacity one node call may need. Allocates.
    pub(crate) fn new(
        plan: &Plan,
        participants: usize,
        store_len: usize,
        inject_units: impl Iterator<Item = u32>,
        overlay: usize,
    ) -> Self {
        let graph = TaskGraph::new(
            &plan.task_activation,
            &plan.task_succ.offsets,
            &plan.task_succ.targets,
        );
        let units = plan.units.len();
        let mut inject_spans = vec![Span::default(); units];
        let mut n_inject = 0u32;
        for (i, u) in inject_units.enumerate() {
            let span = &mut inject_spans[u as usize];
            if span.len == 0 {
                span.start = i as u32;
            }
            debug_assert_eq!(span.start + span.len, i as u32, "injects sorted by unit");
            span.len += 1;
            n_inject += 1;
        }
        // What one op's views can hold at once: a node's audio ports and
        // every param source it reads, or an event merge's inputs and output.
        let held = 2 * MAX_PORTS + crate::param::MAX_PARAM_PORTS * MAX_PARAM_SOURCES + 1;
        Self {
            graph,
            audio: ClaimTable::new(plan.audio_slots as usize),
            events: ClaimTable::new(plan.event_slots as usize),
            units: ClaimTable::new(store_len),
            rings: ClaimTable::new(plan.delays.len()),
            audio_fb: ClaimTable::new(plan.audio_feedback.len()),
            event_fb: ClaimTable::new(plan.event_feedback.len()),
            inject: ClaimTable::new(n_inject as usize),
            inject_spans,
            has_inject: ClaimTable::new(units),
            has_due: ClaimTable::new(units),
            outputs: ClaimTable::new(plan.global_outputs()),
            workers: (0..participants.max(1))
                .map(|_| Worker {
                    held_audio: Held::new(held),
                    held_events: Held::new(held),
                    overlay: Vec::with_capacity(overlay),
                    dropped: 0,
                    fade_ended: false,
                })
                .collect(),
            worker_claims: ClaimTable::new(participants.max(1)),
        }
    }
}

/// Everything one block's parallel run reads or claims.
pub(crate) struct Block<'b, 'o> {
    pub(crate) plan: &'b Plan,
    pub(crate) env: &'b Env,
    pub(crate) frames: usize,
    pub(crate) max: MaxBlock,
    pub(crate) cap: usize,
    pub(crate) inputs: &'b [&'b [f32]],
    pub(crate) outputs: &'b mut [&'o mut [f32]],
    pub(crate) arena: &'b mut Arena,
    pub(crate) flags: &'b [AtomicU8],
    pub(crate) events: &'b mut [Vec<Event>],
    pub(crate) store: &'b mut [Option<Unit>],
    pub(crate) rings: &'b mut [Option<Ring>],
    pub(crate) audio_fb: &'b mut [Option<crate::kernels::AudioRing>],
    pub(crate) event_fb: &'b mut [Option<crate::kernels::EventFifo>],
    pub(crate) inject: &'b mut [Inject],
    pub(crate) has_inject: &'b mut [bool],
    pub(crate) has_due: &'b mut [bool],
    pub(crate) due: Deliveries<'b>,
}

/// What every participant shares while the block runs: `Sync`, so the pool
/// can hand `&Shared` to its threads.
struct Shared<'s, 'o> {
    plan: &'s Plan,
    env: &'s Env,
    frames: usize,
    max: MaxBlock,
    cap: usize,
    inputs: &'s [&'s [f32]],
    graph: &'s TaskGraph,
    arena: SplitRw<'s, Line>,
    flags: &'s [AtomicU8],
    events: SplitRw<'s, Vec<Event>>,
    store: SplitMut<'s, Option<Unit>>,
    rings: SplitMut<'s, Option<Ring>>,
    audio_fb: SplitMut<'s, Option<crate::kernels::AudioRing>>,
    event_fb: SplitMut<'s, Option<crate::kernels::EventFifo>>,
    inject: SplitMut<'s, Inject>,
    inject_spans: &'s [Span],
    has_inject: SplitMut<'s, bool>,
    has_due: SplitMut<'s, bool>,
    outputs: SplitMut<'s, &'o mut [f32]>,
    workers: SplitMut<'s, Worker>,
    due: Deliveries<'s>,
}

/// Run one block's ops on `pool`. Returns the events dropped and whether a
/// crossfade ended.
///
/// # Panics
///
/// If a node panicked on any participant (after every participant has left
/// the block).
pub(crate) fn run_block(par: &mut Par, pool: &Arc<dyn Pool>, b: Block<'_, '_>) -> (u64, bool) {
    par.graph.begin();
    for w in &mut par.workers {
        w.dropped = 0;
        w.fade_ended = false;
    }
    let Par {
        graph,
        audio,
        events,
        units,
        rings,
        audio_fb,
        event_fb,
        inject,
        inject_spans,
        has_inject,
        has_due,
        outputs,
        workers,
        worker_claims,
    } = par;
    let aborted = {
        let shared = Shared {
            plan: b.plan,
            env: b.env,
            frames: b.frames,
            max: b.max,
            cap: b.cap,
            inputs: b.inputs,
            graph,
            arena: b.arena.split(audio),
            flags: b.flags,
            events: SplitRw::new(b.events, 1, events),
            store: SplitMut::new(b.store, units),
            rings: SplitMut::new(b.rings, rings),
            audio_fb: SplitMut::new(b.audio_fb, audio_fb),
            event_fb: SplitMut::new(b.event_fb, event_fb),
            inject: SplitMut::new(b.inject, inject),
            inject_spans,
            has_inject: SplitMut::new(b.has_inject, has_inject),
            has_due: SplitMut::new(b.has_due, has_due),
            outputs: SplitMut::new(b.outputs, outputs),
            workers: SplitMut::new(workers, worker_claims),
            due: b.due,
        };
        let wake = b.plan.task_width().clamp(1, pool.participants());
        pool.run(wake, &|w| shared.participate(w));
        shared.graph.aborted() || !shared.graph.done()
    };
    assert!(
        !aborted,
        "a node panicked on a parallel worker (its message is above), or the pool \
         returned before the block's tasks were done"
    );
    workers
        .iter()
        .fold((0, false), |(d, f), w| (d + w.dropped, f || w.fade_ended))
}

impl Shared<'_, '_> {
    /// One participant's share of the block.
    fn participate(&self, w: usize) {
        if w >= self.workers.len() {
            return;
        }
        let _rt = AudioThread::enter();
        // Per participant, per block: a plugin on this thread may have
        // cleared it since the last one (doc 013's pitfalls).
        let _ftz = ScopedNoDenormals::new();
        let Ok(mut ws) = self.workers.try_claim(w) else {
            return;
        };
        let who = if w == 0 {
            Participant::Caller
        } else {
            Participant::Helper
        };
        let r = catch_unwind(AssertUnwindSafe(|| {
            self.graph.work(who, |t| {
                let span = self.plan.tasks[t as usize];
                for &op in &self.plan.task_ops[span.range()] {
                    self.run_op(&self.plan.ops[op as usize], &mut ws);
                }
            });
        }));
        if r.is_err() {
            self.graph.abort();
        }
    }

    /// One op, on claimed state.
    fn run_op(&self, op: &Op, ws: &mut Worker) {
        let Worker {
            held_audio,
            held_events,
            overlay,
            dropped,
            fade_ended,
        } = ws;
        let frames = self.frames;
        let flags = self.flags;
        match *op {
            Op::GlobalIn { channel, dst } => {
                let mut a = ArenaView {
                    view: self.arena.view(held_audio),
                };
                global_in(&mut a, flags, self.inputs, channel, dst, frames);
            }
            Op::Delay { delay, src, dst } => {
                let mut ring = self.rings.claim(delay as usize);
                let mut a = ArenaView {
                    view: self.arena.view(held_audio),
                };
                delay_op(audio_ring(&mut ring), &mut a, flags, src, dst, frames);
            }
            Op::EventDelay { delay, src, dst } => {
                let mut ring = self.rings.claim(delay as usize);
                let mut e = EventsView {
                    view: self.events.view(held_events),
                };
                *dropped += event_delay_op(event_ring(&mut ring), &mut e, src, dst, frames);
            }
            Op::EventMerge { srcs, dst } => {
                let mut e = EventsView {
                    view: self.events.view(held_events),
                };
                *dropped += event_merge_op(&self.plan.event_list[srcs.range()], dst, &mut e);
            }
            Op::Node { unit, .. } => {
                let rec = &self.plan.nodes.recs[unit as usize];
                let mut u = self.store.claim(rec.store as usize);
                let u = u
                    .as_mut()
                    .expect("the delta placed every unit the plan runs");
                let mut has_inject = self.has_inject.claim(unit as usize);
                let mut has_due = self.has_due.claim(unit as usize);
                let span = self.inject_spans[unit as usize];
                let mut inject = self.inject.claim_run(span.range());
                let mut a = ArenaView {
                    view: self.arena.view(held_audio),
                };
                let mut e = EventsView {
                    view: self.events.view(held_events),
                };
                let head = Head {
                    rec,
                    plan: self.plan,
                    frames,
                    max: self.max,
                    cap: self.cap,
                    env: self.env,
                };
                let mut st = OpState {
                    arena: &mut a,
                    flags,
                    events: &mut e,
                    inject: &mut inject,
                    has_inject: &mut has_inject,
                    has_due: &mut has_due,
                    overlay,
                    due: self.due,
                    dropped,
                };
                *fade_ended |= node_dispatch(u, &head, &mut st);
            }
            Op::Output {
                channel,
                src,
                delay,
            } => {
                let mut out = self.outputs.claim(channel as usize);
                let mut ring = delay.map(|d| self.rings.claim(d as usize));
                let a = ArenaView {
                    view: self.arena.view(held_audio),
                };
                output_op(
                    ring.as_mut().map(|r| audio_ring(r)),
                    &mut out[..frames],
                    &a,
                    flags,
                    src,
                );
            }
            Op::Capture { feedback, src } => {
                let mut ring = self.audio_fb.claim(feedback as usize);
                let ring = ring.as_mut().expect("built by apply");
                let a = ArenaView {
                    view: self.arena.view(held_audio),
                };
                capture_op(ring, &a, flags, src, frames);
            }
            Op::EventCapture { feedback, src } => {
                let mut fifo = self.event_fb.claim(feedback as usize);
                let fifo = fifo.as_mut().expect("built by apply");
                let e = EventsView {
                    view: self.events.view(held_events),
                };
                *dropped += fifo.push(e.ev(src)) as u64;
            }
        }
    }
}
