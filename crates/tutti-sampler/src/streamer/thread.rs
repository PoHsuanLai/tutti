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
use std::thread::JoinHandle;
use std::time::Duration;

use tutti_rt::{PriorityRequest, request_priority};
use tutti_types::SampleRate;

use super::bell::{Bell, StreamerSender};
use super::cache::LruCache;
use super::command::StreamerCommand;
use super::config::BufferConfig;
use super::decode::Seams;
use super::handlers::{Handles, SessionRate};
use super::loop_body::{BACKSTOP, streamer_loop};
use super::metrics::Metrics;
#[cfg(any(test, feature = "test-support"))]
use super::step::StepOutcome;
use super::step::StreamerCycle;
use crate::Error;
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
    /// How long the thread sleeps at most when nothing rings it.
    backstop: Duration,
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
        seams: Seams,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(channel_capacity.max(1));
        let bell = Bell::new();
        let tx = StreamerSender::new(tx, Arc::clone(&bell));

        let shared = Handles {
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            bell,
            session_rate: SessionRate::new(sample_rate),
            seams,
        };

        Self {
            tx,
            rx,
            shared,
            config,
            backstop: BACKSTOP,
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

    /// This streamer with its thread sleeping `backstop` at most when nothing
    /// rings it (tests: a long backstop shows a wake does not wait for it).
    #[cfg(test)]
    pub(super) fn with_backstop(mut self, backstop: Duration) -> Self {
        self.backstop = backstop;
        self
    }

    /// Spawn the streamer thread, which owns the cycle state and the queue's
    /// receiver from here on.
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`] when the thread cannot be spawned. Every stream and
    /// every refill depends on it, so a failure here (the OS out of threads)
    /// would otherwise degrade into an engine that accepts commands and stays
    /// mute; it is the caller's to see instead.
    pub(super) fn start(self) -> Result<Running, Error> {
        let spawn = self.shared.seams.spawn;
        let bell = Arc::clone(&self.shared.bell);
        let shared = self.shared;
        let cycle = StreamerCycle::new(self.config, self.tx.clone());
        let (queue, backstop) = (self.rx, self.backstop);

        let body = move || {
            // The thread owns the queue's one receiver, so however the
            // loop ends (a panic included) its drop tells every sender
            // the streamer is gone instead of letting them queue into
            // nothing; and its table's drop tells every live stream
            // (`Faulted(StreamerGone)`).
            // Disk streaming should stay ahead of background work, not
            // compete with the audio threads (Ardour's streamer sets no CPU
            // priority, LinuxSampler's disk thread runs at nice -10): a
            // raised, never real-time priority, nice -10 on Linux and
            // ABOVE_NORMAL on Windows. This replaced the highest the OS
            // grants (nice -20, HIGHEST) on purpose. A refusal is
            // harmless, so the outcome is only logged.
            tracing::debug!("streamer: {}", request_priority(PriorityRequest::Raised));
            streamer_loop(&queue, shared, cycle, backstop);
        };
        let handle = spawn("tutti-streamer", Box::new(body)).map_err(Error::Spawn)?;

        Ok(Running {
            bell,
            thread_handle: Some(handle),
        })
    }

    /// The caller drives: its cycle is stepped by `ByHand::step_once`.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn by_hand(self) -> ByHand {
        ByHand {
            rx: Some(self.rx),
            cycle: StreamerCycle::new(self.config, self.tx),
            shared: self.shared,
        }
    }
}

/// The streamer driven by its own thread: what
/// [`DiskStreamer::new`](crate::DiskStreamer::new) builds.
///
/// `Drop` stops the thread and joins it.
pub struct Running {
    /// What a stop signals and rings.
    bell: Arc<Bell>,
    thread_handle: Option<JoinHandle<()>>,
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
    /// Shutdown is one mechanism: a flag on the bell, and a ring to end a
    /// sleep already begun. The loop reads it at the top of each cycle, so a
    /// cycle already running finishes first. The queue's receiver is gone
    /// afterwards (the thread dropped it on the way out), so a sender still
    /// holding the queue, a waiting one included, is told the streamer is
    /// gone. Called by `Drop`, and idempotent.
    pub(super) fn stop(&mut self) {
        self.bell.shut();

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
    /// the loop *would* have done next.
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
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::streamer::StreamerGone;
    use crate::streamer::command::StreamId;
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

    /// A backstop far longer than any test waits: a streamer that wakes inside
    /// a test's bound was rung, not timed out.
    const LONG_WAIT: Duration = Duration::from_secs(60);

    fn built(capacity: usize) -> Built {
        Built::new(
            capacity,
            SampleRate::new(48_000.0),
            BufferConfig::default(),
            Seams::default(),
        )
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

    /// Waits until the streamer's thread is parked on its bell.
    fn until_asleep(bell: &Bell) {
        assert!(
            within(Duration::from_secs(10), || bell.is_asleep()),
            "the streamer never went to sleep"
        );
    }

    /// A sent command wakes a sleeping streamer: it is drained well inside the
    /// backstop, not when the backstop ends.
    ///
    /// The backstop is 60 s and the test allows 10 s: the bound is generous for
    /// a loaded machine, and still six times shorter than a streamer that only
    /// ever wakes on its own. Each send is made once the thread is parked, so
    /// the doorbell, not a cycle already running, is what wakes it.
    ///
    /// Mutation (run): `StreamerSender::send` without its `self.bell.ring()` ->
    /// the command sits for the 60 s -> fails at the 10 s bound. Mutation
    /// (run): the loop's `Bell::sleep` replaced by a `thread::sleep` of the
    /// backstop -> the same.
    #[test]
    fn a_send_wakes_a_sleeping_streamer() {
        let built = built(8).with_backstop(LONG_WAIT);
        let tx = built.command_sender();
        let bell = Arc::clone(&built.shared().bell);
        let mut streamer = built.start().expect("the thread starts");

        for stream in [StreamId(90), StreamId(91)] {
            until_asleep(&bell);
            tx.try_send(StreamerCommand::Close { stream })
                .expect("room in an empty queue");
            assert!(
                within(Duration::from_secs(10), || tx.is_empty()),
                "command {stream} waited on the streamer's 60 s backstop"
            );
        }
        streamer.stop();
    }

    /// Dropping a sleeping streamer wakes its thread and joins it promptly:
    /// shutdown is a flag and a ring, not the backstop's end.
    ///
    /// The drop runs on a helper thread so a streamer that does not wake fails
    /// at the bound instead of hanging the test for the backstop's 60 s.
    ///
    /// Mutation (run): `Bell::shut` without its `self.door.ring()` -> the join
    /// waits out the 60 s -> fails at the 10 s bound. Mutation (run):
    /// `Running::stop` without `bell.shut()` -> the thread never ends -> the
    /// same.
    #[test]
    fn dropping_a_sleeping_streamer_joins_promptly() {
        let built = built(8).with_backstop(LONG_WAIT);
        let bell = Arc::clone(&built.shared().bell);
        let streamer = built.start().expect("the thread starts");
        until_asleep(&bell);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(streamer);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the drop waited on the streamer's 60 s backstop"
        );
        dropper.join().unwrap();
    }

    /// With no stream live the streamer sleeps and does not spin: measured by
    /// the cycles it has begun, not by CPU time. It runs one cycle at start,
    /// and a quiet window later it has run no more.
    ///
    /// The window is the one place the test waits for nothing to happen: a
    /// spinning streamer runs thousands of cycles in it, so the bound (a few)
    /// is not a timing guess.
    ///
    /// Mutation (run): the loop sleeping 1 ms (`Duration::from_millis(1)` in
    /// place of `backstop`, the old idle poll) -> about a hundred cycles in
    /// the window -> fails. Mutation (run): `StepOutcome::Idle` returned as
    /// `Busy` -> the loop yields forever -> fails.
    #[test]
    fn an_idle_streamer_does_not_spin() {
        let built = built(8).with_backstop(LONG_WAIT);
        let bell = Arc::clone(&built.shared().bell);
        let mut streamer = built.start().expect("the thread starts");
        until_asleep(&bell);
        let at_rest = bell.cycles();
        thread::sleep(Duration::from_millis(150));
        let later = bell.cycles();
        streamer.stop();
        assert!(
            later - at_rest <= 2,
            "{} cycles in 150 ms with nothing live",
            later - at_rest
        );
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
        let mut streamer = built.start().expect("the thread starts");
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
    /// without its controller's `stop`: a shut bell ends the loop, and the
    /// thread's receiver goes with it.
    ///
    /// Mutation (run): `Built::new` leaking a clone of the receiver → the
    /// queue never disconnects → fails at the bound.
    #[test]
    fn a_streamer_that_exits_is_gone_to_its_senders() {
        let built = built(8);
        let tx = built.command_sender();
        let bell = Arc::clone(&built.shared().bell);
        let _streamer = built.start().expect("the thread starts");
        bell.shut();
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
