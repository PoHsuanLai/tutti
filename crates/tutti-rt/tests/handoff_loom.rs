//! A `loom` model of [`handoff`]: the shipped code, whose atomics, slot cell and
//! `Arc` are loom's under the flag (see `src/handoff.rs`).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test handoff_loom -- --nocapture
//! ```
//!
//! Over every interleaving:
//!
//! - **a value is in one place**: after the threads join, each value lent is in
//!   exactly one of a withdrawn recall, a returned slot, the audio end, or the
//!   control end's hands (a refused lend), and it drops exactly once; loom's
//!   cell checks that no two threads reach the slot at once;
//! - **a recall is never lost**: a value the audio end still holds after the
//!   join comes back at its next poll, with `last` run on it;
//! - **a lend racing a return** (the control end lends a second value while
//!   the audio end returns the first) loses and duplicates neither;
//! - **lends overlapping the polls** are ordered with them: the take sees the
//!   lend's write, and a second lend writes only after the first was taken.
//! - **a lend never erases a recall** for a value the audio end still holds
//!   (Core's review of #494): the lend is refused and the value comes back.
//!
//! # Mutation record
//!
//! Each was applied to `src/handoff.rs` and run; the model failed as noted.
//!
//! - `poll` moving an offer out after its `Acquire` load, without claiming it
//!   (the first draft) → the first model: loom reports a causality violation
//!   on the slot (a recall withdrawing the same offer reaches it too);
//! - `set_slot` as a `fetch_and` then a `fetch_or` (the first draft) → the
//!   second model, "each value in one place": the control end claims the field
//!   while it reads empty between the two, and a value goes missing;
//! - `claim`'s compare-exchange `Relaxed` on success → the third model: a
//!   causality violation (the second lend's write is not ordered after the
//!   take of the first);
//! - `set_slot`'s compare-exchange `Relaxed` → the third model: a causality
//!   violation (the take reads the slot before the lend's write is visible);
//! - `recall` raising the bit with a plain `store` → the first model: the slot
//!   field it overwrote is lost, so the recall never returns the value.
//! - `lend` claiming without `not_while_recalled` (as first written, where it
//!   cleared the bit) → the fourth model: B is lent, the bit is gone, and A is
//!   never returned ("A comes back").
//!
//! The test prints the number of executions loom ran.

#![cfg(loom)]

use std::sync::atomic::{AtomicUsize, Ordering};

use loom::thread;
use tutti_rt::{Rt, handoff};

/// Executions loom ran, to report.
static RUNS: AtomicUsize = AtomicUsize::new(0);

/// A value that records whether `last` ran on it and counts its drops.
struct Tracked {
    id: u32,
    stopped: bool,
    drops: std::sync::Arc<AtomicUsize>,
}

impl Tracked {
    fn new(id: u32, drops: &std::sync::Arc<AtomicUsize>) -> Self {
        Self {
            id,
            stopped: false,
            drops: std::sync::Arc::clone(drops),
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn stop(v: &mut Tracked) {
    v.stopped = true;
}

/// The control end lends, the audio end polls twice, the control end recalls
/// meanwhile; the value ends in one place and a recall still owed is honoured.
#[test]
fn a_lent_value_is_in_one_place_and_a_recall_is_never_lost() {
    loom::model(|| {
        RUNS.fetch_add(1, Ordering::Relaxed);
        let drops = std::sync::Arc::new(AtomicUsize::new(0));
        let (mut hand, mut rt) = handoff::<Tracked>();
        assert!(hand.lend(Tracked::new(1, &drops)).is_ok());

        let audio = thread::spawn(move || {
            for _ in 0..2 {
                let _ = rt.poll(stop);
            }
            rt
        });
        let withdrawn = hand.recall();
        let mut rt: Rt<Tracked> = audio.join().unwrap();

        let returned = hand.take();
        let held = rt.held().is_some();
        let places =
            usize::from(withdrawn.is_some()) + usize::from(returned.is_some()) + usize::from(held);
        assert_eq!(places, 1, "the value is in one place");
        if let Some(v) = &returned {
            assert!(v.stopped, "returned only after `last`");
        }
        if let Some(v) = &withdrawn {
            assert!(!v.stopped, "a withdrawn offer never reached the audio end");
        }
        if held {
            // Recalled while the audio end held it and after its last poll:
            // the bit is still raised, so the next poll returns it.
            assert!(rt.poll(stop).is_none(), "a recall is never lost");
            let back = hand.take().expect("returned");
            assert!(back.stopped);
        }
        drop((withdrawn, returned, hand, rt));
        assert_eq!(drops.load(Ordering::SeqCst), 1, "dropped once");
    });
    println!("handoff_loom: {} executions", RUNS.load(Ordering::Relaxed));
}

/// The audio end returns the first value while the control end lends a
/// second: each ends in one place, and both drop once.
#[test]
fn a_lend_racing_a_return_loses_and_duplicates_nothing() {
    loom::model(|| {
        let drops = std::sync::Arc::new(AtomicUsize::new(0));
        let (mut hand, mut rt) = handoff::<Tracked>();
        assert!(hand.lend(Tracked::new(1, &drops)).is_ok());
        assert!(rt.poll(stop).is_some(), "taken");
        assert!(hand.recall().is_none(), "held by the audio end");

        let audio = thread::spawn(move || {
            let _ = rt.poll(stop);
            rt
        });
        let refused = hand.lend(Tracked::new(2, &drops)).err();
        let mut rt = audio.join().unwrap();

        let mut ids = Vec::new();
        if let Some(v) = &refused {
            ids.push(v.id);
        }
        if let Some(v) = rt.held() {
            ids.push(v.id);
        }
        // The slot: either the returned first value, or the second on offer.
        let returned = hand.take();
        if let Some(v) = &returned {
            ids.push(v.id);
        }
        let offered = hand.recall();
        if let Some(v) = &offered {
            ids.push(v.id);
        }
        ids.sort_unstable();
        assert_eq!(ids, [1, 2], "each value in one place");
        drop((refused, returned, offered, hand, rt));
        assert_eq!(drops.load(Ordering::SeqCst), 2, "each dropped once");
    });
}

/// The control end lends twice while the audio end polls twice, so the slot
/// passes between the threads while both run: a lend's write must be seen by
/// the take that follows it, and a second lend's write must follow the take of
/// the first. Each value ends in one place and drops once.
#[test]
fn lends_overlapping_the_polls_are_ordered_with_them() {
    loom::model(|| {
        let drops = std::sync::Arc::new(AtomicUsize::new(0));
        let (mut hand, mut rt) = handoff::<Tracked>();
        let audio = thread::spawn(move || {
            for _ in 0..2 {
                let _ = rt.poll(stop);
            }
            rt
        });
        assert!(
            hand.lend(Tracked::new(1, &drops)).is_ok(),
            "the slot was empty"
        );
        // Without it loom never schedules the audio end's take between the
        // two lends (counted: no execution of 49 had the second lend land);
        // with it, 84 of 262 do.
        thread::yield_now();
        let refused = hand.lend(Tracked::new(2, &drops)).err();
        let mut rt = audio.join().unwrap();

        let mut ids = Vec::new();
        if let Some(v) = rt.held() {
            ids.push(v.id);
        }
        if let Some(v) = &refused {
            ids.push(v.id);
        }
        let offered = hand.recall();
        if let Some(v) = &offered {
            ids.push(v.id);
        }
        ids.sort_unstable();
        assert_eq!(ids, [1, 2], "each value in one place");
        drop((refused, offered, hand, rt));
        assert_eq!(drops.load(Ordering::SeqCst), 2, "each dropped once");
    });
}

/// Core's sequence on #494: the audio end holds A, a recall is raised, and the
/// control end lends B while the audio end polls. The recall is never erased:
/// A comes back (now or at the next poll), and B is refused, never offered in
/// A's place.
#[test]
fn a_lend_never_erases_a_recall_for_a_held_value() {
    loom::model(|| {
        let drops = std::sync::Arc::new(AtomicUsize::new(0));
        let (mut hand, mut rt) = handoff::<Tracked>();
        assert!(hand.lend(Tracked::new(1, &drops)).is_ok());
        assert!(rt.poll(stop).is_some(), "A taken");
        assert!(hand.recall().is_none(), "A is held: the bit is raised");

        let audio = thread::spawn(move || {
            let _ = rt.poll(stop);
            rt
        });
        let refused = hand.lend(Tracked::new(2, &drops)).err();
        let mut rt = audio.join().unwrap();

        assert_eq!(refused.as_ref().map(|v| v.id), Some(2), "B is refused");
        // A was returned by the poll, or is returned by the next one: the
        // recall is still raised.
        let _ = rt.poll(stop);
        let back = hand.take().expect("A comes back");
        assert_eq!(back.id, 1);
        assert!(back.stopped);
        assert!(rt.held().is_none());
        drop((refused, back, hand, rt));
        assert_eq!(drops.load(Ordering::SeqCst), 2, "each dropped once");
    });
}
