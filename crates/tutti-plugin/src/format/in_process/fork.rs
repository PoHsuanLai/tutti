//! Forking an in-process plugin **by state transfer**: a second instance
//! of the same plugin, from the same library, restored from the live
//! instance's saved state. The out-of-process plugin's fork
//! (`host::node::fork`) is the model; what differs is where the second
//! instance lives.
//!
//! # Why not a clone of the node
//!
//! A clone of [`InProcClient`](super::InProcClient) would share the one plugin instance behind an
//! `Arc<Mutex<_>>`: a fork by clone would render through the live plugin's
//! state from the render thread, interleaving its blocks with the live ones.
//! So the node is not `Clone` at all.
//!
//! # What a fork is
//!
//! [`PluginNode::fork_instance`], which the fork source calls, on the
//! **main thread** (see "Threads" below):
//!
//! 1. **Read what the live node has delivered** per parameter (its parameter
//!    delivery's record, `host::node`'s `param_delivery`), then **ask the
//!    live instance for its state**, under its lock: the current preset,
//!    when the plugin lists any (`Control::current_preset`), its state blob
//!    (`Control::save_state`; VST2's is the plugin's own chunk when it
//!    declares `effFlagsProgramChunks`, else every parameter's value), and
//!    the render mode the instance runs in. Nothing else is touched. The lock is the control side's: the
//!    live node's audio thread renders through its lent Processor meanwhile,
//!    as it does through a project save.
//! 2. **Load a second instance** of the same file at the live node's rate,
//!    from the live instance's [`Origin`] as `Reopen::reopen` does
//!    (`InProcess::load_in_process`; an active `Lifecycle` holds a Controller,
//!    not the loaded plugin `reopen` borrows). For VST2 the second `dlopen` of
//!    the path maps the image already loaded (one more reference to it), and
//!    the plugin's entry point builds a second `AEffect`. The image is never
//!    unloaded (`tutti-vst2-ffi`'s `PluginInstance` leaks the module on purpose),
//!    so the library outlives both instances whatever order they drop in.
//! 3. **Check it is the same plugin** (its id), then **restore the state**:
//!    the preset first, then the blob (`Control::load_state`). In that order
//!    because a preset change may move every parameter; the blob is what the
//!    live instance holds now.
//! 4. **Read the transport from the block's `Cx`**, as the live node does:
//!    for an offline fork the render's timeline, with the meter the live
//!    node had installed when it was forked.
//! 5. **Seed the fork's parameter delivery** with that record, as the
//!    out-of-process fork does (`host::node::fork`): a value the live node
//!    delivered is the fork's too, so a host value held still is not sent to
//!    the fork over the state it was loaded with (an edit made in the live
//!    plugin's editor), and an export plays what live playback plays. Read
//!    before the state, so every value the record shows is in it (the live
//!    node records a value, `Release`, only once its `setParameter` has
//!    returned, and the state is saved later, under the instance lock); a
//!    value delivered between the two is in the state and sent to the fork
//!    once more, harmlessly.
//! 6. **Report the render mode**: an offline fork is told it renders
//!    offline; a live fork ([`ForkMode::Live`]) is told what the live
//!    instance ran in when it was forked (offline if a host had set the live
//!    one offline).
//!
//! The fork's `prepare` tells the plugin its rate itself, under the lock
//! ([`RateDelivery::InPrepare`]): nothing pumps a fork's editor, so a parked
//! rate would never reach it.
//!
//! # Threads
//!
//! Fork on the **main thread** — `InProcess::load_in_process`'s rule. A fork
//! runs the plugin's entry point and dispatches its state, and a plugin may
//! assume all of that is on its host's main thread. Through the graph's split
//! fork (`Editor::fork_gather`, then `PendingFork::build` on a worker) the
//! source declares itself **pinned** (`ForkBuild::pinned`): the whole of the
//! above, and the fork's `prepare` (its rate, `RateDelivery::InPrepare`),
//! runs in the gather, which bevy-tutti's export calls from its exclusive
//! `start_exports`; the worker's build only places the prepared unit. Drop
//! the fork on the main thread too: dropping it closes its instance, which
//! debug builds assert is on the main thread once one is marked. A fork may *render* on any thread, as the
//! live node does; bevy-tutti renders on its compute pool and sends the graph
//! (or, when the export fails or is cancelled, whatever of it was built) back
//! to the main thread to drop.
//!
//! Any failure is a [`PluginForkError`] naming the step, and the second
//! instance (if one was built) is dropped with it. Through the
//! graph it is `ForkError::Source { key, cause }`. There is no fallback:
//! never a fork at the plugin's defaults, never one sharing the live
//! instance.
//!
//! # What a fork does not have
//!
//! - **State the plugin does not save.** A plugin saves what its
//!   [`Control::save_state`] holds (VST2 without `effFlagsProgramChunks`: its
//!   parameter values and current program); anything it holds outside that
//!   (a loaded sample, a non-parameter setting) is not in the fork.
//! - **Running state.** Voices, delay lines, a reverb's tail: a fork starts
//!   silent, as every fork does.
//! - **Isolation from the live instance's image.** The two instances share
//!   one loaded library, so a plugin that keeps per-instance state in a
//!   process global would have the two read it alike. No format gives a host
//!   a way to ask, and a well-formed plugin keeps its state in its instance.
//! - **A fork of its own** through the graph: the node the fork source builds
//!   is inserted bare, like every forked node.

use std::sync::{Arc, Weak};
use tutti_plugin_types::MainThread;

use parking_lot::Mutex;
use tutti_graph::{ForkBuild, ForkCause, ForkMode, ForkSource, Forked};
use tutti_plugin_types::{Control, Features, InProcess, Origin, RenderMode};

use super::client::{PluginNode, RateDelivery, TransportFeed};
use super::instance::Instance;
use super::loader::load_client_restoring;
use crate::host::node::DeliveryCounters;
use tutti_plugin_types::PluginForkError;
use tutti_plugin_types::StateError;

/// An in-process node's [`ForkSource`]: everything a fork needs from the
/// live node, and nothing that keeps the live plugin alive.
pub(super) struct Fork<F: InProcess> {
    /// The live instance — asked for its state, nothing else. Weak: once every
    /// node and handle on it is gone, a fork answers
    /// [`PluginForkError::LiveGone`].
    live: Weak<Mutex<Instance<F>>>,
    /// Where the live instance came from: the file and the choice of plugin
    /// in it, which the second instance loads again.
    origin: Origin,
    /// The live instance's plugin id, which the second one must match.
    id: String,
    /// The live node's rate when this source was taken: what the second
    /// instance is loaded at. The fork's own `prepare` then tells it the
    /// fork's rate.
    sample_rate: f64,
    /// What the live handle reports the plugin can do: whether it has presets
    /// to carry across.
    features: Features,
    /// The live node's transport feed (its shared meter cell): the meter a
    /// fork reads its bars with, as installed when it forks.
    transport: TransportFeed,
    /// The live node's tempo reader, cloned for each fork (control side).
    tempo: tutti_transport::TempoReader,
    /// The live node's parameter delivery: what it has delivered, read when
    /// it forks, before its state.
    delivery: Arc<DeliveryCounters>,
}

impl<F: InProcess + 'static> Fork<F>
where
    F::Processor: 'static,
{
    pub(super) fn of(bound: &PluginNode<F>) -> Self {
        let client = &bound.client;
        Self {
            tempo: bound.tempo.clone(),
            live: Arc::downgrade(&client.inner),
            origin: client.origin.clone(),
            id: client.descriptor.id.clone(),
            sample_rate: client.sample_rate,
            features: client.features,
            transport: client.transport.clone(),
            delivery: client.params.counters(),
        }
    }

    /// The six steps in the module docs, in order.
    fn instance(
        &self,
        main: &MainThread,
        mode: ForkMode,
    ) -> Result<PluginNode<F>, PluginForkError> {
        // First, so a plugin that cannot save costs no second instance. The
        // `Arc` is dropped straight after: the fork holds no strong reference.
        let live = self.live.upgrade().ok_or(PluginForkError::LiveGone)?;
        // Before the state: see step 1.
        let delivered = self.delivery.delivered();
        let (preset, state, live_mode) = {
            let mut instance = live.lock();
            let live_mode = instance.render_mode();
            let control = instance.control().ok_or_else(|| {
                PluginForkError::SaveState(StateError::Rejected("the plugin is lost".into()))
            })?;
            // A plugin with no presets has none to carry (and would refuse
            // the current one's id).
            let preset = if self.features.contains(Features::PRESET_LOAD) {
                control.current_preset(main)
            } else {
                None
            };
            let state = control
                .save_state(main)
                .map_err(|e| PluginForkError::SaveState(StateError::Rejected(e.to_string())))?;
            (preset, state, live_mode)
        };
        drop(live);

        // The preset, then the state, on the fresh instance **before it is
        // activated**: a plugin may take its mode from the state at activation.
        let mut restore = |loaded: &mut F| -> Result<(), String> {
            if let Some(preset) = &preset
                && !Control::load_preset(loaded, main, preset)
            {
                return Err(format!(
                    "the live instance's preset {preset:?} is not in the fresh instance"
                ));
            }
            Control::load_state(loaded, main, &state).map_err(|e| e.to_string())
        };
        // From the stored `Origin`, not `Reopen::reopen`: an active `Lifecycle`
        // holds a Controller, not the `&F` that `reopen` needs.
        let (mut fork, handle, restored) = load_client_restoring::<F>(
            main,
            self.origin.path(),
            self.origin.load(),
            tutti_types::SampleRate(self.sample_rate),
            Some(&mut restore),
        )
        .map_err(|source| PluginForkError::Load {
            path: self.origin.path().to_path_buf(),
            source,
        })?;
        // The fork is driven through its node alone: no editor, no parameter
        // surface. Its instance stays alive in the node.
        drop(handle);
        if fork.descriptor.id != self.id {
            return Err(PluginForkError::Mismatch {
                path: self.origin.path().to_path_buf(),
                expected: self.id.clone(),
                found: fork.descriptor.id.clone(),
            });
        }
        if let Some(Err(reason)) = restored {
            return Err(PluginForkError::LoadState(StateError::Rejected(reason)));
        }
        let offline = match mode {
            ForkMode::Offline => true,
            ForkMode::Live => live_mode.is_offline(),
        };
        fork.inner.lock().set_render_mode(
            main,
            if offline {
                RenderMode::Offline
            } else {
                RenderMode::Realtime
            },
        );

        fork.params.seed(&delivered);
        fork.transport = self.transport.forked(offline);
        fork.rate_delivery = RateDelivery::InPrepare;
        Ok(fork.bind(self.tempo.clone()))
    }
}

impl<F: InProcess + 'static> ForkSource for Fork<F>
where
    F::Processor: 'static,
{
    fn fork(&self, mode: ForkMode) -> Result<Forked, ForkCause> {
        // The gather runs on the main thread (below), which `current` confirms;
        // off it there is no way to load a second instance, so the fork is
        // refused rather than loaded where the format forbids it.
        let main =
            MainThread::current().ok_or_else(|| ForkCause::new(PluginForkError::OffMainThread))?;
        let fork = self.instance(&main, mode).map_err(ForkCause::new)?;
        // In this process: nothing can fail while it renders that the render
        // would not see, so no health probe. Boxed bare: no fork of its own.
        Ok(Forked::new(Box::new(fork)))
    }

    /// **Pinned**, said here rather than left to the default: the second
    /// instance is loaded and handed the state at the gather, and its
    /// `prepare` (which tells the plugin the fork's rate,
    /// `RateDelivery::InPrepare`) runs there too, on the main thread the
    /// gather is called on. Only rendering it leaves that thread.
    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::pinned)
    }
}

impl<F: InProcess + 'static> PluginNode<F>
where
    F::Processor: 'static,
{
    /// Creates a second instance of this plugin carrying this one's saved
    /// state, ready to insert or drive.
    ///
    /// This is a fork by state transfer, and what inserting the node into a
    /// graph ([`IntoNode`](tutti_graph::IntoNode)) hands the editor, so
    /// `Editor::fork` (an offline export) forks a graph holding it.
    ///
    /// **Main thread only**, as `InProcess::load_in_process` is: it runs the
    /// plugin's entry point a second time and dispatches its state. Drop the fork on
    /// the main thread too (dropping closes its instance); it may render on
    /// any thread.
    ///
    /// The state is the current preset (when the plugin has presets) and
    /// what the plugin saves ([`Control::save_state`]). The fork reads
    /// its transport from the graph it renders in, as the live node does,
    /// with the meter installed on this node now.
    /// [`ForkMode::Offline`] tells the plugin it renders offline;
    /// [`ForkMode::Live`] keeps the live instance's render mode. A fork
    /// carries no running DSP state (voices, delay lines), so it starts
    /// silent. Its parameter delivery starts from what this node has
    /// delivered, so a host value held still is not sent to it over the
    /// saved state. Nothing set on either instance afterwards reaches the
    /// other.
    ///
    /// # Errors
    ///
    /// A [`PluginForkError`] naming the step that failed: the live instance is
    /// gone or would not save its state, the second instance did not load (a
    /// plugin that cannot be instantiated twice), it is a different plugin,
    /// or it refused the preset or the state. A second instance that was
    /// built is closed before this returns.
    pub fn fork_instance(
        &self,
        main: &MainThread,
        mode: ForkMode,
    ) -> Result<PluginNode<F>, PluginForkError> {
        Fork::of(self).instance(main, mode)
    }
}
