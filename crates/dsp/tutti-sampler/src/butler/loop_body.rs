//! Butler thread body: the async main loop.
//!
//! Runs on the butler thread inside `smol::block_on`. The *work* of a cycle is
//! [`ButlerCycle::step`](super::step::ButlerCycle::step) — synchronous, and
//! shared with the test driver. What lives here is the half that genuinely needs
//! an executor: the pacing, i.e. racing the command channel against a timer so a
//! max-priority thread with nothing to do does not spin a core.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smol::channel::Receiver;
use smol::Timer;

use super::command::ButlerCommand;
use super::config::BufferConfig;
use super::handlers::Handles;
use super::step::{ButlerCycle, StepOutcome};
use tutti_core::SampleRate;

/// The butler thread's main loop; returns only on shutdown.
///
/// Each cycle runs one [`ButlerCycle::step`] and then parks according to its
/// [`StepOutcome`].
///
/// # Pacing
///
/// This thread runs at maximum priority, so it must not spin. With no streams it
/// parks on the command channel against a 1 ms timer; with every ring above its
/// refill threshold it does the same against [`HEALTHY_SLEEP_MS`]. A genuine
/// refill need keeps a ring below threshold, which drops it straight back to
/// yield-and-loop — so the parking adds no latency to refills that actually
/// matter.
pub(super) async fn butler_loop_async(
    rx: Receiver<ButlerCommand>,
    shared: Handles,
    config: BufferConfig,
    sample_rate: SampleRate,
    shutdown: Arc<AtomicBool>,
) {
    let mut cycle = ButlerCycle::new(config, sample_rate);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        match cycle.step(&shared, || rx.try_recv().ok()) {
            StepOutcome::Shutdown => break,

            // Nothing streaming, nothing urgent, or nothing this loop can do
            // about it: race the command channel against a short timer instead
            // of spinning.
            //
            // `Stalled` belongs here rather than with `Busy`. A ring whose
            // capacity is sized from the whole file can never reach its refill
            // threshold once the stream nears the end, so treating "below
            // threshold" as "come straight back" spun this max-priority thread
            // flat out for the tail of every clip.
            outcome @ (StepOutcome::Idle | StepOutcome::Healthy | StepOutcome::Stalled) => {
                let park_ms = if outcome == StepOutcome::Idle {
                    1
                } else {
                    HEALTHY_SLEEP_MS
                };
                let timeout = async {
                    Timer::after(Duration::from_millis(park_ms)).await;
                    Err(smol::channel::RecvError)
                };
                // The received command is dropped rather than handled here: the
                // recv exists to *wake* the loop, and the next cycle's drain is
                // the one place a command is applied. Pushing it back would
                // reorder it behind commands queued after it.
                if let Ok(cmd) = futures_lite::future::or(rx.recv(), timeout).await {
                    if matches!(cmd, ButlerCommand::Shutdown) {
                        break;
                    }
                    // Apply immediately — this command was taken off the queue
                    // by the wake-up recv, so nothing else will see it.
                    cycle.step(&shared, {
                        let mut once = Some(cmd);
                        move || once.take()
                    });
                }
            }

            // A ring is below threshold. Yield so any spawned async tasks can
            // progress, then come straight back.
            StepOutcome::Busy => futures_lite::future::yield_now().await,
        }
    }
}

/// Short sleep (ms) taken when all active buffers are healthy — long enough to
/// stop the thread from busy-spinning, short enough to stay well inside the
/// smallest ring buffer's drain time so refills never fall behind.
const HEALTHY_SLEEP_MS: u64 = 3;
