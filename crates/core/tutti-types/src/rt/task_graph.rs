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
//! helper is let in. (supernova resets a task's counter when it is taken
//! instead, to save that pass; here the ready list needs the pass anyway,
//! and one reset in one place is also right after an aborted block.)
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
//! This is a shared FIFO, not the per-worker LIFO deques with stealing that
//! doc 013 §4 sketches: with chains already fused into one task each and
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
//! Every task's writes happen-before [`work`] returning on any participant,
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
//! it spins, and on Linux, once no task has finished anywhere for 250 µs,
//! it yields its core once and spins again (`SpinThenYield`,
//! `src/rt/spin.rs`). A healthy block rarely waits that long without a task
//! finishing, so its wait is the plain spin. `remaining` counts only sinks,
//! so it is not that signal: each helper also counts the tasks it finishes
//! on a counter of its own (`progress`, a cache line per helper, a plain
//! `Relaxed` load and store by its one writer), the caller counts its own,
//! and the waiting caller reads the helpers' counters once per
//! `PROGRESS_EVERY` waits. That costs a helper no RMW on a shared line, and
//! a line transfer only when a waiting caller has just read it. The yield
//! is for a helper queued
//! behind the caller on the caller's own core: under `SCHED_FIFO`, which
//! does not time-slice, a caller that only spun would keep it off that core
//! until the kernel migrated it, or for good. Under `SCHED_FIFO` or
//! `SCHED_RR` the yield returns at once when no thread of the caller's
//! priority is ready on its core; under `SCHED_OTHER` it may give the core
//! to another task for a slice, which is why only a stuck wait yields.
//! Elsewhere than Linux the caller only spins (`src/rt/spin.rs`, "Only on
//! Linux").
//! A [`Participant::Helper`] spins at most `HELPER_SPINS` times in a row and
//! then **leaves** `work`, forfeiting the rest of the block, rather than
//! yielding: the caller waits for every helper still inside the job before
//! the block ends, so a helper that yielded the CPU while inside it would
//! make the audio thread wait on the scheduler. Its idle backoff (yield,
//! park) happens outside the job, in the pool.

use sync::{AtomicBool, AtomicU32, Ordering};

use super::spin::SpinThenYield;

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
}

/// How many waits in a row a [`Participant::Helper`] spins with nothing to
/// take before it leaves [`TaskGraph::work`]. A wait is a ready-list `pop`
/// attempt, a load of `remaining` and a `spin_loop` hint: from about 10 ns
/// to about 100 ns depending on the core and on contention (a `pause` is
/// about 14 ns on the VM measured in doc 013, 50 to 70 ns on Intel since
/// Skylake). So this is roughly 10 to 100 µs: long enough to pick up the
/// successors of a task another participant is finishing, short enough that
/// a helper behind a long critical-path node goes back to its pool. A count,
/// so it differs across cores; a time budget like the caller's
/// (`src/rt/spin.rs`) is the alternative.
const HELPER_SPINS: u32 = if cfg!(loom) { 2 } else { 1024 };

/// An empty ready-list entry.
const EMPTY: u32 = u32::MAX;

/// Helpers' completion counters ([`TaskGraph`]'s `progress`): helper `i`
/// counts on slot `i % PROGRESS_SLOTS`.
const PROGRESS_SLOTS: usize = 8;

/// Waits between two reads of the helpers' completion counters by a waiting
/// caller: each read takes the counters' lines from their helpers, so it is
/// rare next to the caller's 250 µs budget (a wait is 10 to 100 ns, see
/// `HELPER_SPINS`). Under loom, every wait.
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
    /// once per 250 µs in which no task finished (see the module docs,
    /// "Waiting").
    Caller,
    /// A worker thread, by its index among the participants (the pool's
    /// `1..`; it picks the completion counter the caller watches): spins a
    /// bounded number of times, then leaves the block to the others (see
    /// `HELPER_SPINS`).
    Helper(usize),
}

/// A DAG of tasks, run once per block. See the module docs
/// (`src/rt/task_graph.rs`).
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
    pub fn aborted(&self) -> bool {
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
    /// may also return earlier, after `HELPER_SPINS` waits with nothing to
    /// take. Any number of participants, including one caller, runs every
    /// task exactly once, each after all its predecessors.
    pub fn work(&self, who: Participant, mut run: impl FnMut(u32)) {
        // Helper: waits in a row with nothing to take.
        let mut idle = 0u32;
        // Caller: its wait, on every task finishing anywhere: the sinks'
        // `remaining`, its own tasks, and the helpers' counters, read once
        // per `PROGRESS_EVERY` waits.
        let mut caller = SpinThenYield::new(u32::MAX);
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
                            polls = (polls + 1) % PROGRESS_EVERY;
                            caller.wait(left.wrapping_add(mine).wrapping_add(theirs));
                        }
                        Participant::Helper(_) => {
                            idle += 1;
                            if idle > HELPER_SPINS {
                                return;
                            }
                            helper_spin();
                        }
                    }
                    continue;
                }
            };
            idle = 0;
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

/// One step of a helper's wait for work another participant is finishing.
/// A helper never yields inside the block (see the module docs, "Waiting"):
/// it leaves instead, after `HELPER_SPINS` of these.
fn helper_spin() {
    // loom needs a yield to let another thread run.
    #[cfg(loom)]
    loom::thread::yield_now();
    #[cfg(not(loom))]
    std::hint::spin_loop();
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// A diamond with a tail: 0 → {1, 2} → 3 → 4.
    fn diamond() -> TaskGraph {
        TaskGraph::new(&[0, 1, 1, 2, 1], &[0, 2, 3, 4, 5, 5], &[1, 2, 3, 3, 4])
    }

    /// One participant runs every task once, in a topological order, block
    /// after block (`begin` re-arms the counters).
    ///
    /// Mutation: drop the counter store in `begin` → the second block's
    /// counters start at 0 and wrap on the first decrement → nothing past
    /// task 0 is ever ready → the watchdog aborts the second block → fails
    /// (run: fails after 10 s).
    #[test]
    fn one_participant_runs_every_task_in_order() {
        let mut g = diamond();
        for _ in 0..3 {
            g.begin();
            let mut order = Vec::new();
            // A watchdog aborts a block that cannot finish, so a broken
            // reset fails here rather than hanging the run.
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::scope(|s| {
                let g = &g;
                s.spawn(move || {
                    if done_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .is_err()
                    {
                        g.abort();
                    }
                });
                g.work(Participant::Caller, |t| order.push(t));
                let _ = done_tx.send(());
            });
            assert!(g.done(), "the block finished: {order:?}");
            let pos = |t: u32| order.iter().position(|&x| x == t).expect("ran");
            assert_eq!(order.len(), 5, "{order:?}");
            assert!(pos(0) < pos(1) && pos(0) < pos(2));
            assert!(pos(1) < pos(3) && pos(2) < pos(3) && pos(3) < pos(4));
        }
    }

    /// Several threads run every task exactly once, each after its
    /// predecessors, over many blocks of a wide graph.
    ///
    /// Mutation: in `pop`, skip the `EMPTY` check → a reserved slot is taken
    /// before its store and `EMPTY` is run as a task → index out of bounds →
    /// fails (under contention; the loom model catches it deterministically).
    #[test]
    fn threads_run_every_task_once() {
        use std::sync::atomic::AtomicU64;
        // 0 → 1..=32 → 33.
        let n = 34u32;
        let mut initial = vec![1u32; n as usize];
        initial[0] = 0;
        initial[33] = 32;
        let mut offsets = vec![0u32, 32];
        let mut targets: Vec<u32> = (1..=32).collect();
        for _ in 1..=32 {
            targets.push(33);
            offsets.push(targets.len() as u32);
        }
        offsets.push(targets.len() as u32);
        let mut g = TaskGraph::new(&initial, &offsets, &targets);
        for _ in 0..if cfg!(miri) { 4 } else { 200 } {
            g.begin();
            let runs: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
            let seen_all_middle = AtomicU64::new(0);
            std::thread::scope(|s| {
                for w in 0..4 {
                    let (g, runs, seen) = (&g, &runs, &seen_all_middle);
                    s.spawn(move || {
                        let who = if w == 0 {
                            Participant::Caller
                        } else {
                            Participant::Helper(w)
                        };
                        g.work(who, |t| {
                            if t == 33 {
                                let done = (1..=32)
                                    .filter(|&m| {
                                        runs[m].load(std::sync::atomic::Ordering::Relaxed) == 1
                                    })
                                    .count();
                                seen.store(done as u64, std::sync::atomic::Ordering::Relaxed);
                            }
                            runs[t as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        });
                    });
                }
            });
            assert!(runs
                .iter()
                .all(|r| r.load(std::sync::atomic::Ordering::Relaxed) == 1));
            assert_eq!(
                seen_all_middle.load(std::sync::atomic::Ordering::Relaxed),
                32
            );
        }
    }

    /// A helper with nothing to take leaves the block after its spin budget,
    /// while the caller is still inside a task: the caller never waits for a
    /// helper that has nothing to do (so never for one the OS descheduled
    /// while it yielded).
    ///
    /// Mutation: make `wait` return `true` for a helper past its budget (the
    /// old spin-then-yield) → the helper stays until the block is done, which
    /// waits on the caller's task, which waits on the helper → the
    /// `recv_timeout` fails (run: fails after 10 s).
    #[test]
    fn an_idle_helper_leaves_the_block() {
        use std::sync::mpsc;
        use std::time::Duration;
        // 0 → 1: while the caller runs 0, nothing is ready.
        let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
        g.begin();
        let g = &g;
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (left_tx, left_rx) = mpsc::channel();
        let helper_ran = std::thread::scope(|s| {
            let helper = s.spawn(move || {
                go_rx.recv().unwrap();
                let mut n = 0;
                g.work(Participant::Helper(1), |_| n += 1);
                left_tx.send(g.done()).unwrap();
                n
            });
            g.work(Participant::Caller, |t| {
                if t != 0 {
                    return;
                }
                go_tx.send(()).unwrap();
                let Ok(done) = left_rx.recv_timeout(Duration::from_secs(10)) else {
                    // Let the helper out, so the failure is a failure and
                    // not a hang.
                    g.abort();
                    panic!("the helper stayed while the caller held task 0");
                };
                assert!(!done, "it left before the block was done");
            });
            helper.join().unwrap()
        });
        assert!(g.done());
        assert_eq!(helper_ran, 0, "nothing was ready while it waited");
    }

    /// The caller, with nothing to take while a helper holds the only task
    /// in flight, yields its core once its budget passes with no task
    /// finished: what lets a helper queued behind a `SCHED_FIFO` caller on
    /// one core run at all (`src/rt/spin.rs`). The caller's clock is a
    /// stepped one (a quarter of the budget per read), and the helper holds
    /// task 0 until it sees the caller's yield counted, so no timing is
    /// involved; the 10 s bound is only how a missing yield fails rather
    /// than hangs. Linux only, as the yield is.
    ///
    /// Mutation: make `SpinThenYield::wait` spin on every wait (drop its
    /// yield branch) → no yield is counted → the helper gives up after 10 s
    /// and finishes the block → `saw_yield` is false → fails (run: fails
    /// after 10 s).
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn the_caller_yields_while_a_helper_holds_a_task() {
        use std::sync::atomic::AtomicU32;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        static YIELDED: AtomicU32 = AtomicU32::new(0);
        // 0 → 1: while the helper holds 0, the caller has nothing to take.
        let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
        g.begin();
        let g = &g;
        let (held_tx, held_rx) = mpsc::channel();
        let saw_yield = std::thread::scope(|s| {
            let helper = s.spawn(move || {
                let mut saw = false;
                g.work(Participant::Helper(1), |t| {
                    if t != 0 {
                        return;
                    }
                    held_tx.send(()).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while YIELDED.load(std::sync::atomic::Ordering::Acquire) == 0 {
                        if Instant::now() > deadline {
                            return;
                        }
                        std::thread::yield_now();
                    }
                    saw = true;
                });
                saw
            });
            held_rx.recv().unwrap();
            crate::rt::spin::tests::count_yields_into(&YIELDED);
            crate::rt::spin::tests::step_clock(crate::rt::spin::CALLER_YIELD_AFTER / 4);
            g.work(Participant::Caller, |t| {
                panic!("the caller took task {t}, which the helper holds or runs inline")
            });
            helper.join().unwrap()
        });
        assert!(
            saw_yield,
            "the caller never yielded while the helper held task 0"
        );
        assert!(g.done());
    }

    /// Every task a helper finishes, sink or not, moves its completion
    /// counter (slot `i % PROGRESS_SLOTS` for helper `i`), and the caller's
    /// own tasks touch no counter: what the waiting caller reads as
    /// progress, now that `remaining` moves only for sinks.
    ///
    /// Mutations (run): the helper's store dropped → fails (0, not 5); the
    /// slot taken as `0` whatever the helper → fails (slot 1 unmoved).
    #[test]
    fn a_helpers_every_task_moves_its_completion_counter() {
        let slot = |g: &TaskGraph, i: usize| g.progress[i].0.load(Ordering::Relaxed);
        let mut g = diamond();
        g.begin();
        g.work(Participant::Helper(PROGRESS_SLOTS + 1), |_| {});
        assert!(g.done());
        assert_eq!(slot(&g, 1), 5, "five tasks, one of them a sink");
        g.begin();
        g.work(Participant::Caller, |_| {});
        assert!((0..PROGRESS_SLOTS).all(|i| slot(&g, i) == if i == 1 { 5 } else { 0 }));
    }

    /// An abort makes every participant leave with work pending.
    ///
    /// Mutation: drop the `aborted` check in `work` → the participant keeps
    /// running the chain after the abort → fails.
    #[test]
    fn an_abort_ends_the_block() {
        let mut g = diamond();
        g.begin();
        let mut ran = 0;
        g.work(Participant::Caller, |_| {
            ran += 1;
            g.abort();
        });
        assert_eq!(ran, 1);
        assert!(!g.done());
        g.begin();
        assert!(!g.aborted());
        let mut ran = 0;
        g.work(Participant::Caller, |_| ran += 1);
        assert_eq!(ran, 5);
    }

    /// A cyclic graph is refused: its tasks would wait on each other forever.
    ///
    /// Mutation: drop the Kahn pass in `new` → built → the `should_panic`
    /// fails.
    #[test]
    #[should_panic(expected = "cycle")]
    fn a_cycle_is_refused() {
        // 0 → 1 → 2 → 1.
        TaskGraph::new(&[0, 2, 1], &[0, 1, 2, 3], &[1, 2, 1]);
    }

    /// A graph whose counts disagree with its rows is refused.
    ///
    /// Mutation: drop the predecessor-count `assert_eq!` in `new` → built,
    /// and task 1 would never run → the `should_panic` fails.
    #[test]
    #[should_panic(expected = "disagree")]
    fn inconsistent_counts_are_refused() {
        TaskGraph::new(&[0, 2], &[0, 1, 1], &[1]);
    }
}
