//! Streamer thread body: the main loop.
//!
//! The *work* of a cycle is
//! [`StreamerCycle::step`](super::step::StreamerCycle::step), synchronous and
//! shared with the test driver. What lives here is the sleeping: when a cycle
//! finds nothing urgent the thread parks on the [`Bell`]
//! until a source rings it (a command, a voice's ring wanting a refill,
//! shutdown) or the backstop passes. Nothing polls.

use std::thread;
use std::time::Duration;

use crossbeam_channel::Receiver;

use super::bell::Bell;
use super::command::StreamerCommand;
use super::handlers::Handles;
use super::step::{StepOutcome, StreamerCycle};

/// How long the streamer sleeps at most when nothing rings it.
///
/// It is the bound on how late a refill can be if a ring is ever *missed* (the
/// design has none: every source rings), and the control-side tick a cell that
/// goes quiet still gets its `reclaim()` on (CLAUDE.md, "Publishing to the
/// audio thread"; each cycle sweeps every ring's retired controls). 50 ms is
/// well inside the smallest ring's drain time at its refill threshold and long
/// enough that an idle streamer costs twenty wake-ups a second.
pub(super) const BACKSTOP: Duration = Duration::from_millis(50);

/// The streamer thread's main loop; returns when shutdown is signalled
/// ([`Bell::shut`]).
///
/// Each cycle clears the bell's refill flag, drains the queue and does one
/// [`StreamerCycle::step`], then sleeps unless the step has more to do. No wake
/// is lost: whatever rings after the cycle began (a command pushed, a flag set)
/// is read by the sleep's own check after it arms the doorbell, so the sleep
/// returns at once.
pub(super) fn streamer_loop(
    queue: &Receiver<StreamerCommand>,
    shared: Handles,
    mut cycle: StreamerCycle,
    backstop: Duration,
) {
    let bell = shared.bell.clone();
    while !bell.is_shut() {
        bell.begin_cycle();
        match cycle.step(&shared, || queue.try_recv().ok()) {
            // Nothing urgent, or nothing this loop can do about it (a ring
            // that holds all its mapping writes, the tail of every clip):
            // sleep until something rings.
            StepOutcome::Idle | StepOutcome::Healthy | StepOutcome::Stalled => {
                sleep(&bell, queue, backstop);
            }

            // A ring is below threshold and the cycle moved frames into it:
            // come straight back, letting other threads run first.
            StepOutcome::Busy => thread::yield_now(),
        }
    }
}

fn sleep(bell: &Bell, queue: &Receiver<StreamerCommand>, backstop: Duration) {
    bell.sleep(|| !queue.is_empty(), backstop);
}
