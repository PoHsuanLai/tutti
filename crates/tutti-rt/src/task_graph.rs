//! [`TaskGraph`]: a DAG of tasks run once per block by any number of
//! participating threads — activation counters, a ready list, and the first
//! ready successor run inline.
//!
//! # The protocol
//!
//! Each task has a counter holding how many of its predecessors have not run
//! yet this block. [`begin`](TaskGraph::begin) (with `&mut`, so between
//! blocks) lists the tasks with no predecessor as ready. Each participant
//! then loops in [`work`](TaskGraph::work): take a ready task, run it,
//! decrement each successor's counter; a successor whose counter reaches
//! zero is ready. The **first** such successor is run next by the same
//! participant without going through the list (its inputs are hot in this
//! core's cache, and no other participant needs to wake for it); the rest
//! are pushed. A participant leaves when every task of the block has run.
//!
//! [`begin`](TaskGraph::begin) re-arms every counter and clears the ready
//! list: one pass over the tasks per block, on the audio thread before any
//! helper is let in. The ready list needs the pass anyway, and one reset in
//! one place is also right after an aborted block.
//!
//! # The ready list
//!
//! Every task becomes ready at most once per block, so the list is a plain
//! array of one entry per task, filled front to back: a push reserves an
//! index with one `fetch_add` and stores the task there; a pop
//! compare-exchanges the head forward past an index whose entry has been
//! stored. A pop that finds the head's entry reserved but not yet stored
//! reports nothing and the caller tries again — the pusher is between two
//! instructions. (If the pusher is descheduled right there, the entries
//! behind it wait too, even when they are stored: a FIFO's head-of-line
//! cost, bounded by one preemption, which the decision below accepts.) No
//! allocation, no lock, and one shared cache line per counter pair
//! (`head`, `tail`) padded apart.
//!
//! This is a shared FIFO, not per-worker LIFO deques with stealing: with chains already fused into one task each and
//! the first ready successor run inline, a block's graph dispatches tens to
//! hundreds of tasks, not the millions work stealing is built for, and one
//! list keeps the model small enough to check exhaustively.
//!
//! # Ordering
//!
//! A task's writes happen-before its successors' reads: the counter
//! decrement is `AcqRel` (so the decrement that reaches zero has acquired
//! every earlier one's release), a push stores `Release` and a pop loads
//! `Acquire`, and a successor run inline is in the same thread's program
//! order.
//!
//! Every task's writes happen-before `work` returning on any participant,
//! and only the **sinks** (tasks with no successor) are counted for it:
//! `remaining` starts at the sink count, and a sink decrements it
//! (`Release`) after it runs. A participant returns once it loads 0
//! (`Acquire`). The argument:
//!
//! 1. Every task `t` has a sink descendant `s`: follow any successor from
//!    `t`; the graph is finite and acyclic (`new` refuses a cycle), so the
//!    walk ends at a task with none.
//! 2. Along that path each step is the ordering above: the task's run, then
//!    its `AcqRel` decrement of the next task's counter, which the decrement
//!    that reaches zero acquires (the counter's RMWs form one release
//!    sequence), then the next task's run on that thread or, through the
//!    push's `Release` and the pop's `Acquire`, on another. So `t`'s writes
//!    happen-before `s` runs, and before `s`'s `Release` decrement of
//!    `remaining`.
//! 3. `remaining` only ever changes by RMW after `begin`, so a load that
//!    reads 0 reads the last of them and synchronises with every sink's
//!    `Release` decrement. So every sink, and by 2 every task, happens-before
//!    the load, and before `work` returns.
//!
//! Counting every task would make each one an RMW on the one line every
//! participant polls; counting the sinks leaves that line to the few tasks
//! that end the block. An aborted block still reports not done
//! ([`done`](TaskGraph::done)): a sink can only run after every task it
//! descends from, so `remaining` reaches 0 only when every task has run.
//! The `loom` model in `tests/task_graph_loom.rs` checks both properties
//! against the shipped code, the second on a graph whose tasks read nothing,
//! so only the counted sinks order a task before the return.
//!
//! # Waiting
//!
//! A participant with nothing to take while tasks are still running spins.
//! The audio thread ([`Participant::Caller`]) waits until the block is done:
//! it spins, and on Linux, once no task has finished anywhere for
//! [`BlockWaits::caller_yield_after`] (250 µs by default), it yields its
//! core once and spins again (`SpinThenYield`, `src/spin.rs`). A healthy
//! block rarely waits that long without a task finishing, so its wait is
//! the plain spin. `remaining` counts only sinks, so it is not that signal:
//! each helper also counts the tasks it finishes on a counter of its own
//! (`progress`, a cache line per helper, a plain `Relaxed` load and store
//! by its one writer), the caller counts its own, and the waiting caller
//! reads the helpers' counters once per `PROGRESS_EVERY` waits. That costs
//! a helper no RMW on a shared line, and a line transfer only when a
//! waiting caller has just read it. The yield is for a helper queued behind
//! the caller on the caller's own core: under `SCHED_FIFO`, which does not
//! time-slice, a caller that only spun would keep it off that core until
//! the kernel migrated it, or for good. Under `SCHED_FIFO` or `SCHED_RR`
//! the yield returns at once when no thread of the caller's priority is
//! ready on its core; under `SCHED_OTHER` it may give the core to another
//! task for a slice, which is why only a stuck wait yields. Elsewhere than
//! Linux the caller only spins (`src/spin.rs`, "Only on Linux").
//! A [`Participant::Helper`] with nothing to take spins for at most
//! [`BlockWaits::helper_leave_after`] (15 µs by default; `SpinThenLeave`)
//! and then **leaves** `work`, forfeiting the rest of the block, rather than
//! yielding: the caller waits for every helper still inside the job before
//! the block ends, so a helper that yielded the CPU while inside it would
//! make the audio thread wait on the scheduler. Its idle backoff (yield,
//! park) happens outside the job, in the pool.
//!
//! Both budgets are the pool's ([`BlockWaits`]): `work` takes them per call,
//! so one graph can run on any pool.

use sync::{AtomicBool, AtomicU32, Ordering};

use super::spin::{BlockWaits, SpinThenLeave, SpinThenYield};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
}

/// An empty ready-list entry.
const EMPTY: u32 = u32::MAX;

/// Helpers' completion counters ([`TaskGraph`]'s `progress`): helper `i`
/// counts on slot `i % PROGRESS_SLOTS`.
const PROGRESS_SLOTS: usize = 8;

/// Waits between two reads of the helpers' completion counters by a waiting
/// caller: each read takes the counters' lines from their helpers, so it is
/// rare next to the caller's budget, 250 µs by default (a wait is 10 to
/// 100 ns, see `src/spin.rs`'s `CLOCK_EVERY`). Under loom, every wait.
const PROGRESS_EVERY: u32 = if cfg!(loom) { 1 } else { 64 };

/// An atomic on a cache line pair of its own (128 bytes: adjacent-line
/// prefetch pulls lines in pairs on x86), so counters different threads hit
/// do not bounce one line between them.
#[repr(align(128))]
struct Padded<T>(T);

/// Who is running [`TaskGraph::work`], which decides how it waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Participant {
    /// The audio thread: spins while it waits, and on Linux yields its core
    /// once per [`BlockWaits::caller_yield_after`] in which no task finished
    /// (see the module docs, "Waiting").
    Caller,
    /// A worker thread, by its index among the participants (the pool's
    /// `1..`; it picks the completion counter the caller watches): spins for
    /// at most [`BlockWaits::helper_leave_after`] with nothing to take, then
    /// leaves the block to the others.
    Helper(usize),
}

/// A DAG of tasks, run once per block. See the module docs
/// (`src/task_graph.rs`).
pub struct TaskGraph {
    /// Predecessor count per task.
    initial: Box<[u32]>,
    /// Predecessors not yet run this block, per task.
    pending: Box<[Padded<AtomicU32>]>,
    /// Successors, as CSR rows.
    offsets: Box<[u32]>,
    targets: Box<[u32]>,
    /// Tasks with no predecessor, in task order.
    roots: Box<[u32]>,
    /// Tasks with no successor.
    sinks: u32,
    ready: Box<[AtomicU32]>,
    head: Padded<AtomicU32>,
    tail: Padded<AtomicU32>,
    /// Sinks not yet finished this block (module docs, "Ordering").
    remaining: Padded<AtomicU32>,
    /// Set by [`abort`](Self::abort): every participant leaves.
    aborted: AtomicBool,
    /// Tasks each helper has finished, ever (wrapping), one line per slot:
    /// the waiting caller's progress signal (module docs, "Waiting"). Never
    /// reset, so a new block needs no pass over them.
    progress: Box<[Padded<AtomicU32>]>,
}

impl TaskGraph {
    /// A graph of `initial.len()` tasks: task `t` has `initial[t]`
    /// predecessors and successors `targets[offsets[t]..offsets[t + 1]]`.
    /// Allocates: the executor builds one when it applies a plan, in the
    /// commit that already allocates the plan's state.
    ///
    /// # Panics
    ///
    /// If the rows do not match the task count, a successor is out of range,
    /// a task's predecessor count disagrees with the rows naming it, or the
    /// rows have a cycle — each a graph whose blocks could never finish.
    #[must_use]
    pub fn new(initial: &[u32], offsets: &[u32], targets: &[u32]) -> Self {
        let n = initial.len();
        assert!(n < EMPTY as usize, "too many tasks");
        assert_eq!(offsets.len(), n + 1, "one successor row per task");
        let mut named = vec![0u32; n];
        for &t in targets {
            assert!((t as usize) < n, "successor {t} of {n} tasks");
            named[t as usize] += 1;
        }
        assert_eq!(named, initial, "predecessor counts disagree with the rows");
        // Kahn: a cycle would leave its tasks waiting forever, and the audio
        // thread spinning with them. Refused here, where it costs a pass on
        // the control side, rather than found on the audio thread.
        let mut left = named;
        let mut ready: Vec<u32> = (0..n as u32).filter(|&t| left[t as usize] == 0).collect();
        let mut out = 0;
        while let Some(t) = ready.pop() {
            out += 1;
            for &s in &targets[offsets[t as usize] as usize..offsets[t as usize + 1] as usize] {
                left[s as usize] -= 1;
                if left[s as usize] == 0 {
                    ready.push(s);
                }
            }
        }
        assert_eq!(out, n, "the task graph has a cycle");
        Self {
            initial: initial.into(),
            pending: initial.iter().map(|&c| Padded(AtomicU32::new(c))).collect(),
            offsets: offsets.into(),
            targets: targets.into(),
            roots: (0..n as u32)
                .filter(|&t| initial[t as usize] == 0)
                .collect(),
            sinks: (0..n).filter(|&t| offsets[t] == offsets[t + 1]).count() as u32,
            ready: (0..n).map(|_| AtomicU32::new(EMPTY)).collect(),
            head: Padded(AtomicU32::new(0)),
            tail: Padded(AtomicU32::new(0)),
            remaining: Padded(AtomicU32::new(0)),
            aborted: AtomicBool::new(false),
            progress: (0..PROGRESS_SLOTS)
                .map(|_| Padded(AtomicU32::new(0)))
                .collect(),
        }
    }

    /// Tasks.
    pub fn len(&self) -> usize {
        self.initial.len()
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.initial.is_empty()
    }

    /// Tasks with no predecessor: how many participants can start at once.
    pub fn roots(&self) -> usize {
        self.roots.len()
    }

    /// Arm the next block: every task pending, the roots ready. `&mut`, so
    /// no participant of the previous block is still inside
    /// [`work`](Self::work). No allocation.
    pub fn begin(&mut self) {
        for (p, &c) in self.pending.iter().zip(self.initial.iter()) {
            p.0.store(c, Ordering::Relaxed);
        }
        for r in self.ready.iter() {
            r.store(EMPTY, Ordering::Relaxed);
        }
        for (r, &t) in self.ready.iter().zip(self.roots.iter()) {
            r.store(t, Ordering::Relaxed);
        }
        self.head.0.store(0, Ordering::Relaxed);
        self.tail
            .0
            .store(self.roots.len() as u32, Ordering::Relaxed);
        self.remaining.0.store(self.sinks, Ordering::Relaxed);
        self.aborted.store(false, Ordering::Relaxed);
    }

    /// Stop the block: every participant leaves [`work`](Self::work) at its
    /// next task boundary, whatever is still pending. For a participant
    /// that caught a panic in a task, so the others do not wait for work
    /// that will never be done.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
    }

    /// Whether [`abort`](Self::abort) was called this block.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Whether every task of the block has run.
    pub fn done(&self) -> bool {
        self.remaining.0.load(Ordering::Acquire) == 0
    }

    fn push(&self, t: u32) {
        let i = self.tail.0.fetch_add(1, Ordering::Relaxed);
        // Each task is pushed at most once per block (its counter reaches
        // zero once), so `i` stays below the task count.
        self.ready[i as usize].store(t, Ordering::Release);
    }

    fn pop(&self) -> Option<u32> {
        loop {
            let h = self.head.0.load(Ordering::Relaxed);
            if h >= self.tail.0.load(Ordering::Relaxed) {
                return None;
            }
            let t = self.ready[h as usize].load(Ordering::Acquire);
            if t == EMPTY {
                // Reserved, not yet stored: the pusher is mid-push.
                return None;
            }
            if self
                .head
                .0
                .compare_exchange_weak(h, h + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Some(t);
            }
        }
    }

    /// Participate in the block: take ready tasks and hand each to `run`,
    /// which gets the task index. A [`Participant::Caller`] returns when
    /// every task has run or the block is aborted; a [`Participant::Helper`]
    /// may also return earlier, after [`BlockWaits::helper_leave_after`]
    /// with nothing to take. `waits` are the pool's budgets (the module
    /// docs, "Waiting"). Any number of participants, including one caller,
    /// runs every task exactly once, each after all its predecessors.
    pub fn work(&self, who: Participant, waits: BlockWaits, mut run: impl FnMut(u32)) {
        // Helper: its wait with nothing to take.
        let mut idle = SpinThenLeave::new(waits.helper_leave_after);
        // Caller: its wait, on every task finishing anywhere: the sinks'
        // `remaining`, its own tasks, and the helpers' counters, read once
        // per `PROGRESS_EVERY` waits.
        let mut caller = SpinThenYield::new(u32::MAX, waits.caller_yield_after);
        let (mut mine, mut theirs, mut polls) = (0u32, 0u32, 0u32);
        // Helper: its completion counter.
        let progress = match who {
            Participant::Caller => None,
            Participant::Helper(i) => Some(&self.progress[i % PROGRESS_SLOTS].0),
        };
        let mut next = None;
        loop {
            if self.aborted.load(Ordering::Relaxed) {
                return;
            }
            let t = match next.take().or_else(|| self.pop()) {
                Some(t) => t,
                None => {
                    let left = self.remaining.0.load(Ordering::Acquire);
                    if left == 0 {
                        return;
                    }
                    match who {
                        Participant::Caller => {
                            if polls == 0 {
                                theirs = self
                                    .progress
                                    .iter()
                                    .fold(0u32, |n, p| n.wrapping_add(p.0.load(Ordering::Relaxed)));
                            }
                            // A compare and reset, not `% PROGRESS_EVERY`: under
                            // loom the constant is 1, which clippy rejects.
                            polls += 1;
                            if polls == PROGRESS_EVERY {
                                polls = 0;
                            }
                            caller.wait(left.wrapping_add(mine).wrapping_add(theirs));
                        }
                        Participant::Helper(_) => {
                            if !idle.wait() {
                                return;
                            }
                        }
                    }
                    continue;
                }
            };
            idle.reset();
            run(t);
            // Progress for a waiting caller. The one writer of its slot
            // (unless more than `PROGRESS_SLOTS` helpers share one, when a
            // lost count only delays a yield), on a line of its own: a
            // plain load and store, no RMW on a shared line.
            match progress {
                Some(p) => p.store(p.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed),
                None => mine = mine.wrapping_add(1),
            }
            let row = self.offsets[t as usize] as usize..self.offsets[t as usize + 1] as usize;
            if row.is_empty() {
                self.remaining.0.fetch_sub(1, Ordering::Release);
                continue;
            }
            for &s in &self.targets[row] {
                if self.pending[s as usize].0.fetch_sub(1, Ordering::AcqRel) == 1 {
                    if next.is_none() {
                        next = Some(s);
                    } else {
                        self.push(s);
                    }
                }
            }
        }
    }
}

impl std::fmt::Debug for TaskGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskGraph")
            .field("tasks", &self.len())
            .field("roots", &self.roots.len())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
