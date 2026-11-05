//! The main-thread half of an activation, and the transitions that split a
//! [`ClapLoaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_plugin_types::{
    Control, Controller, Edit, EditSink, Loaded, Normalized, ParamId, PluginError, PluginTail,
    Prepare, Processor, Refused, RestartRules, SampleFormat, Samples, Stuck,
};
use tutti_types::ArcLend;

use super::config::RangeTable;
use super::processor::{ClapProcessor, ClapProcessorAny};
use super::shell::Shell;
use super::thread_pool::PoolCell;
use super::{ClapLoaded, ClapSample};

/// The main-thread half of an activation: the whole control surface of a
/// [`ClapLoaded`], reached through `Deref`, with parameter writes redirected
/// to the Processor.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it ends the
/// activation: it retires the shell, stops processing, deactivates and destroys
/// the plugin, so a Processor that outlives it renders nothing.
pub struct ClapController {
    /// `None` only once `deactivate` has taken the shell.
    shell: Option<Arc<Shell>>,
    pub(crate) edits: EditSink,
    /// Edits a full ring refused, latest value per parameter, retried on the
    /// next tick ([`flush_edits`](tutti_plugin_types::Control::flush_edits)).
    pub(crate) unsent: Vec<(ParamId, Normalized)>,
    /// A [`Edit::Reset`] the full ring refused: sent before anything in `unsent`,
    /// which is everything written since.
    reset_pending: bool,
    /// The ranges the Processor denormalizes against, republished after a
    /// rescan that changed the parameters.
    pub(crate) ranges: Arc<ArcLend<RangeTable>>,
    /// The pool the Processor serves `clap.thread-pool` requests on.
    pools: Arc<ArcLend<PoolCell>>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for ClapController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapController")
            .field("unsent", &self.unsent)
            .finish_non_exhaustive()
    }
}

// SAFETY: the Controller runs on one thread at a time (every method takes
// `&mut self` or a shared reference of a type that is not `Sync`), and clause 2
// of the invariant on `Shell` keeps its calls to the main-thread set.
unsafe impl Send for ClapController {}

impl ClapController {
    pub(crate) fn new(
        shell: Arc<Shell>,
        edits: EditSink,
        ranges: Arc<ArcLend<RangeTable>>,
        pools: Arc<ArcLend<PoolCell>>,
    ) -> Self {
        Self {
            shell: Some(shell),
            edits,
            unsent: Vec::new(),
            reset_pending: false,
            ranges,
            pools,
            _not_sync: PhantomData,
        }
    }

    fn shell(&self) -> &Arc<Shell> {
        self.shell
            .as_ref()
            .expect("a Controller holds its shell until it is consumed")
    }

    /// Serves the plugin's `clap.thread-pool` requests on `pool` from the next
    /// block, or refuses them (`None`), and returns the pool it replaces. See
    /// [`ClapLoaded::set_thread_pool`], which a controller would otherwise reach
    /// through `Deref`: this one tells the Processor too.
    ///
    /// Control thread. The Processor lets go of a replaced pool at its next block
    /// boundary and the controller frees it on a later tick, so dropping the
    /// pool never happens on the audio thread.
    pub fn set_thread_pool(
        &mut self,
        pool: Option<Arc<dyn tutti_graph::Pool>>,
    ) -> Option<Arc<dyn tutti_graph::Pool>> {
        let old = ClapLoaded::set_thread_pool(self, pool);
        let snapshot = self.pool_snapshot();
        self.pools.publish(PoolCell(snapshot));
        old
    }

    /// Edits the ring refused for want of room, cumulatively. Each was kept and
    /// retried; the count says the Processor was not draining as fast as the
    /// Controller wrote.
    #[must_use]
    pub fn refused_edits(&self) -> u32 {
        self.edits.refused()
    }
}

impl Deref for ClapController {
    type Target = ClapLoaded;

    fn deref(&self) -> &ClapLoaded {
        // SAFETY: main thread, shared; see `Shell::loaded`.
        unsafe { self.shell().loaded() }
            .as_ref()
            .expect("a live Controller's shell holds its plugin")
    }
}

impl DerefMut for ClapController {
    fn deref_mut(&mut self) -> &mut ClapLoaded {
        let shell = self.shell();
        // SAFETY: main thread, exclusive; see `Shell::loaded_mut`.
        unsafe { shell.loaded_mut() }
            .as_mut()
            .expect("a live Controller's shell holds its plugin")
    }
}

impl Drop for ClapController {
    fn drop(&mut self) {
        let Some(shell) = self.shell.take() else {
            return;
        };
        // Before anything is called: a Processor that has not yet taken the
        // `[audio-thread]` role finds this set when it does, and renders nothing.
        shell.retired.store(true, Ordering::Release);
        // SAFETY: the Controller is the only user of the cell.
        let Some(mut loaded) = unsafe { shell.loaded_mut() }.take() else {
            return;
        };
        stop_and_deactivate(&shell, &mut loaded);
        // `loaded` drops here: the editor closes and the plugin is destroyed, on
        // this (the main) thread.
    }
}

/// Ends processing and deactivates, in the order CLAP requires.
fn stop_and_deactivate(shell: &Shell, loaded: &mut ClapLoaded) {
    let host_state = loaded.host_state.clone();
    {
        // The claim waits for a `process` still in flight on another thread, and
        // is the `[audio-thread]` role `stop_processing` is annotated with.
        let _claim = host_state.claim_audio_thread();
        if shell.processing.swap(false, Ordering::Relaxed) {
            // SAFETY: the plugin is alive; this thread holds the claim.
            let plugin = unsafe { loaded.plugin.as_ref() };
            if let Some(stop) = plugin.stop_processing {
                unsafe { stop(loaded.plugin.as_ptr()) };
            }
        }
    }
    // `deactivate` is `[main-thread]`, so the claim is released first.
    loaded.deactivate_plugin();
}

impl Controller for ClapController {
    type Loaded = ClapLoaded;
    type Processor = ClapProcessorAny;

    fn latency(&self) -> Samples {
        Samples(self.latency_samples() as usize)
    }

    fn tail(&self) -> PluginTail {
        self.tail_samples()
            .map_or(PluginTail::Unknown, PluginTail::from_clap_samples)
    }
}

impl ClapController {
    /// Queues a parameter write to the Processor.
    ///
    /// A value already waiting to be retried is replaced by the newer one; a full
    /// ring keeps the value for the next tick.
    pub(crate) fn queue_edit(&mut self, id: ParamId, value: Normalized) {
        if let Some(slot) = self.unsent.iter_mut().find(|(waiting, _)| *waiting == id) {
            slot.1 = value;
            return;
        }
        if self.reset_pending || self.edits.send(Edit::Param { id, value }).is_err() {
            self.unsent.push((id, value));
        }
    }

    /// Makes every write made so far stale: a state or a preset load replaced
    /// the values they were written against, and the next block must not put
    /// them back. The ring's order makes a [`Edit::Reset`] exact; a full ring
    /// holds it for the next tick, and every write after it waits behind it.
    pub(crate) fn reset_edits(&mut self) {
        self.unsent.clear();
        self.reset_pending = self.edits.send(Edit::Reset).is_err();
    }

    /// Retries what the ring refused, oldest first, and lets the parameter
    /// ranges' retired values go.
    pub(crate) fn retry_edits(&mut self) {
        self.ranges.reclaim();
        self.pools.reclaim();
        if self.reset_pending {
            if self.edits.send(Edit::Reset).is_err() {
                return;
            }
            self.reset_pending = false;
        }
        let waiting = std::mem::take(&mut self.unsent);
        for (id, value) in waiting {
            if self.edits.send(Edit::Param { id, value }).is_err() {
                self.unsent.push((id, value));
            }
        }
    }

    /// Reads the parameters again and republishes the ranges the Processor
    /// denormalizes against. Allocates; main thread.
    pub(crate) fn republish_ranges(&mut self) {
        let table = self.read_param_ranges();
        self.ranges.publish(table);
    }
}

impl ClapLoaded {
    /// Rejoins the halves of one activation of any sample type: checks they are
    /// a pair, stops the Processor, drops it, and hands back the plugin
    /// deactivated.
    fn rejoin<P>(
        mut processor: P,
        mut controller: ClapController,
        shell_of: impl Fn(&P) -> &Arc<Shell>,
        stop: impl FnOnce(&mut P),
        take_edits: impl FnOnce(&mut P, &mut Vec<(ParamId, Normalized)>),
    ) -> Result<ClapLoaded, (P, ClapController, PluginError)> {
        if !Arc::ptr_eq(shell_of(&processor), controller.shell()) {
            return Err((
                processor,
                controller,
                PluginError::Other("the halves are of two activations".into()),
            ));
        }
        // The pair and nothing else: one share each.
        if Arc::strong_count(controller.shell()) != 2 {
            return Err((
                processor,
                controller,
                PluginError::Other("the activation has a third handle".into()),
            ));
        }
        stop(&mut processor);
        // Edits no block has drained would die with the ring. Older first: what
        // the ring held, then what a full ring had refused.
        let mut pending = Vec::new();
        take_edits(&mut processor, &mut pending);
        pending.append(&mut std::mem::take(&mut controller.unsent));
        drop(processor);
        let shell = controller
            .shell
            .take()
            .expect("a Controller holds its shell until it is consumed");
        let shell = Arc::try_unwrap(shell)
            .unwrap_or_else(|_| unreachable!("the pair was the only two handles"));
        let mut loaded = shell
            .into_loaded()
            .expect("the plugin stays in the shell until the Controller drops");
        loaded.deactivate_plugin();
        // An inactive plugin takes a `params.flush` on the main thread, so the
        // writes land now and the next activation starts from them.
        for (id, value) in pending {
            Control::set_parameter(&mut loaded, id, value);
        }
        Ok(loaded)
    }

    /// [`Loaded::deactivate`] for a processor of a fixed sample type, as
    /// [`activate_typed`](Self::activate_typed) made it.
    ///
    /// # Errors
    ///
    /// The halves come back, with the reason, when they are of two activations.
    #[allow(clippy::result_large_err)]
    pub fn deactivate_typed<T: ClapSample>(
        processor: ClapProcessor<T>,
        controller: ClapController,
    ) -> Result<ClapLoaded, (ClapProcessor<T>, ClapController, PluginError)> {
        Self::rejoin(
            processor,
            controller,
            |p| p.shell(),
            ClapProcessor::stop,
            ClapProcessor::take_edits,
        )
    }
}

impl Loaded for ClapLoaded {
    type Processor = ClapProcessorAny;
    type Controller = ClapController;

    /// CLAP restarts for latency and for the audio ports.
    const RESTARTS: RestartRules = RestartRules::CLAP;

    fn activate(
        self,
        prepare: Prepare,
    ) -> Result<(ClapProcessorAny, ClapController), Refused<Self>> {
        match prepare.format {
            SampleFormat::Float64 => self
                .activate_typed::<f64>(prepare)
                .map(|(p, c)| (ClapProcessorAny::F64(p), c)),
            _ => self
                .activate_typed::<f32>(prepare)
                .map(|(p, c)| (ClapProcessorAny::F32(p), c)),
        }
    }

    fn deactivate(
        processor: ClapProcessorAny,
        controller: ClapController,
    ) -> Result<Self, Stuck<ClapProcessorAny, ClapController>> {
        Self::rejoin(
            processor,
            controller,
            |p| p.shell(),
            Processor::stop,
            ClapProcessorAny::take_edits,
        )
        .map_err(|(processor, controller, reason)| Stuck {
            processor,
            controller,
            reason,
        })
    }
}
