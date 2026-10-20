//! Butler thread body: the main loop.
//!
//! The *work* of a cycle is
//! [`ButlerCycle::step`](super::step::ButlerCycle::step) — synchronous, and
//! shared with the test driver. What lives here is the pacing: parking with a
//! timeout between cycles, so a max-priority thread with nothing to do does
//! not spin a core, and a sent command (which unparks it) is handled at once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::handlers::Handles;
use super::queue::ButlerQueue;
use super::step::{ButlerCycle, StepOutcome};

/// How long the butler parks between cycles when nothing wakes it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Pacing {
    /// Nothing streaming.
    pub(super) idle: Duration,
    /// Every ring above its refill threshold, or stalled.
    pub(super) healthy: Duration,
}

impl Pacing {
    /// 1 ms idle, [`HEALTHY_SLEEP_MS`] healthy.
    pub(super) const DEFAULT: Self = Self {
        idle: Duration::from_millis(1),
        healthy: Duration::from_millis(HEALTHY_SLEEP_MS),
    };
}

/// The butler thread's main loop; returns only on shutdown.
///
/// Each cycle drains the queue and does one [`ButlerCycle::step`], then
/// paces according to its [`StepOutcome`].
///
/// # Pacing
///
/// This thread runs at maximum priority, so it must not spin. With no streams
/// it parks for [`Pacing::idle`]; with every ring above its refill threshold,
/// for [`Pacing::healthy`]. A sent command unparks it (the sender's
/// `unpark`), so the park bounds only how late a refill is checked, never how
/// late a command lands. A genuine refill need keeps a ring below threshold,
/// which drops it straight back to yield-and-loop — so the parking adds no
/// latency to refills that actually matter.
pub(super) fn butler_loop(
    queue: &ButlerQueue,
    shared: Handles,
    mut cycle: ButlerCycle,
    shutdown: Arc<AtomicBool>,
    pacing: Pacing,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        match cycle.step(&shared, || queue.pop()) {
            StepOutcome::Shutdown => break,

            // Nothing streaming, nothing urgent, or nothing this loop can do
            // about it: park until a command wakes it or the timeout ends.
            //
            // `Stalled` belongs here rather than with `Busy`. A ring whose
            // capacity is sized from the whole file can never reach its refill
            // threshold once the stream nears the end, so treating "below
            // threshold" as "come straight back" spun this max-priority thread
            // flat out for the tail of every clip.
            //
            // No wake is lost: a command pushed after the step's drain left
            // the park token set, and this park returns at once.
            outcome @ (StepOutcome::Idle | StepOutcome::Healthy | StepOutcome::Stalled) => {
                thread::park_timeout(if outcome == StepOutcome::Idle {
                    pacing.idle
                } else {
                    pacing.healthy
                });
            }

            // A ring is below threshold: come straight back, letting other
            // threads run first.
            StepOutcome::Busy => thread::yield_now(),
        }
    }
}

/// Short sleep (ms) taken when all active buffers are healthy — long enough to
/// stop the thread from busy-spinning, short enough to stay well inside the
/// smallest ring buffer's drain time so refills never fall behind.
const HEALTHY_SLEEP_MS: u64 = 3;
