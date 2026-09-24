//! A `loom` model of `RtPublish`'s reclamation protocol — the shipped code,
//! not a replica.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-types --release --test rt_publish_loom
//! ```
//!
//! Under `--cfg loom`, `rt/publish.rs` builds its atomics, fences and mutex
//! from `loom` (see its `sync` module), so every interleaving and weak-memory
//! reordering loom explores below is one the real `read`/`publish`/`drop` can
//! take. `READER_SLOTS` is 2 under the flag, so three nested reads reach the
//! overflow path.
//!
//! # What each model asserts
//!
//! Every published value is a [`Tracked`] whose payload lives in a
//! `loom::cell::UnsafeCell`, written on construction and poisoned on drop:
//!
//! - **readers see a fully published value** — the reader's `with` must
//!   observe the constructor's write (a missing `Acquire`/`Release` pair is a
//!   loom causality error, or a wrong value);
//! - **no use-after-free** — the drop's `with_mut` must happen-after every
//!   reader's `with` of that value (loom reports a causality violation if a
//!   free races a read), and a reader checks the tracker says "alive";
//! - **no double free** — the tracker's per-value flag is swapped on drop and
//!   must not already be set;
//! - **no leak** — after the cell drops, every value has been freed;
//! - **the reader thread never frees** — each drop records its loom thread id,
//!   and none may be a reader's.
//!
//! # Why this can run against the real code
//!
//! `tutti-shm-model` exists as a separate crate with a mirrored protocol
//! because `--cfg loom` breaks `tutti-plugin`'s dependency closure and its
//! protocol lives in an mmap loom cannot model. Neither applies here: nothing
//! `tutti-types` depends on reacts to `cfg(loom)`, and the cell is ordinary
//! heap state. A replica would have to be kept in step by hand; this cannot
//! drift.
//!
//! # Mutation record
//!
//! Each of these was applied to `rt/publish.rs`, and the model failed:
//!
//! - either `fence(SeqCst)` removed (the reader's or the publisher's) → a
//!   causality violation in all four models: the free races the read;
//! - the reader's `current.load(Acquire)` weakened to `Relaxed` → a causality
//!   violation in all four;
//! - `RtRef::drop`'s slot `store(FREE, Release)` weakened to `Relaxed` → the
//!   drop's `with_mut` races the read (every model with a slot reader);
//! - its overflow `fetch_sub(Release)` weakened to `Relaxed` →
//!   `nested_reads_overflow` alone fails, as it is the only model that
//!   overflows;
//! - the overflow early return in `take_unprotected` deleted →
//!   `nested_reads_overflow` frees under a live overflow read;
//! - the slot check in `take_unprotected` removed (free everything retired) →
//!   causality violations (the run aborts on the first);
//! - `RtRef::drop` made to reclaim (both branches) → "reader thread freed
//!   value N" in all four.

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Mutex;
use loom::thread;
use std::sync::Arc;
use tutti_types::RtPublish;

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

// SAFETY (test): `RtPublish<T>` is shared only when `T: Send + Sync`, and
// loom's `UnsafeCell` is not `Sync`. Sharing it is exactly what the model is
// checking: readers only `with` (shared), the drop only `with_mut`
// (exclusive), and loom flags any pair of those that is not ordered by
// happens-before — which is the property under test, not an assumption of it.
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

/// Exhaustive: no preemption bound. The four models together take a few
/// minutes in release (about 200 s on the machine that wrote them, dominated
/// by `nested_reads_overflow` and `two_readers_one_publish`); a bound of 3 finishes in under a second and
/// was used while developing. Set `LOOM_MAX_PREEMPTIONS` to bound it locally.
fn model(f: impl Fn() + Sync + Send + 'static) {
    loom::model::Builder::new().check(f);
}

/// The core race: one reader against two back-to-back publishes. The second
/// publish is what reclaims a value retired by the first, so both "freed by
/// the publish that retired it" and "freed later" paths are explored.
#[test]
fn one_reader_two_publishes() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = std::sync::Arc::new(RtPublish::from_arc(tracker.make(0)));

        let reader = {
            let cell = cell.clone();
            thread::spawn(move || {
                let r = cell.read();
                r.check();
                drop(r);
                thread::current().id()
            })
        };

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        let reader = reader.join().unwrap();
        drop(std::sync::Arc::into_inner(cell).unwrap());
        tracker.assert_all_freed_off(&[reader]);
    });
}

/// Two readers on two slots against one publish: the publisher must scan
/// both, and each reader's slot protects only its own snapshot.
#[test]
fn two_readers_one_publish() {
    model(|| {
        let tracker = Tracker::new(2);
        let cell = std::sync::Arc::new(RtPublish::from_arc(tracker.make(0)));

        let readers: Vec<_> = (0..2)
            .map(|_| {
                let cell = cell.clone();
                thread::spawn(move || {
                    cell.read().check();
                    thread::current().id()
                })
            })
            .collect();

        cell.publish(tracker.make(1));

        let readers: Vec<_> = readers.into_iter().map(|h| h.join().unwrap()).collect();
        drop(std::sync::Arc::into_inner(cell).unwrap());
        tracker.assert_all_freed_off(&readers);
    });
}

/// Three nested reads on a two-slot cell: the third takes the overflow path,
/// and a concurrent publish must free nothing while it is live. Two publishes,
/// so a value retired under an overflow read is reclaimed by the later one if
/// the read has ended by then.
#[test]
fn nested_reads_overflow() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = std::sync::Arc::new(RtPublish::from_arc(tracker.make(0)));

        let reader = {
            let cell = cell.clone();
            thread::spawn(move || {
                let a = cell.read();
                let b = cell.read();
                let c = cell.read(); // overflow
                a.check();
                b.check();
                c.check();
                drop((a, b, c));
                thread::current().id()
            })
        };

        cell.publish(tracker.make(1));
        cell.publish(tracker.make(2));

        let reader = reader.join().unwrap();
        drop(std::sync::Arc::into_inner(cell).unwrap());
        tracker.assert_all_freed_off(&[reader]);
    });
}

/// Two publishers racing, with a reader: the retirement mutex must serialize
/// them so each outgoing value is retired exactly once.
#[test]
fn two_publishers_and_a_reader() {
    model(|| {
        let tracker = Tracker::new(3);
        let cell = std::sync::Arc::new(RtPublish::from_arc(tracker.make(0)));

        let reader = {
            let cell = cell.clone();
            thread::spawn(move || {
                cell.read().check();
                thread::current().id()
            })
        };
        let other = {
            let (cell, tracker) = (cell.clone(), tracker.clone());
            thread::spawn(move || cell.publish(tracker.make(2)))
        };

        cell.publish(tracker.make(1));

        other.join().unwrap();
        let reader = reader.join().unwrap();
        drop(std::sync::Arc::into_inner(cell).unwrap());
        tracker.assert_all_freed_off(&[reader]);
    });
}
