//! Butler thread body: the main loop.
//!
//! The *work* of a cycle is
//! [`ButlerCycle::step`](super::step::ButlerCycle::step) — synchronous, and
//! shared with the test driver. What lives here is the pacing: waiting on the
//! command queue with a timeout between cycles, so a max-priority thread with
//! nothing to do does not spin a core, and a sent command is handled at once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use super::command::ButlerCommand;
use super::handlers::Handles;
use super::step::{ButlerCycle, StepOutcome};

/// How long the butler waits between cycles when no command arrives.
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
/// it waits on the queue for [`Pacing::idle`]; with every ring above its
/// refill threshold, for [`Pacing::healthy`]. A sent command ends the wait,
/// so the timeout bounds only how late a refill is checked, never how late a
/// command lands. No wake is lost: a command sent at any point after a
/// cycle's drain is in the queue, and the wait returns with it at once. A
/// genuine refill need keeps a ring below threshold, which drops it straight
/// back to yield-and-loop — so the waiting adds no latency to refills that
/// actually matter.
///
/// Returns too when every sender is gone (nothing can reach it again).
pub(super) fn butler_loop(
    queue: &Receiver<ButlerCommand>,
    shared: Handles,
    mut cycle: ButlerCycle,
    shutdown: Arc<AtomicBool>,
    pacing: Pacing,
) {
    // A command the wait received, handled first by the next cycle.
    let mut woken_by: Option<ButlerCommand> = None;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let outcome = cycle.step(&shared, || {
            woken_by.take().or_else(|| queue.try_recv().ok())
        });
        match outcome {
            StepOutcome::Shutdown => break,

            // Nothing streaming, nothing urgent, or nothing this loop can do
            // about it: wait until a command arrives or the timeout ends.
            //
            // `Stalled` belongs here rather than with `Busy`. A ring whose
            // capacity is sized from the whole file can never reach its refill
            // threshold once the stream nears the end, so treating "below
            // threshold" as "come straight back" spun this max-priority thread
            // flat out for the tail of every clip.
            outcome @ (StepOutcome::Idle | StepOutcome::Healthy | StepOutcome::Stalled) => {
                let wait = if outcome == StepOutcome::Idle {
                    pacing.idle
                } else {
                    pacing.healthy
                };
                match queue.recv_timeout(wait) {
                    Ok(cmd) => woken_by = Some(cmd),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
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
