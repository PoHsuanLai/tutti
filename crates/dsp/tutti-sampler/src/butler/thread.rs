//! Butler thread controller: spawn, stop, and expose shared handles.
//!
//! The controller is UI-side — callers hold a `ButlerThread`, send commands
//! via `command_sender`, and read metrics / cache / stream states through
//! accessors. The thread body and command handlers live in sibling modules.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use dashmap::DashMap;
use thread_priority::ThreadPriority;
use tutti_core::PrerollTable;
use tutti_core::SampleRate;

use super::cache::LruCache;
use super::command::ButlerCommand;
use super::config::BufferConfig;
use super::handlers::{Handles, SessionRate};
use super::loop_body::{butler_loop, Pacing};
use super::metrics::Metrics;
use super::plan::ChannelPlan;
use super::preroll::PdcFeed;
use super::queue::{ButlerQueue, ButlerSender};
use super::step::ButlerCycle;
#[cfg(any(test, feature = "test-support"))]
use super::step::StepOutcome;

/// Owner of the butler thread and of every handle shared with it.
///
/// Control-thread side: build with [`with_config`](Self::with_config), spawn
/// with [`start`](Self::start), drive through
/// [`command_sender`](Self::command_sender), and read stream state through
/// [`plans`](Self::plans). `Drop` stops the thread and joins it.
pub struct ButlerThread {
    /// The command queue. The butler thread pops it; every sender pushes
    /// and unparks.
    queue: Arc<ButlerQueue>,
    thread_handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Handles,
    config: BufferConfig,
    /// The delay-compensation table subscribed with
    /// [`with_pdc`](Self::with_pdc), if any, handed to each cycle built.
    pdc: Option<Arc<PrerollTable>>,
    /// How long the thread parks between cycles.
    pacing: Pacing,
    /// The synchronous cycle, when this controller is being driven by hand
    /// rather than by a thread. `Some` only after [`step_once`](Self::step_once)
    /// has been called on an unstarted controller; `start` refuses once it is
    /// set, so the two drivers can never both own `Local`.
    #[cfg(any(test, feature = "test-support"))]
    manual: Option<ButlerCycle>,
}

impl ButlerThread {
    /// Build the controller and its shared handles without spawning anything —
    /// [`start`](Self::start) does that.
    ///
    /// `channel_capacity` bounds the command queue in messages. A full queue
    /// makes a control-thread send wait and an audio-thread send refuse
    /// ([`ButlerSender`]). `sample_rate` is the
    /// *session* rate, against which each file's own rate becomes the stream's
    /// [`SrcRatio`](tutti_core::SrcRatio).
    pub fn with_config(
        channel_capacity: usize,
        sample_rate: SampleRate,
        config: BufferConfig,
    ) -> Self {
        let queue = ButlerQueue::new(channel_capacity);

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
            queue,
            thread_handle: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            shared,
            config,
            pdc: None,
            pacing: Pacing::DEFAULT,
            #[cfg(any(test, feature = "test-support"))]
            manual: None,
        }
    }

    /// Subscribe to a per-channel delay-compensation table, indexed by channel
    /// index and denominated in [`Samples`](tutti_core::Samples).
    ///
    /// Set by whoever runs `tutti_core::latency::compensate` over the audio
    /// graph (a session republishes it with every plan); the butler reads it
    /// each refill cycle and pre-rolls each stream by its channel's entry.
    /// The subscription is shared, not copied, so later sets are observed.
    ///
    /// Each cycle is handed the table when it is built: the one
    /// [`start`](Self::start) runs, and a controller already stepped by hand
    /// hands it to that cycle here, keeping the cycle's streams (dropping
    /// the cycle would drop their rings' writers while the plans still name
    /// them).
    pub fn with_pdc(mut self, snapshot: Arc<PrerollTable>) -> Self {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(cycle) = self.manual.as_mut() {
            cycle.set_pdc(PdcFeed::over(Some(&snapshot)));
        }
        self.pdc = Some(snapshot);
        self
    }

    /// The cycle state a driver owns: the thread [`start`](Self::start)
    /// spawns, or [`step_once`](Self::step_once). Built here, on the control
    /// thread, by both, so the hand-stepped tests build what the thread runs,
    /// the compensation table included.
    fn cycle(&self) -> ButlerCycle {
        ButlerCycle::new(self.config, PdcFeed::over(self.pdc.as_ref()))
    }

    /// The hand-stepped cycle's preroll for `channel`, `None` before the
    /// first hand step (tests).
    #[cfg(test)]
    pub(crate) fn manual_preroll(&self, channel: usize) -> Option<u64> {
        self.manual
            .as_ref()
            .map(|cycle| cycle.pdc().preroll(channel))
    }

    /// The table every cycle is handed, if subscribed (tests).
    #[cfg(test)]
    pub(crate) fn pdc_table(&self) -> Option<&Arc<PrerollTable>> {
        self.pdc.as_ref()
    }

    /// A cloneable sender onto the butler's command queue.
    ///
    /// Valid before `start`: commands sent then simply queue until the thread
    /// drains them. Its blocking send is control-thread work; its `try_send`
    /// is the audio thread's.
    pub(crate) fn command_sender(&self) -> ButlerSender {
        ButlerSender::new(Arc::clone(&self.queue))
    }

    /// This controller with its thread parking for `pacing` between cycles
    /// (tests: a long park shows a command does not wait for it).
    #[cfg(test)]
    pub(super) fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }

    /// Spawn the butler thread at maximum priority. Idempotent — a second call
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
        // `Local` has exactly one owner. A controller already stepped by hand
        // holds it here; handing a second copy to a thread would leave two
        // region maps disagreeing about which rings exist.
        #[cfg(any(test, feature = "test-support"))]
        assert!(
            self.manual.is_none(),
            "start on a hand-stepped butler: the cycle state is already owned here"
        );

        let queue = Arc::clone(&self.queue);
        let shutdown = Arc::clone(&self.shutdown);
        let shared = self.shared.clone();
        let cycle = self.cycle();
        let pacing = self.pacing;

        let handle = thread::Builder::new()
            .name("tutti-butler".into())
            .spawn(move || {
                // Closes the queue however the loop ends (a panic included),
                // so a sender learns the butler is gone instead of queueing
                // into nothing, and drops what was left queued, here on the
                // butler's own thread (`ButlerQueue::close`).
                struct CloseOnExit(Arc<ButlerQueue>);
                impl Drop for CloseOnExit {
                    fn drop(&mut self) {
                        self.0.close();
                    }
                }
                let queue = CloseOnExit(queue);
                // Before the first cycle: a command pushed before this wakes
                // nobody, and that first cycle drains it.
                queue.0.register_butler();
                let _ = thread_priority::set_current_thread_priority(ThreadPriority::Max);
                butler_loop(&queue.0, shared, cycle, shutdown, pacing);
            })
            // Fatal init: spawning the disk-I/O butler thread is a prerequisite
            // for all streaming/recording. A spawn failure means the OS is out of
            // threads — unrecoverable at this layer, and every downstream stream
            // command would silently stall on a channel with no consumer. Panic
            // loudly at startup rather than degrade into a mute engine.
            .expect("Failed to spawn butler thread");

        self.thread_handle = Some(handle);
    }

    /// Signal shutdown and join the butler thread, blocking until it exits.
    ///
    /// The flag ends the loop at its next cycle, and an unpark starts that
    /// cycle now; a `Shutdown` command is also queued when there is room,
    /// which ends a cycle already draining. The queue is closed (and
    /// drained) afterwards, so a sender still holding it is told the butler
    /// is gone; for a started butler its thread has already done so on its
    /// way out, and this covers a controller that never started. Called by
    /// `Drop`, and idempotent — stopping an already-stopped controller does
    /// nothing more.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.command_sender().try_send(ButlerCommand::Shutdown);
        self.queue.wake();

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        self.queue.close();
    }

    /// Run exactly one butler cycle on the **calling** thread: drain every
    /// queued command, then apply PDC preroll, seeks and refills.
    /// Returns once that work is done, reporting what the pacing layer *would*
    /// have done next.
    ///
    /// This is the same [`ButlerCycle::step`] the butler thread runs — the
    /// thread adds only pacing — so driving a controller by hand exercises the
    /// shipped path rather than a parallel one.
    ///
    /// Intended for tests, which is what the feature gate says. A butler-driven
    /// stream is otherwise observable only through wall clock: send a command,
    /// sleep, look. Stepping makes it observable by *counting*, so a test can
    /// state "after three cycles the ring holds audio" rather than "after 50 ms
    /// it probably does".
    ///
    /// # Panics
    ///
    /// If the butler thread is running. Both drivers own the same butler-local
    /// state, so a controller is one or the other for its whole life; stepping a
    /// live controller would race the thread inside `Local`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn step_once(&mut self) -> StepOutcome {
        assert!(
            self.thread_handle.is_none(),
            "step_once on a started butler: the thread already owns the cycle state"
        );
        let queue = Arc::clone(&self.queue);
        if self.manual.is_none() {
            self.manual = Some(self.cycle());
        }
        let cycle = self.manual.as_mut().expect("set just above");
        cycle.step(&self.shared, || queue.pop())
    }

    /// The shared per-channel plan map, keyed by channel index.
    ///
    /// This is the reader-factory: a plan whose `link` the butler has installed
    /// yields the ring consumer and the `RtState` a disk voice is built from.
    /// Both the butler and the control thread hold it, so an entry may appear or
    /// change between two reads.
    pub fn plans(&self) -> Arc<DashMap<usize, ChannelPlan>> {
        Arc::clone(&self.shared.plans)
    }

    /// The butler's wave cache (tests).
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

impl Drop for ButlerThread {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::butler::loop_body::Pacing;
    use crate::butler::ButlerGone;
    use crate::ports::{Command, Commands};
    use tutti_core::Samples;

    /// The controller is `Sync` (a Bevy resource holds it), with its
    /// hand-stepped cycle held plainly: a compile-time check.
    ///
    /// Mutation (run): a `std::cell::Cell<u8>` field in `ButlerCycle` →
    /// does not compile.
    const _: () = {
        fn sync<T: Sync>() {}
        let _ = sync::<ButlerThread>;
    };

    /// **A table subscribed after the first hand step reaches the cycle**:
    /// the step's cycle is built once and kept, so `with_pdc` must hand the
    /// new feed to it, not only to cycles built later.
    ///
    /// Mutation (run): `with_pdc` not handing the feed to the kept cycle →
    /// the cycle still reads no table (preroll 0), fails.
    #[test]
    fn a_table_subscribed_after_a_hand_step_reaches_the_cycle() {
        let mut butler =
            ButlerThread::with_config(4, SampleRate(48_000.0), BufferConfig::default());
        let _ = butler.step_once();
        let mut butler = butler.with_pdc(Arc::new(PrerollTable::new(vec![Samples(7)])));
        let _ = butler.step_once();
        let cycle = butler.manual.as_ref().expect("stepped by hand");
        assert_eq!(cycle.pdc().preroll(0), 7);
    }

    /// A butler that, left alone, would sleep far longer than any test waits.
    const LONG_PARK: Pacing = Pacing {
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

    /// **A sent command wakes a parked butler**: it is drained well inside
    /// the park's timeout, not when the timeout ends.
    ///
    /// The butler parks for 60 s between cycles and the test allows 10 s:
    /// the bound is generous for a loaded machine, and still six times
    /// shorter than a butler that only ever wakes on its own. The first send
    /// may land before the thread parks; the second is sent once the first
    /// has been drained, by which point the thread is (or is about to be)
    /// parked, and the park token makes the two cases one.
    ///
    /// Mutation (run): `try_send` without its `wake()` → the second command
    /// sits for the park's 60 s → fails at the 10 s bound.
    #[test]
    fn a_send_wakes_a_parked_butler() {
        let mut butler =
            ButlerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default())
                .with_pacing(LONG_PARK);
        let tx = butler.command_sender();
        butler.start();

        for channel_index in [90, 91] {
            tx.try_send(ButlerCommand::StopStreaming { channel_index })
                .expect("room in an empty queue");
            assert!(
                within(Duration::from_secs(10), || tx.queue().len() == 0),
                "command {channel_index} waited on the butler's 60 s park"
            );
            // Let the butler finish its cycle and park again.
            thread::sleep(Duration::from_millis(20));
        }
        butler.stop();
    }

    /// **A stopped butler is gone** to every sender, started or not:
    /// `Commands::send` reports [`ButlerGone`] and the audio thread's
    /// `try_send` refuses.
    ///
    /// A started butler's queue is closed twice over (its thread's exit
    /// guard, then `stop`), so the mutation that bites is on the controller
    /// that never started. Mutation (run): `stop` without its
    /// `queue.close()` → the started half still passes (the guard closed
    /// it), the unstarted half queues into a queue nobody drains, `Ok(())`
    /// → fails.
    #[test]
    fn a_stopped_butler_reports_gone() {
        let mut unstarted =
            ButlerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default());
        unstarted.stop();
        assert_eq!(
            Commands::new(unstarted.command_sender()).send(Command::Stop { channel_index: 0 }),
            Err(ButlerGone),
            "a controller stopped before it started"
        );

        let mut butler =
            ButlerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default());
        butler.start();
        let commands = Commands::new(butler.command_sender());
        assert_eq!(commands.send(Command::Stop { channel_index: 0 }), Ok(()));

        butler.stop();
        assert_eq!(
            commands.send(Command::Stop { channel_index: 0 }),
            Err(ButlerGone)
        );
        assert!(matches!(
            butler
                .command_sender()
                .try_send(ButlerCommand::StopStreaming { channel_index: 0 }),
            Err(crate::butler::TrySendError::Closed)
        ));
    }

    /// **A butler thread that exits on its own closes the queue**, without
    /// its controller's `stop`: a `Shutdown` command ends the loop, and the
    /// thread's exit guard closes the queue on the way out.
    ///
    /// Mutation (run): the thread body without its `CloseOnExit` guard →
    /// the queue never closes → fails at the bound.
    #[test]
    fn a_butler_that_exits_closes_its_queue() {
        let mut butler =
            ButlerThread::with_config(8, SampleRate::new(48_000.0), BufferConfig::default());
        butler.start();
        let tx = butler.command_sender();
        tx.try_send(ButlerCommand::Shutdown).unwrap();
        assert!(
            within(Duration::from_secs(10), || tx.queue().is_closed()),
            "the thread exited but the queue still accepts commands"
        );
        let commands = Commands::new(tx);
        assert_eq!(
            commands.send(Command::Stop { channel_index: 0 }),
            Err(ButlerGone)
        );
    }
}
