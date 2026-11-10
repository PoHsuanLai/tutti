//! A `loom` model of [`Doorbell`]: the shipped code, whose atomics, fences
//! and park are loom's under the flag (see `src/doorbell.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test doorbell_loom   # with the loom cfg flag set
//! ```
//!
//! The sleeper waits for a flag the ringer sets and then rings, the shape of
//! every user (a pool's gate, a bridge's queue). Loom reports a lost wake-up
//! as a deadlock (the sleeper parked, nobody left to unpark it). Over every
//! interleaving:
//!
//! - **ring before the park** (`a_ring_before_the_park_is_not_lost`): the
//!   ringer may run entirely before, during or after the sleeper's `wait`;
//! - **ring during the park**: the same model covers the ring landing between
//!   the sleeper's `asleep` store, its check and its park;
//! - **many rings** (`several_rings_wake_one_sleeper_for_each_round`): a
//!   sleeper that is rung for two rounds in a row wakes for both, whichever
//!   ring finds it asleep, and a ring that finds it awake leaves at most a
//!   stale token (a spurious return, which the loop absorbs);
//! - **one fence, many bells** (`ring_first_*`): the pool's `wake`, ringing
//!   two sleepers after one fence;
//! - **two ringers** (`two_ringers_one_sleeper`): both may ring; the sleeper
//!   sees the work either way.
//!
//! # The swaps
//!
//! Every write to `asleep` is a `swap`, not a store. With a plain `store`
//! for the sleeper's arm and disarm, loom reports a lost wake-up in
//! `several_rings_wake_one_sleeper_for_each_round` from 4 preemptions (the
//! other tests still pass): the first ring's `swap` reads the sleeper's
//! disarm, the sleeper's next arm is then ordered inside that `swap`'s
//! read-modify-write, and the second ring reads the `false`. That execution
//! is a loom artefact: under the C++20 model a read-modify-write is atomic,
//! so no store can fall inside it, and a plain-store `Doorbell` is correct.
//! The swaps keep the model honest: with every write to `asleep` a
//! read-modify-write they are totally ordered, and loom has no such
//! execution to report. Because the artefact needs 4 preemptions, CI and
//! `cargo xtask loom` run this model unbounded.
//!
//! # Mutation record
//!
//! Each was applied to `src/doorbell.rs` (`LOOM_MAX_PREEMPTIONS=3`, CI's
//! bound); all three tests failed with a deadlock unless noted.
//!
//! - `ring` without its `SeqCst` fence -> deadlock;
//! - `park` without its `SeqCst` fence -> deadlock;
//! - `park` not arming `asleep` -> deadlock (a ring finds nobody asleep);
//! - `ring` reading `asleep` before its fence instead of after -> deadlock;
//! - `ring` without the `unpark` -> deadlock;
//! - `park` ignoring `pending` (always parking) -> only
//!   `several_rings_wake_one_sleeper_for_each_round` fails, as a deadlock
//!   (the other two are saved by the unpark token the ring left);
//! - the arm and disarm back to plain stores -> deadlock from 4 preemptions
//!   in `several_rings_*` only (the artefact above)
//!   (above);
//! - `ring_first` without its fence, or with it after the loop ->
//!   deadlock in `ring_first_wakes_every_sleeper_it_is_asked_to`;
//! - `ring_first` stopping after one bell woken (not `n`) -> the same;
//! - not caught: the arm's `Release` and the swap's `Acquire` weakened to
//!   `Relaxed`. The thread handle sits in a `std::sync::OnceLock` that loom
//!   does not track, so that pair (the handle is set before `asleep`, and a
//!   ringer that sees `asleep` sees the handle) is argued in the source, not
//!   checked here.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use tutti_rt::Doorbell;

#[test]
fn a_ring_before_the_park_is_not_lost() {
    loom::model(|| {
        let bell = Arc::new(Doorbell::new());
        let work = Arc::new(AtomicUsize::new(0));
        let ringer = {
            let (bell, work) = (Arc::clone(&bell), Arc::clone(&work));
            thread::spawn(move || {
                work.store(1, Ordering::Release);
                bell.ring();
            })
        };
        while work.load(Ordering::Acquire) == 0 {
            bell.wait(|| work.load(Ordering::Acquire) != 0);
        }
        ringer.join().unwrap();
    });
}

#[test]
fn several_rings_wake_one_sleeper_for_each_round() {
    loom::model(|| {
        let bell = Arc::new(Doorbell::new());
        let work = Arc::new(AtomicUsize::new(0));
        let ringer = {
            let (bell, work) = (Arc::clone(&bell), Arc::clone(&work));
            thread::spawn(move || {
                for round in 1..=2 {
                    work.store(round, Ordering::Release);
                    bell.ring();
                }
            })
        };
        let mut seen = 0;
        while seen < 2 {
            let now = work.load(Ordering::Acquire);
            if now > seen {
                seen = now;
                continue;
            }
            bell.wait(|| work.load(Ordering::Acquire) > seen);
        }
        ringer.join().unwrap();
    });
}

#[test]
fn two_ringers_one_sleeper() {
    loom::model(|| {
        let bell = Arc::new(Doorbell::new());
        let work = Arc::new(AtomicUsize::new(0));
        let ringers: Vec<_> = (0..2)
            .map(|_| {
                let (bell, work) = (Arc::clone(&bell), Arc::clone(&work));
                thread::spawn(move || {
                    work.fetch_add(1, Ordering::Release);
                    bell.ring();
                })
            })
            .collect();
        while work.load(Ordering::Acquire) < 2 {
            bell.wait(|| work.load(Ordering::Acquire) >= 2);
        }
        for r in ringers {
            r.join().unwrap();
        }
    });
}

/// One `ring_first` after a single publish wakes two sleepers on two bells:
/// the shipped one-fence path the pool's `wake` takes.
#[test]
fn ring_first_wakes_every_sleeper_it_is_asked_to() {
    loom::model(|| {
        let bells = Arc::new(vec![Doorbell::new(), Doorbell::new()]);
        let work = Arc::new(AtomicUsize::new(0));
        let sleepers: Vec<_> = (0..2)
            .map(|i| {
                let (bells, work) = (Arc::clone(&bells), Arc::clone(&work));
                thread::spawn(move || {
                    while work.load(Ordering::Acquire) == 0 {
                        bells[i].wait(|| work.load(Ordering::Acquire) != 0);
                    }
                })
            })
            .collect();
        work.store(1, Ordering::Release);
        Doorbell::ring_first(&bells[..], 2);
        for s in sleepers {
            s.join().unwrap();
        }
    });
}
