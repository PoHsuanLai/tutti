//! A `loom` model of [`FaultLatch`]: the shipped code, whose flag is loom's
//! under the flag (see `src/fault_latch.rs`). The ring the items travel
//! through is [`edit_ring`], itself a shipped, loom-checked structure, standing
//! in for the `ringbuf` rings of a capture source; what is under test is the
//! flag-before-ring order, which does not depend on which ring it is.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test fault_latch_loom
//! ```
//!
//! The producer (a capture callback) pushes 1, 2 and then raises the latch.
//! The consumer (a recorder's pump) runs [`FaultLatch::drain`] over the ring
//! for three passes, stopping at [`Drained::Failed`], and after the producer
//! joins drains what is left. Over every interleaving:
//!
//! - **a failure never hides a frame**: the pass that answers `Failed` happened
//!   after every pushed item was taken, so the items taken before it are all of
//!   them, in order;
//! - **nothing is lost or repeated**: the items taken are `[1, 2]`, in order;
//! - **a raised latch is seen**: after the producer joined, the drain ends in
//!   `Failed`, never `Empty`;
//! - **an unraised latch is never a failure** (the second test).
//!
//! # Mutation record
//!
//! Each was applied to `src/fault_latch.rs`; the model failed as noted.
//!
//! - `drain` popping first and reading the latch after (the order doc 025
//!   forbids) → "a failure hid a frame": the pop finds the ring empty, the
//!   producer then pushes 2 and raises, and the load reads the raise;
//! - `raise` as a `Relaxed` store → the same (the load reads the raise but is
//!   not ordered after the pushes, so the pop can miss them);
//! - `is_raised` as a `Relaxed` load → the same;
//! - `drain` answering `Failed` on any empty pop, raised or not → "an empty
//!   pop is not a failure" in the second test (and "a failure hid a frame" in
//!   the first).
//!
//! The model runs 4,961 executions (the test prints the number of times the
//! closure ran).

#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;
use tutti_rt::{Drained, FaultLatch, edit_ring};

/// Executions loom ran, to report.
static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn a_failure_is_never_read_before_the_frames_pushed_ahead_of_it() {
    loom::model(|| {
        RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let latch = Arc::new(FaultLatch::new());
        let (mut writer, mut reader) = edit_ring::<u32>(4);

        let producer = {
            let latch = Arc::clone(&latch);
            thread::spawn(move || {
                writer.push(1).expect("room");
                writer.push(2).expect("room");
                latch.raise();
            })
        };

        let mut taken = Vec::new();
        for _ in 0..3 {
            match latch.drain(|| reader.pop()) {
                Drained::Item(n) => taken.push(n),
                Drained::Empty => {}
                Drained::Failed => {
                    assert_eq!(taken, [1, 2], "a failure hid a frame");
                    break;
                }
            }
        }

        producer.join().unwrap();
        // The producer is done: whatever is left is drained, and the raise is
        // seen.
        loop {
            match latch.drain(|| reader.pop()) {
                Drained::Item(n) => taken.push(n),
                Drained::Empty => panic!("the latch is raised, so an empty pop is a failure"),
                Drained::Failed => break,
            }
        }
        assert_eq!(taken, [1, 2], "nothing lost, nothing repeated, in order");
    });
    eprintln!(
        "fault_latch model: {} executions",
        RUNS.load(std::sync::atomic::Ordering::Relaxed)
    );
}

/// An empty ring under a latch nobody raised is `Empty`, never `Failed`.
#[test]
fn an_unraised_latch_never_reads_as_failed() {
    loom::model(|| {
        let latch = Arc::new(FaultLatch::new());
        let (mut writer, mut reader) = edit_ring::<u32>(2);
        let producer = thread::spawn(move || {
            writer.push(7).expect("room");
        });
        for _ in 0..2 {
            let verdict = latch.drain(|| reader.pop());
            assert_ne!(verdict, Drained::Failed, "an empty pop is not a failure");
        }
        producer.join().unwrap();
    });
}
