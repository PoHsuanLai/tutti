//! A `loom` model of `ArcLend`'s reclamation protocol: the shipped code, not
//! a replica.
//!
//! ```text
//! LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test -p arc-lend --release --test loom
//! ```
//!
//! Under `--cfg loom`, the crate builds its atomics, fences and mutex from
//! `loom` (see `src/sync.rs`), so every interleaving and weak-memory
//! reordering loom explores below is one the real `reader`/`lend`/`publish`/
//! `reclaim`/`drop` can take. The slot list starts at one slot and doubles,
//! so a model with two readers grows it once and one with three twice.
//!
//! # What each model asserts
//!
//! Every published value is a [`Tracked`] whose payload lives in a
//! `loom::cell::UnsafeCell`, written on construction and poisoned on drop:
//!
//! - **readers see a fully published value**: the reader's `with` must
//!   observe the constructor's write (a missing `Acquire`/`Release` pair is a
//!   loom causality error, or a wrong value);
//! - **no use-after-free**: the drop's `with_mut` must happen-after every
//!   reader's `with` of that value (loom reports a causality violation if a
//!   free races a read), and a reader checks the tracker says "alive";
//! - **no double free**: the tracker's per-value flag is swapped on drop and
//!   must not already be set;
//! - **no leak**: after the cell drops, every value has been freed;
//! - **the reader thread never frees**: each drop records its loom thread id,
//!   and none may be a reader's;
//! - **the publisher never waits on a reader**: there is no loop in a
//!   publish, so a model whose publisher spun on a reader mid-lend would not
//!   terminate in loom's bounded exploration (loom reports a model that
//!   exceeds its branch limit as a failure).
//!
//! # Why this can run against the real code
//!
//! A model of a replica would have to be kept in step with the code by hand;
//! this one cannot drift. It can run the real code because nothing the crate
//! depends on reacts to `cfg(loom)` (it has no dependencies), and the cell is
//! ordinary heap state: no mmap, no FFI.
//!
//! Two properties are the unit tests', not this model's, because breaking either only makes
//! the publisher keep *more*: a new claim pins only from the last scan
//! (`a_new_claim_pins_only_from_the_last_scan`), and a forgotten loan pins one value until
//! its slot is reused (`a_forgotten_loan_pins_one_value_until_its_slot_is_reused`).

#![cfg(loom)]

use arc_lend::ArcLend;
use loom::cell::UnsafeCell;
use loom::sync::Mutex;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread;
use std::sync::Arc;

/// Bookkeeping shared by every value in one model execution. Its own memory is
/// never freed by the cell, so it is safe to consult about values that are.
struct Tracker {
    freed: Vec<AtomicBool>,
    freed_on: Mutex<Vec<(usize, thread::ThreadId)>>,
}

impl Tracker {
    fn new(values: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            freed: (0..values).map(|_| AtomicBool::new(false)).collect(),
            freed_on: Mutex::new(Vec::new()),
        })
    }

    fn make(self: &std::sync::Arc<Self>, id: usize) -> Arc<Tracked> {
        Arc::new(Tracked {
            id,
            payload: UnsafeCell::new(id * 10),
            tracker: self.clone(),
        })
    }

    /// End-of-execution checks: every value freed once, never on a reader.
    fn assert_all_freed_off(&self, readers: &[thread::ThreadId]) {
        for (id, f) in self.freed.iter().enumerate() {
            assert!(f.load(Ordering::SeqCst), "value {id} leaked");
        }
        for (id, t) in self.freed_on.lock().unwrap().iter() {
            assert!(!readers.contains(t), "reader thread freed value {id}");
        }
    }
}

struct Tracked {
    id: usize,
    payload: UnsafeCell<usize>,
    tracker: std::sync::Arc<Tracker>,
}

// SAFETY (test): `ArcLend<T>` is shared only when `T: Send + Sync`, and
// loom's `UnsafeCell` is not `Sync`. Sharing it is exactly what the model is
// checking: readers only `with` (shared), the drop only `with_mut`
// (exclusive), and loom flags any pair of those that is not ordered by
// happens-before, which is the property under test, not an assumption of it.
unsafe impl Sync for Tracked {}

impl Tracked {
    /// What a reader does with a value: check it is alive and whole.
    fn check(&self) {
        assert!(
            !self.tracker.freed[self.id].load(Ordering::SeqCst),
            "read value {} after it was freed",
            self.id
        );
        // SAFETY (test): a shared read; loom checks it against the drop's
        // write for causality.
        let v = self.payload.with(|p| unsafe { *p });
        assert_eq!(
            v,
            self.id * 10,
            "value {} seen half-built or poisoned",
            self.id
        );
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        // SAFETY (test): the exclusive write loom checks against every read.
        self.payload.with_mut(|p| unsafe { *p = usize::MAX });
        assert!(
            !self.tracker.freed[self.id].swap(true, Ordering::SeqCst),
            "value {} freed twice",
            self.id
        );
        self.tracker
            .freed_on
            .lock()
            .unwrap()
            .push((self.id, thread::current().id()));
    }
}

/// No preemption bound of its own: exhaustive unless `LOOM_MAX_PREEMPTIONS`
/// is set. CI runs every model unbounded but
/// `registering_and_dropping_readers`, which it bounds at 4 and 5 (at 6 it
/// did not finish in 25 minutes):
///
/// ```text
/// RUSTFLAGS="--cfg loom" cargo test --release --test loom -- --skip registering_and_dropping_readers
/// LOOM_MAX_PREEMPTIONS=5 RUSTFLAGS="--cfg loom" cargo test --release --test loom -- registering_and_dropping_readers --exact
/// ```
fn model(f: impl Fn() + Sync + Send + 'static) {
    loom::model::Builder::new().check(f);
}

type Cell = std::sync::Arc<ArcLend<Tracked>>;

fn cell(tracker: &std::sync::Arc<Tracker>) -> Cell {
    std::sync::Arc::new(ArcLend::from_arc(tracker.make(0)))
}

/// Drops the cell (it must be the last handle) and checks every value went,
/// none on a reader.
fn finish(cell: Cell, tracker: &Tracker, readers: &[thread::ThreadId]) {
    drop(std::sync::Arc::into_inner(cell).expect("every reader is dropped"));
    tracker.assert_all_freed_off(readers);
}

/// The core race: one reader against two back-to-back publishes. The second
/// publish is what reclaims a value retired by the first, so both "freed by
/// the publish that retired it" and "freed later" are explored, and so is a
/// reader preempted mid-lend across both scans (the claim the second scan
/// must recognise as the one the first saw).
#[test]
fn one_reader_two_publishes() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);
        let mut r = cell.reader();
        let reader = thread::spawn(move || {
            r.lend().check();
            drop(r);
            thread::current().id()
        });

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        let reader = reader.join().unwrap();
        finish(cell, &tracker, &[reader]);
    });
}

/// Two readers on two slots against one publish: the publisher must scan
/// both, and each reader's slot protects only its own snapshot.
#[test]
fn two_readers_one_publish() {
    model(|| {
        let tracker = Tracker::new(2);
        let cell = cell(&tracker);
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let mut r = cell.reader();
                thread::spawn(move || {
                    r.lend().check();
                    drop(r);
                    thread::current().id()
                })
            })
            .collect();

        cell.publish(tracker.make(1));

        let readers: Vec<_> = readers.into_iter().map(|h| h.join().unwrap()).collect();
        finish(cell, &tracker, &readers);
    });
}

/// One reader lends twice in a row against two publishes: the second lend's
/// claim is a new generation in the same slot. A scan that reads it must
/// still have the first lend's reads ordered before anything it frees (the
/// claim's `Release`), and must tell it from the first claim.
#[test]
fn a_reader_lends_twice() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);
        let mut r = cell.reader();
        let reader = thread::spawn(move || {
            r.lend().check();
            r.lend().check();
            drop(r);
            thread::current().id()
        });

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        let reader = reader.join().unwrap();
        finish(cell, &tracker, &[reader]);
    });
}

/// Nesting: one thread holds two readers' loans at once (one reader per
/// level), against two publishes.
#[test]
fn nested_readers() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);
        let (mut outer, mut inner) = (cell.reader(), cell.reader());
        let reader = thread::spawn(move || {
            let a = outer.lend();
            let b = inner.lend();
            a.check();
            b.check();
            drop((a, b));
            drop((outer, inner));
            thread::current().id()
        });

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        let reader = reader.join().unwrap();
        finish(cell, &tracker, &[reader]);
    });
}

/// A publisher-side read racing two publishes: `current` takes an owning
/// handle under the lock, so the value it returns stays alive however the
/// scans interleave, and is freed exactly once.
///
/// The snapshot goes back to the main thread through `join` and is dropped
/// there. The values are `std::sync::Arc`s, whose refcount loom does not
/// instrument: a snapshot dropped on its own thread would race the scan's
/// drop of the cell's count, and loom, not seeing the `Arc`'s
/// `Release`/`Acquire` decrement, would report the read before it as racing
/// the free. Handing it back keeps the model on what it checks, the load
/// and increment under the lock against a publish and its free; that two
/// owners' drops are ordered is `Arc`'s own guarantee.
#[test]
fn current_while_publishing() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);
        let snapshot = {
            let cell = cell.clone();
            thread::spawn(move || {
                let value = cell.current();
                value.check();
                value
            })
        };

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        drop(snapshot.join().unwrap());
        finish(cell, &tracker, &[]);
    });
}

/// Two publishers racing, with a reader: the control mutex must serialize
/// them so each outgoing value is retired exactly once, and both scans must
/// see the reader.
#[test]
fn two_publishers_and_a_reader() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);
        let mut r = cell.reader();
        let reader = thread::spawn(move || {
            r.lend().check();
            drop(r);
            thread::current().id()
        });
        let other = {
            let (cell, tracker) = (cell.clone(), tracker.clone());
            thread::spawn(move || cell.publish(tracker.make(2)))
        };

        cell.publish(tracker.make(1));

        other.join().unwrap();
        let reader = reader.join().unwrap();
        finish(cell, &tracker, &[reader]);
    });
}

/// A reclaim (a scan with no swap) between a publish and the end of a lend:
/// its fence orders the last publish's swap against the reader's claim.
#[test]
fn reclaim_while_a_reader_lends() {
    model(|| {
        let tracker = Tracker::new(2);
        let cell = cell(&tracker);
        let mut r = cell.reader();
        let reader = thread::spawn(move || {
            r.lend().check();
            drop(r);
            thread::current().id()
        });

        cell.publish(tracker.make(1));
        cell.reclaim();

        let reader = reader.join().unwrap();
        finish(cell, &tracker, &[reader]);
    });
}

/// A reclaim with nothing retired (the early return, no fence, no scan)
/// before a publish, and a reclaim after it, while a reader lends twice: the
/// skipped scan must leave the claim bookkeeping as safe as a scan would.
#[test]
fn reclaim_with_nothing_retired() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);
        let mut r = cell.reader();
        let reader = thread::spawn(move || {
            r.lend().check();
            r.lend().check();
            drop(r);
            thread::current().id()
        });

        cell.reclaim();
        cell.publish(tracker.make(1));
        cell.reclaim();
        cell.publish(tracker.make(2));

        let reader = reader.join().unwrap();
        finish(cell, &tracker, &[reader]);
    });
}

/// Readers registered and dropped while a publisher runs, on a list that
/// doubles: the first two readers fill the first two blocks (a slot each),
/// and the third registration, while the second still holds its slot,
/// reuses the first's slot if its reader has dropped by then (a slot whose
/// previous owner was a different thread), or grows the list by a block of
/// two.
#[test]
fn registering_and_dropping_readers() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = cell(&tracker);

        let mut first = cell.reader();
        let mut second = cell.reader();
        let a = thread::spawn(move || {
            first.lend().check();
            drop(first);
            thread::current().id()
        });
        let b = {
            let cell = cell.clone();
            thread::spawn(move || {
                second.lend().check();
                // Registers on this thread, while the publisher may be
                // scanning: reuses `first`'s slot if it is free by now,
                // grows the list otherwise.
                let mut third = cell.reader();
                third.lend().check();
                drop((second, third));
                thread::current().id()
            })
        };

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        let readers = [a.join().unwrap(), b.join().unwrap()];
        finish(cell, &tracker, &readers);
    });
}
