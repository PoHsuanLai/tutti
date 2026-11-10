//! Streamer thread controller: spawn, stop, and expose shared handles.
//!
//! The controller is UI-side — callers hold a `StreamerThread`, send commands
//! via `command_sender`, and read metrics / cache / stream states through
//! accessors. The thread body and command handlers live in sibling modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use dashmap::DashMap;
use tutti_rt::{PriorityRequest, request_priority};
use tutti_types::SampleRate;

use super::StreamerSender;
use super::cache::LruCache;
use super::command::StreamerCommand;
use super::config::BufferConfig;
use super::handlers::{Handles, SessionRate};
use super::loop_body::{Pacing, streamer_loop};
use super::metrics::Metrics;
use super::plan::ChannelPlan;
#[cfg(any(test, feature = "test-support"))]
use super::step::StepOutcome;
use super::step::StreamerCycle;
use crossbeam_channel::Receiver;

/// Owner of the streamer thread and of every handle shared with it.
///
/// Control-thread side: build with [`with_config`](Self::with_config), spawn
/// with [`start`](Self::start), drive through
/// [`command_sender`](Self::command_sender), and read stream state through
/// [`plans`](Self::plans). `Drop` stops the thread and joins it.
pub struct StreamerThread {
    /// The command queue's sending end, cloned for every sender.
    tx: StreamerSender,
    /// Its receiving end, until a driver takes it: the thread
    /// [`start`](Self::start) spawns (which drops it on exit, however it
    /// exits), or `StreamerThread::step_once`. [`stop`](Self::stop) drops
    /// it. With every receiver dropped a send reports the streamer gone.
    rx: Option<Receiver<StreamerCommand>>,
    thread_handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Handles,
    config: BufferConfig,
    /// How long the thread parks between cycles.
    pacing: Pacing,
    /// The synchronous cycle, when this controller is being driven by hand
    /// rather than by a thread. `Some` only after [`step_once`](Self::step_once)
    /// has been called on an unstarted controller; `start` refuses once it is
    /// set, so the two drivers can never both own `Local`.
    #[cfg(any(test, feature = "test-support"))]
    manual: Option<StreamerCycle>,
}

impl StreamerThread {
    /// Build the controller and its shared handles without spawning anything —
    /// [`start`](Self::start) does that.
    ///
    /// `channel_capacity` bounds the command queue in messages (at least
    /// one); a send to a full queue waits. `sample_rate` is the
    /// *session* rate, against which each file's own rate becomes the stream's
    /// [`SrcRatio`](tutti_types::SrcRatio).
    pub fn with_config(
        channel_capacity: usize,
        sample_rate: SampleRate,
        config: BufferConfig,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(channel_capacity.max(1));

        let shared = Handles {
            plans: Arc::new(DashMap::new()),
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            session_rate: SessionRate::new(sample_rate),
        };

        Self {
            tx,
            rx: Some(rx),
            thread_handle: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            shared,
            config,
            pacing: Pacing::DEFAULT,
            #[cfg(any(test, feature = "test-support"))]
            manual: None,
        }
    }

    /// The cycle state a driver owns: the thread [`start`](Self::start)
    /// spawns, or `StreamerThread::step_once`. Built here, on the control
    /// thread, by both, so the hand-stepped tests build what the thread runs.
    fn cycle(&self) -> StreamerCycle {
        StreamerCycle::new(self.config)
    }

    /// A cloneable sender onto the streamer's command queue.
    ///
    /// Valid before `start`: commands sent then simply queue until the thread
    /// drains them. Control threads only: a send waits while the queue is
    /// full.
    pub(crate) fn command_sender(&self) -> StreamerSender {
        self.tx.clone()
    }

    /// This controller with its thread waiting `pacing` between cycles
    /// (tests: a long wait shows a command does not wait for it).
    #[cfg(test)]
    pub(super) fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }

    /// Spawn the streamer thread at maximum priority. Idempotent — a second call
    /// while the thread runs is a no-op.
    ///
    /// # Panics
    ///
    /// If the thread cannot be spawned. Every stream and every refill depends on
    /// it, so a failure here (the OS out of threads) would otherwise degrade
    /// into an engine that accepts commands and stays mute.
    pub fn start(&mut self) {
        if self.thread_handle.is_some() {
            return;
        }
        // Stopped: nothing to drive.
        let Some(queue) = self.rx.take() else {
            return;
        };
        // `Local` has exactly one owner. A controller already stepped by hand
        // holds it here; handing a second copy to a thread would leave two
        // region maps disagreeing about which rings exist.
        #[cfg(any(test, feature = "test-support"))]
        assert!(
            self.manual.is_none(),
            "start on a hand-stepped streamer: the cycle state is already owned here"
        );

        let shutdown = Arc::clone(&self.shutdown);
        let shared = self.shared.clone();
        let cycle = self.cycle();
        let pacing = self.pacing;

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
                streamer_loop(&queue, shared, cycle, shutdown, pacing);
            })
            // Fatal init: spawning the disk-I/O streamer thread is a prerequisite
            // for all streaming/recording. A spawn failure means the OS is out of
            // threads — unrecoverable at this layer, and every downstream stream
            // command would silently stall on a channel with no consumer. Panic
            // loudly at startup rather than degrade into a mute engine.
            .expect("Failed to spawn streamer thread");

        self.thread_handle = Some(handle);
    }

    /// Signal shutdown and join the streamer thread, blocking until it exits.
    ///
    /// The flag ends the loop at its next cycle, and a `Shutdown` command,
    /// queued when there is room, ends a wait or a cycle already draining.
    /// The queue's receivers are gone afterwards (a started streamer's thread
    /// dropped its own on the way out, and this drops the controller's, for
    /// one that never started), so a sender still holding the queue, a
    /// waiting one included, is told the streamer is gone. Called by `Drop`,
    /// and idempotent — stopping an already-stopped controller does nothing
    /// more.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.tx.try_send(StreamerCommand::Shutdown);

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        self.rx = None;
    }

    /// Run exactly one streamer cycle on the calling thread: drain every
    /// queued command, then apply directions and refills.
    /// Returns once that work is done, reporting what the pacing layer *would*
    /// have done next.
    ///
    /// This is the same [`StreamerCycle::step`] the streamer thread runs — the
    /// thread adds only pacing — so driving a controller by hand exercises the
    /// shipped path rather than a parallel one.
    ///
    /// Intended for tests, which is what the feature gate says. A streamer-driven
    /// stream is otherwise observable only through wall clock: send a command,
    /// sleep, look. Stepping makes it observable by *counting*, so a test can
    /// state "after three cycles the ring holds audio" rather than "after 50 ms
    /// it probably does".
    ///
    /// # Panics
    ///
    /// If the streamer thread is running. Both drivers own the same streamer-local
    /// state, so a controller is one or the other for its whole life; stepping a
    /// live controller would race the thread inside `Local`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn step_once(&mut self) -> StepOutcome {
        assert!(
            self.thread_handle.is_none(),
            "step_once on a started streamer: the thread already owns the cycle state"
        );
        if self.manual.is_none() {
            self.manual = Some(self.cycle());
        }
        let cycle = self.manual.as_mut().expect("set just above");
        let queue = self.rx.as_ref();
        cycle.step(&self.shared, || queue.and_then(|rx| rx.try_recv().ok()))
    }

    /// The shared per-channel plan map, keyed by channel index.
    ///
    /// This is the reader-factory: a plan whose `link` the streamer has installed
    /// yields the ring consumer and the `RtState` a disk voice is built from.
    /// Both the streamer and the control thread hold it, so an entry may appear or
    /// change between two reads.
    pub fn plans(&self) -> Arc<DashMap<usize, ChannelPlan>> {
        Arc::clone(&self.shared.plans)
    }

    /// The streamer's wave cache (tests).
    #[cfg(test)]
    pub(crate) fn cache(&self) -> Arc<LruCache> {
        Arc::clone(&self.shared.cache)
    }

    /// The session-rate cell, shared: a [`Status`](crate::Status) built from
    /// it reads the rate as it is, not as it was.
    pub(crate) fn session_rate(&self) -> SessionRate {
        self.shared.session_rate.clone()
    }

    /// Move the session rate, re-deriving every open stream's ratio (see
    /// [`SessionRate::set`]). Control thread; streams opened later derive
    /// theirs against it.
    pub(crate) fn set_session_rate(&self, rate: SampleRate) {
        self.shared.session_rate.set(rate, &self.shared.plans);
    }
}

impl Drop for StreamerThread {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::ports::{Command, Commands};
    use crate::streamer::StreamerGone;
    use crate::streamer::loop_body::Pacing;

    /// The controller is `Sync` (a Bevy resource holds it), with its
    /// hand-stepped cycle held plainly: a compile-time check.
    ///
    /// Mutation (run): a `std::cell::Cell<u8>` field in `StreamerCycle` →
    /// does not compile.
    const _: () = {
        fn sync<T: Sync>() {}
        let _ = sync::<StreamerThread>;
    };

    /// A streamer that, left alone, would sleep far longer than any test waits.
    const LONG_WAIT: Pacing = Pacing {
        idle: Duration::from_secs(60),
        healthy: Duration::from_secs(60),
    };

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
        let mut streamer =
            StreamerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default())
                .with_pacing(LONG_WAIT);
        let tx = streamer.command_sender();
        streamer.start();

        for channel_index in [90, 91] {
            tx.try_send(StreamerCommand::StopStreaming { channel_index })
                .expect("room in an empty queue");
            assert!(
                within(Duration::from_secs(10), || tx.is_empty()),
                "command {channel_index} waited on the streamer's 60 s wait"
            );
            // Let the streamer finish its cycle and wait again.
            thread::sleep(Duration::from_millis(20));
        }
        streamer.stop();
    }

    /// A stopped streamer is gone to every sender, started or not.
    ///
    /// A started streamer's thread drops its receiver on the way out, so the
    /// mutation that bites is on the controller that never started.
    /// Mutation (run): `stop` without its `self.rx = None` → the unstarted
    /// half queues into a queue nobody drains, `Ok(())` → fails.
    #[test]
    fn a_stopped_streamer_reports_gone() {
        let mut unstarted =
            StreamerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default());
        unstarted.stop();
        assert_eq!(
            Commands::new(unstarted.command_sender()).send(Command::Stop { channel_index: 0 }),
            Err(StreamerGone),
            "a controller stopped before it started"
        );

        let mut streamer =
            StreamerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default());
        streamer.start();
        let commands = Commands::new(streamer.command_sender());
        assert_eq!(commands.send(Command::Stop { channel_index: 0 }), Ok(()));

        streamer.stop();
        assert_eq!(
            commands.send(Command::Stop { channel_index: 0 }),
            Err(StreamerGone)
        );
    }

    /// A send waiting on a full queue returns once the streamer stops,
    /// reporting it gone, rather than waiting for good.
    ///
    /// Mutation (run): `stop` without its `self.rx = None` → the waiting
    /// send never returns → fails at the 10 s bound.
    #[test]
    fn a_waiting_send_is_released_when_the_streamer_stops() {
        let mut streamer =
            StreamerThread::with_config(1, SampleRate::new(48_000.0), BufferConfig::default());
        let commands = Commands::new(streamer.command_sender());
        assert_eq!(commands.send(Command::Stop { channel_index: 0 }), Ok(()));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let commands = commands.clone();
            thread::spawn(move || {
                let _ = done_tx.send(commands.send(Command::Stop { channel_index: 1 }));
            })
        };
        // The queue is full and nothing drains it: the send is waiting.
        thread::sleep(Duration::from_millis(20));
        assert!(
            done_rx.try_recv().is_err(),
            "a send into a full queue waits"
        );
        streamer.stop();
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
    /// Mutation (run): `start` handing the thread a clone of the receiver
    /// and keeping its own → the queue never disconnects → fails at the
    /// bound.
    #[test]
    fn a_streamer_that_exits_is_gone_to_its_senders() {
        let mut streamer =
            StreamerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default());
        streamer.start();
        let tx = streamer.command_sender();
        tx.try_send(StreamerCommand::Shutdown).unwrap();
        // Probed without waiting, so a queue that never disconnects fails
        // at the bound rather than filling and blocking the probe.
        assert!(
            within(Duration::from_secs(10), || matches!(
                tx.try_send(StreamerCommand::StopStreaming { channel_index: 0 }),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )),
            "the thread exited but the queue still accepts commands"
        );
        assert_eq!(
            Commands::new(tx).send(Command::Stop { channel_index: 0 }),
            Err(StreamerGone)
        );
    }
}
