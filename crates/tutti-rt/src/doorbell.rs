//! [`Doorbell`]: wake one sleeping thread from another, without the waker
//! ever waiting.
//!
//! A helper thread that has nothing to do parks; the audio thread, which
//! must not lock, allocate or wait, rings it when work arrives. This is the
//! pair of fences and the park that a worker pool's helper used to carry
//! itself, as one primitive with a `loom` model (`tests/doorbell_loom.rs`)
//! over the shipped code.
//!
//! # The protocol
//!
//! The ringer **publishes** what the sleeper is waiting for (opens a gate,
//! pushes to a ring, sets a flag: any write), then calls [`ring`](Doorbell::ring).
//! The sleeper calls [`wait`](Doorbell::wait) with a closure that reads what
//! the ringer publishes:
//!
//! 1. `wait` stores `asleep = true`, issues a `SeqCst` fence, then runs the
//!    closure; only if it says nothing is pending does it park;
//! 2. `ring` issues a `SeqCst` fence, then reads `asleep`; only if it is set
//!    does it clear it and unpark the thread.
//!
//! The two fences are totally ordered (a store-load Dekker pair), so either
//! the closure sees the ringer's write and the sleeper does not park, or
//! `ring` sees `asleep` and unparks. A ring that lands before the park is
//! not lost either: `unpark` before `park` makes the park return at once.
//! Fences rather than `SeqCst` accesses because `loom` models a fence
//! exactly and treats a `SeqCst` access as `AcqRel`.
//!
//! # What the ringer costs
//!
//! `ring` allocates nothing and takes no lock. With no sleeper parked it is
//! a fence and a load. With one parked it is also a swap and
//! `Thread::unpark`, which is a futex wake on Linux (a system call; the
//! wake-up itself, not a wait: it does not block on the sleeper).
//!
//! # Spurious returns
//!
//! `wait` may return with nothing pending: `park` can return spuriously, a
//! `ring` that found the closure already true leaves its unpark token for
//! the next park, and a timeout ends it. The caller loops and rechecks what
//! it waits for, as with any park.

use std::time::Duration;

use sync::{AtomicBool, Ordering, current, fence, park};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, Ordering, fence};
    #[cfg(loom)]
    pub(super) use loom::thread::{Thread, current, park};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, Ordering, fence};
    #[cfg(not(loom))]
    pub(super) use std::thread::{Thread, current, park};
}

/// A wake-up for one sleeping thread. See the module docs (`src/doorbell.rs`).
///
/// One thread sleeps on a doorbell (the first to call [`wait`](Self::wait)
/// binds it); any number ring it. Aligned to two cache lines, so a slice of
/// doorbells does not share lines between sleepers.
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
/// use tutti_rt::Doorbell;
///
/// let bell = Arc::new(Doorbell::new());
/// let work = Arc::new(AtomicBool::new(false));
/// let sleeper = {
///     let (bell, work) = (Arc::clone(&bell), Arc::clone(&work));
///     std::thread::spawn(move || {
///         while !work.load(Ordering::Acquire) {
///             bell.wait(|| work.load(Ordering::Acquire));
///         }
///     })
/// };
/// work.store(true, Ordering::Release); // publish, then ring
/// bell.ring();
/// sleeper.join().unwrap();
/// ```
#[repr(align(128))]
pub struct Doorbell {
    asleep: AtomicBool,
    thread: std::sync::OnceLock<sync::Thread>,
}

impl Doorbell {
    /// A doorbell nobody sleeps on yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            asleep: AtomicBool::new(false),
            thread: std::sync::OnceLock::new(),
        }
    }

    /// Wake the sleeper if it is parked (or about to park), returning
    /// whether it was. Call it **after** the write the sleeper's closure
    /// reads. Real-time safe (module docs, "What the ringer costs").
    pub fn ring(&self) -> bool {
        // The other half of the sleeper's store-load pair (`park`): either
        // this sees its `asleep`, or its closure sees the write before this.
        fence(Ordering::SeqCst);
        // The load first: the common case, a sleeper that is awake, is not a
        // read-modify-write on a line the sleeper owns.
        if self.asleep.load(Ordering::Relaxed) && self.asleep.swap(false, Ordering::Acquire) {
            if let Some(t) = self.thread.get() {
                t.unpark();
            }
            return true;
        }
        false
    }

    /// Whether the sleeper has announced it is about to park, or is parked.
    /// A reading for diagnostics and tests; it can be stale the moment it
    /// returns.
    pub fn is_asleep(&self) -> bool {
        self.asleep.load(Ordering::Acquire)
    }

    /// Park the calling thread unless `pending` says there is already
    /// something to do, until [`ring`](Self::ring)ed. `pending` reads what
    /// the ringer publishes. May return spuriously (module docs).
    pub fn wait(&self, pending: impl FnOnce() -> bool) {
        self.park(pending, None);
    }

    /// [`wait`](Self::wait), returning after `timeout` at the latest.
    ///
    /// Under `--cfg loom` the timeout is ignored (loom has no timed park),
    /// which is the stricter model: no wake-up may be lost.
    pub fn wait_timeout(&self, pending: impl FnOnce() -> bool, timeout: Duration) {
        self.park(pending, Some(timeout));
    }

    fn park(&self, pending: impl FnOnce() -> bool, timeout: Option<Duration>) {
        // Bound before `asleep` is set, so a ringer that sees the flag sees
        // the thread (the `Release` store, the ringer's `Acquire` swap).
        self.thread.get_or_init(current);
        self.arm(true, Ordering::Release);
        // Pairs with the fence in `ring`: a write published before it is
        // seen by `pending`; one published after finds `asleep` set and
        // unparks (an unpark before the park makes it return).
        fence(Ordering::SeqCst);
        if !pending() {
            match timeout {
                #[cfg(not(loom))]
                Some(t) => std::thread::park_timeout(t),
                _ => park(),
            }
        }
        self.arm(false, Ordering::Relaxed);
    }

    /// Every write to `asleep` is a swap, so they are totally ordered with
    /// `ring`'s: a plain store racing a swap is a case `loom` orders inside
    /// the swap's read-modify-write (its model then loses a wake-up no
    /// execution of the swap has). Only the sleeper's cold path pays for it.
    fn arm(&self, asleep: bool, order: Ordering) {
        self.asleep.swap(asleep, order);
    }
}

impl Default for Doorbell {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Doorbell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Doorbell")
            .field("asleep", &self.is_asleep())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use super::*;

    fn sleeper(bell: &Arc<Doorbell>, work: &Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        let (bell, work) = (Arc::clone(bell), Arc::clone(work));
        std::thread::spawn(move || {
            while !work.load(Ordering::Acquire) {
                bell.wait(|| work.load(Ordering::Acquire));
            }
        })
    }

    /// A parked sleeper is woken by a ring (`ring` reports it), however late.
    ///
    /// Mutation: make `ring` return without the unpark -> the join hangs
    /// (the test times out under nextest).
    #[test]
    fn a_ring_wakes_a_parked_sleeper() {
        let bell = Arc::new(Doorbell::new());
        let work = Arc::new(AtomicBool::new(false));
        let t = sleeper(&bell, &work);
        // Wait until it is parked (asleep is set just before the park).
        while !bell.is_asleep() {
            std::thread::yield_now();
        }
        assert!(!t.is_finished());
        work.store(true, Ordering::Release);
        assert!(bell.ring());
        t.join().unwrap();
    }

    /// Nobody asleep: a ring reports `false` and costs no unpark.
    #[test]
    fn a_ring_with_no_sleeper_reports_it() {
        let bell = Doorbell::new();
        assert!(!bell.ring());
    }

    /// A timed wait with nothing pending returns on its own.
    ///
    /// Mutation: ignore the timeout (plain `park`) -> the test hangs.
    #[test]
    fn a_timed_wait_returns_without_a_ring() {
        let bell = Doorbell::new();
        let t0 = Instant::now();
        bell.wait_timeout(|| false, Duration::from_millis(20));
        assert!(t0.elapsed() >= Duration::from_millis(15));
    }

    /// A closure that is already true keeps the sleeper from parking.
    ///
    /// Mutation: park regardless of `pending` -> the test hangs.
    #[test]
    fn a_pending_wait_does_not_park() {
        let bell = Doorbell::new();
        bell.wait(|| true);
        assert!(!bell.is_asleep());
    }
}
