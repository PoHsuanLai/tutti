//! A `loom` model of `ArcLend`'s reclamation protocol: the shipped code, not
//! a replica.
//!
//! ```text
//! LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test -p arc-lend --release --test loom
//! ```
//!
//! Under `--cfg loom`, `src/lend.rs` builds its atomics, fences and mutex
//! from `loom` (see its `sync` module), so every interleaving and weak-memory
//! reordering loom explores below is one the real `reader`/`lend`/`publish`/
//! `reclaim`/`drop` can take. Under the flag a block of the slot list holds
//! two slots, so a model with three readers grows the list.
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
//! # Mutation record
//!
//! Each of these was applied to `src/lend.rs` and the models run (at
//! `LOOM_MAX_PREEMPTIONS=3`, each model on its own, because an aborting model
//! hides the rest). The failing models are named.
//!
//! "All seven" means `one_reader_two_publishes`, `two_readers_one_publish`,
//! `a_reader_lends_twice`, `nested_readers`, `two_publishers_and_a_reader`,
//! `reclaim_while_a_reader_lends` and `registering_and_dropping_readers`.
//!
//! - the reader's `fence(SeqCst)` removed → all seven (a causality
//!   violation: the scan misses the claim and frees what the reader loads);
//! - the publisher's `fence(SeqCst)` (in `scan`) removed → all seven;
//! - the lend's `current.load(Acquire)` weakened to `Relaxed` → all seven
//!   (the reader sees the pointer before the payload's write);
//! - the claim's `store(claim, Release)` weakened to `Relaxed` →
//!   `a_reader_lends_twice` only: the scan reads the second lend's claim,
//!   which no longer carries the first lend's reads of value 0 with it, and
//!   frees 0 under them;
//! - the loan drop's `store(FREE, Release)` weakened to `Relaxed` → all
//!   seven (a scan that reads the `FREE` no longer happens-after the
//!   reader's last read);
//! - the scan's slot load weakened from `Acquire` to `Relaxed` → all seven;
//! - the publish's `swap(Release)` weakened to `Relaxed` → all seven (a
//!   causality violation: a reader's `Acquire` load of the new pointer no
//!   longer sees the payload's construction);
//! - every claim given a fresh floor (`scanned_at`, as if the last scan had
//!   not seen it) → all but `two_readers_one_publish` (which has one
//!   publish, so no claim spans two scans): a reader preempted between its
//!   fence and its announcement across two publishes loads value 0, which
//!   the second publish frees;
//! - a claim made to pin nothing (`floor` left at `u64::MAX`) → all seven;
//! - the announcement check removed from the retired filter → all seven;
//! - `Reader::drop` made to reclaim (lock, `scan`, free) → all seven, with
//!   "reader thread freed value N";
//! - `Reader::drop`'s `owned.store(false, Release)` weakened to `Relaxed` →
//!   `registering_and_dropping_readers` only: the new owner's registration
//!   no longer happens-after the old owner's last `FREE`, and a scan that
//!   reads that `FREE` frees the new owner's value.
//!
//! The swap was `AcqRel` until a review found that weakening it to `Release`
//! survives every model: the outgoing value was stored by a publisher holding
//! the same lock, so the mutex already orders its contents before this
//! publisher, and the `Acquire` half added nothing. `Release` is what ships.

//! Two mutations survive, by construction, because both only make the
//! publisher keep *more*:
//!
//! - dropping the generation from the claim (`next_claim` returning `1`):
//!   every claim then looks like the one the last scan saw, so each keeps the
//!   oldest floor. Safe, but a reader that keeps lending pins every value
//!   retired since its first claim. The unit test
//!   `a_new_claim_pins_only_from_the_last_scan` catches it.
//! - not resetting a reused slot's word to `FREE` at registration: a stale
//!   announcement pins one value until the new owner's first lend. The unit
//!   test `a_forgotten_loan_pins_one_value_until_its_slot_is_reused`
//!   catches it.

#![cfg(loom)]

use arc_lend::ArcLend;
use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Mutex;
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
/// is set. CI and `just loom-full` run every model unbounded but
/// `registering_and_dropping_readers`, which they bound at 4 and 5 (at 6 it
/// did not finish in 25 minutes).
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

/// Readers registered and dropped while a publisher runs, on a list of
/// two-slot blocks: the first reader's slot is freed and handed to the third
/// while the second holds the other slot, so the third registration reuses a
/// slot whose previous owner was a different thread, and a fourth grows the
/// list.
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
