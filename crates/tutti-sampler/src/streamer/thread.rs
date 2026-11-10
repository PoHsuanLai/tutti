//! The streamer's drivers, as a typestate: a streamer is [`Built`], then
//! either [`Running`] on its own thread or `ByHand`, stepped on the caller's
//! (doc 025 §2.5; the `Lifecycle` shape of design 016).
//!
//! The two drivers own the same streamer-local state (`Local`: the stream
//! table and the scratch), so a streamer is one or the other for its whole
//! life, and the type says which. Handing the state to a thread consumes the
//! `Built`; stepping by hand needs a `ByHand`, which has no thread to race.
//! What was two runtime flags, two asserts and a `should_panic` test is a
//! method that exists on one type and not the other.
//!
//! The thread body and command handlers live in sibling modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use tutti_rt::{PriorityRequest, request_priority};
use tutti_types::SampleRate;

use super::StreamerSender;
use super::cache::LruCache;
use super::command::StreamerCommand;
use super::config::BufferConfig;
use super::handlers::{Handles, SessionRate};
use super::loop_body::{Pacing, streamer_loop};
use super::metrics::Metrics;
#[cfg(any(test, feature = "test-support"))]
use super::step::StepOutcome;
use super::step::StreamerCycle;
use crossbeam_channel::Receiver;

/// A streamer that is built and drives nothing yet: its queue, its shared
/// handles and the state a driver will own.
///
/// Consumed by [`start`](Self::start) (a thread) or `by_hand` (the caller).
/// Dropping it drops the queue's one receiver, so a sender is told the
/// streamer is gone.
pub(super) struct Built {
    /// The command queue's sending end, cloned for every sender.
    tx: StreamerSender,
    /// Its receiving end, until a driver takes it.
    rx: Receiver<StreamerCommand>,
    shared: Handles,
    config: BufferConfig,
    /// How long the thread parks between cycles.
    pacing: Pacing,
}

impl Built {
    /// Build the queue and the shared handles without spawning anything.
    ///
    /// `channel_capacity` bounds the command queue in messages (at least
    /// one); a send to a full queue waits. `sample_rate` is the
    /// *session* rate, against which each file's own rate becomes the stream's
    /// [`SrcRatio`](tutti_types::SrcRatio).
    pub(super) fn new(
        channel_capacity: usize,
        sample_rate: SampleRate,
        config: BufferConfig,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(channel_capacity.max(1));

        let shared = Handles {
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            session_rate: SessionRate::new(sample_rate),
        };

        Self {
            tx,
            rx,
            shared,
            config,
            pacing: Pacing::DEFAULT,
        }
    }

    /// A cloneable sender onto the streamer's command queue.
    ///
    /// Valid before a driver exists: commands sent then simply queue until it
    /// drains them. Control threads only: a send waits while the queue is
    /// full.
    pub(super) fn command_sender(&self) -> StreamerSender {
        self.tx.clone()
    }

    /// The shared handles, for the control side's own reads.
    pub(super) fn shared(&self) -> Handles {
        self.shared.clone()
    }

    /// This streamer with its thread waiting `pacing` between cycles
    /// (tests: a long wait shows a command does not wait for it).
    #[cfg(test)]
    pub(super) fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }

    /// Spawn the streamer thread, which owns the cycle state and the queue's
    /// receiver from here on.
    ///
    /// # Panics
    ///
    /// If the thread cannot be spawned. Every stream and every refill depends on
    /// it, so a failure here (the OS out of threads) would otherwise degrade
    /// into an engine that accepts commands and stays mute.
    pub(super) fn start(self) -> Running {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let shared = self.shared;
        let cycle = StreamerCycle::new(self.config);
        let (queue, pacing) = (self.rx, self.pacing);

        let handle = thread::Builder::new()
            .name("tutti-streamer".into())
            .spawn(move || {
                // The thread owns the queue's one receiver, so however the
                // loop ends (a panic included) its drop tells every sender
                // the streamer is gone instead of letting them queue into
                // nothing.
                // Disk streaming should stay ahead of background work, not
                // compete with the audio threads (Ardour's streamer sets no CPU
                // priority, LinuxSampler's disk thread runs at nice -10): a
                // raised, never real-time priority, nice -10 on Linux and
                // ABOVE_NORMAL on Windows. This replaced the highest the OS
                // grants (nice -20, HIGHEST) on purpose. A refusal is
                // harmless, so the outcome is only logged.
                tracing::debug!("streamer: {}", request_priority(PriorityRequest::Raised));
                streamer_loop(&queue, shared, cycle, thread_shutdown, pacing);
            })
            // Fatal init: spawning the disk-I/O streamer thread is a prerequisite
            // for all streaming/recording. A spawn failure means the OS is out of
            // threads — unrecoverable at this layer, and every downstream stream
            // command would silently stall on a channel with no consumer. Panic
            // loudly at startup rather than degrade into a mute engine.
            .expect("Failed to spawn streamer thread");

        Running {
            tx: self.tx,
            thread_handle: Some(handle),
            shutdown,
        }
    }

    /// The caller drives: its cycle is stepped by `ByHand::step_once`.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn by_hand(self) -> ByHand {
        ByHand {
            rx: Some(self.rx),
            cycle: StreamerCycle::new(self.config),
            shared: self.shared,
        }
    }
}

/// The streamer driven by its own thread: what
/// [`DiskStreamer::new`](crate::DiskStreamer::new) builds.
///
/// `Drop` stops the thread and joins it.
pub struct Running {
    /// For the `Shutdown` command a stop sends.
    tx: StreamerSender,
    thread_handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("alive", &self.thread_handle.is_some())
            .finish_non_exhaustive()
    }
}

impl Running {
    /// Signal shutdown and join the streamer thread, blocking until it exits.
    ///
    /// The flag ends the loop at its next cycle, and a `Shutdown` command,
    /// queued when there is room, ends a wait or a cycle already draining.
    /// The queue's receiver is gone afterwards (the thread dropped it on the
    /// way out), so a sender still holding the queue, a waiting one included,
    /// is told the streamer is gone. Called by `Drop`, and idempotent.
    pub(super) fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.tx.try_send(StreamerCommand::Shutdown);

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The streamer driven by the caller, a cycle at a time: what
/// `DiskStreamer::manual` builds. Requires the
/// `test-support` feature.
///
/// There is no thread to race, so a test can say "after this many cycles the
/// ring holds audio"; it is the same [`StreamerCycle::step`] the thread runs.
#[cfg(any(test, feature = "test-support"))]
pub struct ByHand {
    rx: Option<Receiver<StreamerCommand>>,
    cycle: StreamerCycle,
    shared: Handles,
}

#[cfg(any(test, feature = "test-support"))]
impl std::fmt::Debug for ByHand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByHand").finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ByHand {
    /// One cycle on the calling thread: drain every queued command, then apply
    /// directions and refills. Returns once that work is done, reporting what
    /// the pacing layer *would* have done next.
    pub(super) fn step_once(&mut self) -> StepOutcome {
        let queue = self.rx.as_ref();
        self.cycle
            .step(&self.shared, || queue.and_then(|rx| rx.try_recv().ok()))
    }

    /// How many streams the streamer holds (tests).
    #[cfg(test)]
    pub(super) fn open_streams(&self) -> usize {
        self.cycle.streams().len()
    }

    /// Drop the queue's receiver, as a streamer thread that exited does: every
    /// sender is told the streamer is gone (tests).
    #[cfg(test)]
    pub(super) fn gone(&mut self) {
        self.rx = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::streamer::StreamerGone;
    use crate::streamer::command::StreamId;
    use crate::streamer::loop_body::Pacing;
    use crate::streamer::stream::StreamerPort;

    /// Both drivers are `Sync` (a Bevy resource holds a `DiskStreamer`), with
    /// the hand-stepped cycle held plainly: a compile-time check.
    ///
    /// Mutation (run): a `std::cell::Cell<u8>` field in `StreamerCycle` →
    /// does not compile.
    const _: () = {
        fn sync<T: Sync>() {}
        let _ = sync::<Running>;
        let _ = sync::<ByHand>;
    };

    /// A streamer that, left alone, would sleep far longer than any test waits.
    const LONG_WAIT: Pacing = Pacing {
        idle: Duration::from_secs(60),
        healthy: Duration::from_secs(60),
    };

    fn built(capacity: usize) -> Built {
        Built::new(capacity, SampleRate::new(48_000.0), BufferConfig::default())
    }

    /// Polls `done` until it holds or `bound` passes; whether it held.
    fn within(bound: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < bound {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        done()
    }

    /// A sent command wakes a waiting streamer: it is drained well inside
    /// the wait's timeout, not when the timeout ends.
    ///
    /// The streamer waits 60 s between cycles and the test allows 10 s: the
    /// bound is generous for a loaded machine, and still six times shorter
    /// than a streamer that only ever wakes on its own. The first send may land
    /// before the thread waits; the second is sent once the first has been
    /// drained, by which point the thread is (or is about to be) waiting, and
    /// the queue makes the two cases one.
    ///
    /// Mutation (run): the loop's `recv_timeout` replaced by a
    /// `thread::sleep` of the same length → the second command sits for the
    /// 60 s → fails at the 10 s bound.
    #[test]
    fn a_send_wakes_a_waiting_streamer() {
        let built = built(8).with_pacing(LONG_WAIT);
        let tx = built.command_sender();
        let mut streamer = built.start();

        for stream in [StreamId(90), StreamId(91)] {
            tx.try_send(StreamerCommand::Close { stream })
                .expect("room in an empty queue");
            assert!(
                within(Duration::from_secs(10), || tx.is_empty()),
                "command {stream} waited on the streamer's 60 s wait"
            );
            // Let the streamer finish its cycle and wait again.
            thread::sleep(Duration::from_millis(20));
        }
        streamer.stop();
    }

    /// A streamer that is gone, in either state, is gone to every sender.
    ///
    /// A started streamer's thread drops its receiver on the way out; a
    /// built one drops it with itself.
    /// Mutation (run): `Built::new` leaking a clone of the receiver
    /// (`std::mem::forget(rx.clone())`) → the queue never disconnects → fails.
    #[test]
    fn a_stopped_streamer_reports_gone() {
        let unstarted = built(8);
        let port = StreamerPort::new(unstarted.command_sender());
        drop(unstarted);
        assert_eq!(
            port.send(StreamerCommand::Close {
                stream: StreamId(0)
            }),
            Err(StreamerGone),
            "a streamer dropped before it started"
        );

        let built = built(8);
        let port = StreamerPort::new(built.command_sender());
        let mut streamer = built.start();
        assert_eq!(
            port.send(StreamerCommand::Close {
                stream: StreamId(0)
            }),
            Ok(())
        );

        streamer.stop();
        assert_eq!(
            port.send(StreamerCommand::Close {
                stream: StreamId(0)
            }),
            Err(StreamerGone)
        );
    }

    /// A send waiting on a full queue returns once the streamer is gone,
    /// reporting it gone, rather than waiting for good.
    ///
    /// Mutation (run): `Built::new` leaking a clone of the receiver → the
    /// waiting send never returns → fails at the 10 s bound.
    #[test]
    fn a_waiting_send_is_released_when_the_streamer_stops() {
        let streamer = built(1);
        let port = StreamerPort::new(streamer.command_sender());
        assert_eq!(
            port.send(StreamerCommand::Close {
                stream: StreamId(0)
            }),
            Ok(())
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let port = port.clone();
            thread::spawn(move || {
                let _ = done_tx.send(port.send(StreamerCommand::Close {
                    stream: StreamId(1),
                }));
            })
        };
        // The queue is full and nothing drains it: the send is waiting.
        thread::sleep(Duration::from_millis(20));
        assert!(
            done_rx.try_recv().is_err(),
            "a send into a full queue waits"
        );
        drop(streamer);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(10)),
            Ok(Err(StreamerGone)),
            "the waiting send must return once the streamer is gone, and say so"
        );
        waiter.join().unwrap();
    }

    /// A streamer thread that exits on its own is gone to its senders,
    /// without its controller's `stop`: a `Shutdown` command ends the loop,
    /// and the thread's receiver goes with it.
    ///
    /// Mutation (run): `Built::new` leaking a clone of the receiver → the
    /// queue never disconnects → fails at the bound.
    #[test]
    fn a_streamer_that_exits_is_gone_to_its_senders() {
        let built = built(8);
        let tx = built.command_sender();
        let _streamer = built.start();
        tx.try_send(StreamerCommand::Shutdown).unwrap();
        // Probed without waiting, so a queue that never disconnects fails
        // at the bound rather than filling and blocking the probe.
        assert!(
            within(Duration::from_secs(10), || matches!(
                tx.try_send(StreamerCommand::Close {
                    stream: StreamId(0)
                }),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )),
            "the thread exited but the queue still accepts commands"
        );
        assert_eq!(
            StreamerPort::new(tx).send(StreamerCommand::Close {
                stream: StreamId(0)
            }),
            Err(StreamerGone)
        );
    }
}
