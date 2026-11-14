//! The main-thread half of an activation, and the transitions that split a
//! [`ClapLoaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tutti_plugin_types::MainThread;

use tutti_plugin_types::{
    Control, Controller, EditDrain, EditQueue, EditSink, Loaded, Normalized, ParamId, PluginError,
    PluginTail, Prepare, Refused, RestartRules, Samples, Stuck,
};
use tutti_rt::ArcLend;

use super::ClapLoaded;
use super::config::RangeTable;
use super::processor::ClapProcessor;
use super::thread_pool::PoolCell;

/// The main-thread half of an activation: the control surface of a
/// [`ClapLoaded`] (the [`Control`] trait and the methods
/// below), with parameter writes redirected to the Processor. It does not
/// `Deref` to the loaded plugin; a caller that needs the format's own surface
/// (the editor, ports, parameter indication) asks for
/// [`loaded`](Self::loaded) or [`loaded_mut`](Self::loaded_mut).
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it ends the
/// activation: it stops processing, deactivates and destroys the plugin, so a
/// Processor that outlives it renders nothing.
///
/// ```compile_fail
/// // No `Deref`: a `&ClapController` is not a `&ClapLoaded`.
/// fn loaded(c: &tutti_clap_host::ClapController) -> &tutti_clap_host::ClapLoaded {
///     c
/// }
/// ```
pub struct ClapController {
    /// The plugin. `None` only once `deactivate` has taken it.
    loaded: Option<ClapLoaded>,
    /// The writes to the Processor, and those a full ring refused, retried on
    /// the next tick ([`flush_edits`](tutti_plugin_types::Control::flush_edits)).
    queue: EditQueue,
    /// The ranges the Processor denormalizes against, republished after a
    /// rescan that changed the parameters.
    pub(crate) ranges: Arc<ArcLend<RangeTable>>,
    /// The pool the Processor serves `clap.thread-pool` requests on.
    pools: Arc<ArcLend<PoolCell>>,
    /// The plugin's own parameter edits, which the Processor queued from its
    /// output events ([`take_reported_edits`](Control::take_reported_edits)).
    pub(crate) reported: EditDrain,
    /// What the ring refused, counted by the Processor.
    pub(crate) reports_dropped: Arc<AtomicU64>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for ClapController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapController")
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

impl ClapController {
    pub(crate) fn new(
        loaded: ClapLoaded,
        edits: EditSink,
        ranges: Arc<ArcLend<RangeTable>>,
        pools: Arc<ArcLend<PoolCell>>,
        (reported, reports_dropped): (EditDrain, Arc<AtomicU64>),
    ) -> Self {
        Self {
            loaded: Some(loaded),
            queue: EditQueue::new(edits),
            ranges,
            pools,
            reported,
            reports_dropped,
            _not_sync: PhantomData,
        }
    }

    /// The loaded plugin, for the surface the [`Control`] impl
    /// and this type's own methods do not cover. The Controller does not
    /// `Deref` to it: a call that must also tell the
    /// Processor is a method here, not one reached through the loaded type.
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    #[must_use]
    pub fn loaded(&self) -> &ClapLoaded {
        self.loaded
            .as_ref()
            .expect("a Controller holds its plugin until it is consumed")
    }

    /// [`loaded`](Self::loaded), mutably. Prefer a method of the Controller
    /// where one exists (`set_thread_pool`).
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    pub fn loaded_mut(&mut self) -> &mut ClapLoaded {
        self.loaded
            .as_mut()
            .expect("a Controller holds its plugin until it is consumed")
    }

    /// Serves the plugin's `clap.thread-pool` requests on `pool` from the next
    /// block, or refuses them (`None`), and returns the pool it replaces. See
    /// [`ClapLoaded::set_thread_pool`], which the controller no longer
    /// reaches through `Deref`: this one tells the Processor too.
    ///
    /// Control thread. The Processor lets go of a replaced pool at its next block
    /// boundary and the controller frees it on a later tick, so dropping the
    /// pool never happens on the audio thread.
    pub fn set_thread_pool(
        &mut self,
        pool: Option<Arc<tutti_graph::WorkerPool>>,
    ) -> Option<Arc<tutti_clap_ffi::GraphPool>> {
        self.set_task_pool(pool.map(|pool| Arc::new(tutti_clap_ffi::GraphPool::new(pool))))
    }

    /// [`set_thread_pool`](Self::set_thread_pool) with the served pool built by
    /// the caller, for a test that gates a wrapped pool.
    #[doc(hidden)]
    pub fn set_task_pool(
        &mut self,
        pool: Option<Arc<tutti_clap_ffi::GraphPool>>,
    ) -> Option<Arc<tutti_clap_ffi::GraphPool>> {
        let old = self.loaded_mut().set_task_pool(pool);
        let snapshot = self.loaded().pool_snapshot();
        self.pools.publish(PoolCell(snapshot));
        old
    }

    /// Edits the ring refused for want of room, cumulatively. Each was kept and
    /// retried; the count says the Processor was not draining as fast as the
    /// Controller wrote.
    #[must_use]
    pub fn refused_edits(&self) -> u32 {
        self.queue.refused()
    }
}

impl Drop for ClapController {
    fn drop(&mut self) {
        // The FFI plugin stops processing under the audio claim (waiting out a
        // `process` in flight), deactivates, and destroys on drop, so a
        // Processor that outlives this renders nothing. A drop off the main
        // thread has no token to deactivate with: the FFI plugin then leaks
        // the instance instead (its drop).
        if let Some(mut loaded) = self.loaded.take()
            && let Some(main) = MainThread::current()
        {
            loaded.deactivate_plugin(&main);
        }
    }
}

impl Controller for ClapController {
    type Loaded = ClapLoaded;
    type Processor = ClapProcessor;

    fn latency(&self, main: &MainThread) -> Samples {
        Samples(self.loaded().latency_samples(main) as usize)
    }

    fn tail(&self, main: &MainThread) -> PluginTail {
        self.loaded()
            .tail_samples(main)
            .map_or(PluginTail::Unknown, PluginTail::from_clap_samples)
    }
}

impl ClapController {
    /// Queues a parameter write to the Processor: applied at the head of its
    /// next block, or kept for the next tick when the ring is full.
    pub(crate) fn queue_edit(&mut self, id: ParamId, value: Normalized) {
        self.queue.queue(id, value);
    }

    /// Makes every write made so far stale (see [`EditQueue::reset`]).
    pub(crate) fn reset_edits(&mut self) {
        self.queue.reset();
    }

    /// Retries what the ring refused, oldest first, and lets the parameter
    /// ranges' and the pool's retired values go.
    pub(crate) fn retry_edits(&mut self) {
        self.ranges.reclaim();
        self.pools.reclaim();
        self.queue.retry(|_, _| {});
    }
}

impl ClapLoaded {
    /// Rejoins the halves of one activation: checks they are a pair, stops the
    /// Processor, drops it, and hands back the plugin deactivated.
    // The error carries the halves and the whole plugin back, which is the point.
    #[allow(clippy::result_large_err)]
    fn rejoin(
        main: &MainThread,
        mut processor: ClapProcessor,
        mut controller: ClapController,
    ) -> Result<ClapLoaded, (ClapProcessor, ClapController, PluginError)> {
        if !controller.loaded().plugin.owns(processor.audio_side()) {
            return Err((
                processor,
                controller,
                PluginError::Other("the halves are of two activations".into()),
            ));
        }
        // The pair and nothing else: one share each.
        if controller.loaded().plugin.audio_sides() != 1 {
            return Err((
                processor,
                controller,
                PluginError::Other("the activation has a third handle".into()),
            ));
        }
        processor.stop();
        // Edits no block has drained would die with the ring. Older first: what
        // the ring held, then what a full ring had refused.
        let mut pending = Vec::new();
        processor.take_edits(&mut pending);
        pending.append(&mut controller.queue.take_unsent());
        drop(processor);
        let mut loaded = controller
            .loaded
            .take()
            .expect("a Controller holds its plugin until it is consumed");
        loaded.deactivate_plugin(main);
        // An inactive plugin takes a `params.flush` on the main thread, so the
        // writes land now and the next activation starts from them.
        for (id, value) in pending {
            Control::set_parameter(&mut loaded, main, id, value);
        }
        Ok(loaded)
    }
}

impl Loaded for ClapLoaded {
    type Processor = ClapProcessor;
    type Controller = ClapController;

    /// CLAP restarts for latency and for the audio ports.
    const RESTARTS: RestartRules = RestartRules::CLAP;

    fn activate(
        self,
        main: &MainThread,
        prepare: Prepare,
    ) -> Result<(ClapProcessor, ClapController), Refused<Self>> {
        self.activate_now(main, prepare)
    }

    fn deactivate(
        main: &MainThread,
        processor: ClapProcessor,
        controller: ClapController,
    ) -> Result<Self, Stuck<ClapProcessor, ClapController>> {
        Self::rejoin(main, processor, controller).map_err(|(processor, controller, reason)| Stuck {
            processor,
            controller,
            reason,
        })
    }
}
