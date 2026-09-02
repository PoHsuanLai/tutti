//! Butler thread controller: spawn, stop, and expose shared handles.
//!
//! The controller is UI-side — callers hold a `ButlerThread`, send commands
//! via `command_sender`, and read metrics / cache / stream states through
//! accessors. The thread body and command handlers live in sibling modules.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use dashmap::DashMap;
use smol::channel::{bounded, Receiver, Sender};
use thread_priority::ThreadPriority;
use tutti_core::RtPublish;
use tutti_core::SampleRate;
use tutti_core::Samples;

use super::cache::LruCache;
use super::command::ButlerCommand;
use super::config::BufferConfig;
use super::handlers::Handles;
use super::loop_body::butler_loop_async;
use super::metrics::Metrics;
use super::plan::ChannelPlan;
#[cfg(any(test, feature = "test-support"))]
use super::step::{ButlerCycle, StepOutcome};

/// Owner of the butler thread and of every handle shared with it.
///
/// Control-thread side: build with [`with_config`](Self::with_config), spawn
/// with [`start`](Self::start), drive through
/// [`command_sender`](Self::command_sender), and read stream state through
/// [`plans`](Self::plans). `Drop` stops the thread and joins it.
pub struct ButlerThread {
    tx: Sender<ButlerCommand>,
    rx: Option<Receiver<ButlerCommand>>,
    thread_handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Handles,
    config: BufferConfig,
    sample_rate: SampleRate,
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
    /// `channel_capacity` bounds the command queue in messages; a full queue
    /// blocks the sender rather than dropping a command. `sample_rate` is the
    /// *session* rate, against which each file's own rate becomes the stream's
    /// [`SrcRatio`](tutti_core::SrcRatio).
    pub fn with_config(
        channel_capacity: usize,
        sample_rate: SampleRate,
        config: BufferConfig,
    ) -> Self {
        let (tx, rx) = bounded(channel_capacity);

        let shared = Handles {
            plans: Arc::new(DashMap::new()),
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            pdc: None,
        };

        Self {
            tx,
            rx: Some(rx),
            thread_handle: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            shared,
            config,
            sample_rate,
            #[cfg(any(test, feature = "test-support"))]
            manual: None,
        }
    }

    /// Subscribe to a per-channel delay-compensation table, indexed by channel
    /// index and denominated in [`Samples`].
    ///
    /// Published by whoever runs `tutti_core::latency::compensate` over the
    /// audio graph; the butler takes a fresh [`RtPublish::read`] snapshot each
    /// refill cycle and pre-rolls each stream by its channel's entry. The
    /// subscription is shared, not copied, so later publications are observed.
    pub fn with_pdc(mut self, snapshot: Arc<RtPublish<Vec<Samples>>>) -> Self {
        self.shared.pdc = Some(snapshot);
        self
    }

    /// A cloneable sender onto the butler's command queue.
    ///
    /// Valid before `start`: commands sent then simply queue until the thread
    /// drains them. Sending is control-thread work — it blocks when the queue is
    /// full, so it must not be done from the audio thread.
    pub fn command_sender(&self) -> Sender<ButlerCommand> {
        self.tx.clone()
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

        // Fatal-init invariant: `rx` is `Some` for the whole pre-start lifetime
        // and is only taken here. The `thread_handle.is_some()` guard above
        // returns early once the thread is running, so `start()` reaches this
        // point exactly once — the `take()` can never observe `None`.
        let rx = self.rx.take().expect("rx already taken");
        let shutdown = Arc::clone(&self.shutdown);
        let shared = self.shared.clone();
        let config = self.config;
        let sample_rate = self.sample_rate;

        let handle = thread::Builder::new()
            .name("tutti-butler".into())
            .spawn(move || {
                let _ = thread_priority::set_current_thread_priority(ThreadPriority::Max);
                smol::block_on(butler_loop_async(rx, shared, config, sample_rate, shutdown));
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
    /// Both a flag and a `Shutdown` command are sent: the flag ends the loop
    /// even mid-cycle, the command wakes a thread parked waiting for one. Called
    /// by `Drop`, and idempotent — stopping an unstarted or already-stopped
    /// controller does nothing.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.tx.send_blocking(ButlerCommand::Shutdown);

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }

    /// Run exactly one butler cycle on the **calling** thread: drain every
    /// queued command, then apply PDC preroll, seeks, loop wraps and refills.
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
        let rx = self
            .rx
            .as_ref()
            .expect("rx is taken only by `start`, which the assert above excludes")
            .clone();
        let cycle = self
            .manual
            .get_or_insert_with(|| ButlerCycle::new(self.config, self.sample_rate));
        cycle.step(&self.shared, || rx.try_recv().ok())
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
}

impl Drop for ButlerThread {
    fn drop(&mut self) {
        self.stop();
    }
}
