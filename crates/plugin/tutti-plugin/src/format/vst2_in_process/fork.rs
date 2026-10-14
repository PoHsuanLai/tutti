//! Forking an in-process VST2 plugin **by state transfer**: a second instance
//! of the same plugin, from the same library, restored from the live
//! instance's saved state. The out-of-process plugin's fork
//! (`host::node::fork`) is the model; what differs is where the second
//! instance lives.
//!
//! # Why not a clone of the node
//!
//! [`InProcessVst2Client`]'s clones share the one `AEffect` behind an
//! `Arc<Mutex<_>>`: a fork by clone would render through the live plugin's
//! state from the render thread, interleaving its blocks with the live ones.
//!
//! # What a fork is
//!
//! [`InProcessVst2Client::fork_instance`], which the fork source calls, on the
//! **main thread** (see "Threads" below):
//!
//! 1. **Ask the live instance for its state**, under its lock: the current
//!    program, when the plugin declares any (`effGetProgram`), its state blob
//!    (`Vst2Instance::get_state`: the plugin's own chunk through `effGetChunk`
//!    when it declares `effFlagsProgramChunks`, else every parameter's value,
//!    one `getParameter` each), and whether the host reports it as rendering
//!    offline. Nothing else is touched. For as long as the lock is held the
//!    live node's audio thread finds it taken (`try_lock`) and renders silent
//!    blocks — the same stall a project save through the handle causes. That
//!    is one block for a chunk or a few parameters, and can be several for a
//!    plugin without chunks that has thousands of parameters, or a slow
//!    `effGetChunk`.
//! 2. **Load a second instance** of the same file at the live node's rate:
//!    `Vst2Instance::load`, as the live one was. The second `dlopen` of the
//!    path maps the image already loaded (one more reference to it), and the
//!    plugin's entry point builds a second `AEffect`. The image is never
//!    unloaded (`tutti-vst2-host`'s `Vst2Handle` leaks the module on purpose),
//!    so the library outlives both instances whatever order they drop in.
//! 3. **Check it is the same plugin** (its id), then **restore the state**:
//!    the program first (`effProgramChange`, bracketed), then the blob
//!    (`effSetChunk`, or `setParameter` for each value). In that order because
//!    a program change may move every parameter; the blob is what the live
//!    instance holds now.
//! 4. **Read the transport from the block's `Env`**, not the live node's
//!    polled reader (which reads the live timeline): for an offline fork the
//!    render's timeline, with the live node's meter if one was installed.
//! 5. **Report the render mode**: an offline fork answers
//!    `audioMasterGetCurrentProcessLevel` with `kVstProcessLevelOffline`; a
//!    live fork ([`ForkMode::Live`]) answers as the live instance did when it
//!    was forked (offline if a host had set the live one offline).
//!
//! The fork's `prepare` tells the plugin its rate itself, under the lock
//! ([`RateDelivery::InPrepare`]): nothing pumps a fork's editor, so a parked
//! rate would never reach it.
//!
//! # Threads
//!
//! Fork on the **main thread** — `Vst2Instance::load`'s rule. A fork runs the
//! plugin's entry point (`VSTPluginMain`, through a loader whose entry-point
//! slot is a process global) and dispatches state opcodes, and a plugin may
//! assume all of that is on its host's main thread. Through the graph's split
//! fork (`Editor::fork_gather`, then `PendingFork::build` on a worker) the
//! source declares itself **pinned** (`ForkBuild::pinned`): the whole of the
//! above, and the fork's `prepare` (its rate, `RateDelivery::InPrepare`),
//! runs in the gather, which bevy-tutti's export calls from its exclusive
//! `start_exports`; the worker's build only places the prepared unit. Drop
//! the fork on the main thread too: dropping it closes its instance
//! (`effMainsChanged(0)`, `effClose`), which debug builds assert is on the
//! main thread once one is marked. A fork may *render* on any thread, as the
//! live node does; bevy-tutti renders on its compute pool and sends the graph
//! (or, when the export fails or is cancelled, whatever of it was built) back
//! to the main thread to drop.
//!
//! Any failure is a [`PluginForkError`] naming the step, and the second
//! instance (if one was built) is dropped with it — `effClose`. Through the
//! graph it is `ForkError::Source { key, cause }`. There is no fallback:
//! never a fork at the plugin's defaults, never one sharing the live
//! instance.
//!
//! # What a fork does not have
//!
//! - **State the plugin does not save.** A plugin without
//!   `effFlagsProgramChunks` saves its parameter values and current program;
//!   anything it holds outside them (a loaded sample, a non-parameter
//!   setting) is not in the fork. A chunk plugin saves what its chunk holds.
//! - **Running state.** Voices, delay lines, a reverb's tail: a fork starts
//!   silent, as every fork does.
//! - **Isolation from the live instance's image.** The two instances share
//!   one loaded library, so a plugin that keeps per-instance state in a
//!   process global would have the two read it alike. VST2 gives a host no way
//!   to ask, and a well-formed plugin keeps its state in its `AEffect`.
//! - **A fork of its own** through the graph: the node the fork source builds
//!   is inserted bare, like every forked node.

use std::path::Path;
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use tutti_core::meter::MeterMap;
use tutti_core::RtPublish;
use tutti_graph::{ForkBuild, ForkCause, ForkMode, ForkSource, Forked};
use tutti_vst2_host::Vst2Instance;

use super::client::{EnvTransport, InProcessVst2Client, RateDelivery};
use crate::error::{PluginForkError, StateError};
use crate::host::node::input_slot::InputSlot;
use crate::host::node::transport_source::{PolledTransport, SteadyTime};

/// An in-process VST2 node's [`ForkSource`]: everything a fork needs from the
/// live node, and nothing that keeps the live plugin alive.
pub(super) struct Vst2Fork {
    /// The live instance — asked for its state, nothing else. Weak: once every
    /// node and handle on it is gone, a fork answers
    /// [`PluginForkError::LiveGone`].
    live: Weak<Mutex<Vst2Instance>>,
    /// The file the live instance was loaded from.
    origin: Arc<Path>,
    /// The live instance's plugin id, which the second one must match.
    id: String,
    /// The live node's rate when this source was taken: what the second
    /// instance is loaded at. The fork's own `prepare` then tells it the
    /// fork's rate.
    sample_rate: f64,
    /// The live node's transport slot (its shared cell): the meter a fork
    /// reads its bars with, if the host installed one.
    transport: InputSlot<PolledTransport>,
}

impl Vst2Fork {
    pub(super) fn of(client: &InProcessVst2Client) -> Self {
        Self {
            live: Arc::downgrade(&client.inner),
            origin: Arc::clone(&client.origin),
            id: client.metadata.id.clone(),
            sample_rate: client.sample_rate,
            transport: client.transport.clone(),
        }
    }

    /// The five steps in the module docs, in order.
    fn instance(&self, mode: ForkMode<'_>) -> Result<InProcessVst2Client, PluginForkError> {
        // First, so a plugin that cannot save costs no second instance. The
        // `Arc` is dropped straight after: the fork holds no strong reference.
        let live = self.live.upgrade().ok_or(PluginForkError::LiveGone)?;
        let (program, state, live_offline) = {
            let instance = live.lock();
            let program = (instance.program_count() > 0).then(|| instance.current_program());
            let state = instance
                .get_state()
                .map_err(|e| PluginForkError::SaveState(StateError::Rejected(e.to_string())))?;
            (program, state, instance.is_offline_render())
        };
        drop(live);

        let (mut fork, handle) =
            super::load_client(&self.origin, tutti_core::SampleRate(self.sample_rate)).map_err(
                |source| PluginForkError::Load {
                    path: self.origin.to_path_buf(),
                    source,
                },
            )?;
        // The fork is driven through its node alone: no editor, no parameter
        // surface. Its instance stays alive in the node.
        drop(handle);
        if fork.metadata.id != self.id {
            return Err(PluginForkError::Mismatch {
                path: self.origin.to_path_buf(),
                expected: self.id.clone(),
                found: fork.metadata.id.clone(),
            });
        }
        {
            let mut instance = fork.inner.lock();
            if let Some(program) = program {
                if !instance.set_program(program) {
                    return Err(PluginForkError::LoadState(StateError::Rejected(format!(
                        "the live instance's program {program} is out of the fresh \
                         instance's range"
                    ))));
                }
            }
            instance
                .set_state(&state)
                .map_err(|e| PluginForkError::LoadState(StateError::Rejected(e.to_string())))?;
            let offline = match mode {
                ForkMode::Offline(_) => true,
                ForkMode::Live => live_offline,
            };
            instance.set_offline_render(offline);
        }

        let meter = self
            .transport
            .source_ref()
            .load()
            .as_ref()
            .map(|source| Arc::clone(source.meter()))
            .unwrap_or_else(|| Arc::new(RtPublish::new(MeterMap::default())));
        fork.env_transport = Some(EnvTransport {
            meter,
            steady: SteadyTime::default(),
        });
        fork.rate_delivery = RateDelivery::InPrepare;
        Ok(fork)
    }
}

impl ForkSource for Vst2Fork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let fork = self.instance(mode).map_err(ForkCause::new)?;
        // In this process: nothing can fail while it renders that the render
        // would not see, so no health probe. Boxed bare: no fork of its own.
        Ok(Forked::new(Box::new(fork)))
    }

    /// **Pinned**, said here rather than left to the default: the second
    /// instance is loaded and handed the state at the gather, and its
    /// `prepare` (which tells the plugin the fork's rate,
    /// `RateDelivery::InPrepare`) runs there too, on the main thread the
    /// gather is called on. Only rendering it leaves that thread.
    fn gather(&self, mode: ForkMode<'_>) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::pinned)
    }
}

impl InProcessVst2Client {
    /// Creates a second instance of this plugin carrying this one's saved
    /// state, ready to insert or drive.
    ///
    /// This is a fork by state transfer, and what inserting the node into a
    /// graph ([`IntoNode`](tutti_graph::IntoNode)) hands the editor, so
    /// `Editor::fork` (an offline export) forks a graph holding it.
    ///
    /// **Main thread only**, as `Vst2Instance::load` is: it runs the plugin's
    /// entry point a second time and dispatches its state. Drop the fork on
    /// the main thread too (dropping closes its instance); it may render on
    /// any thread.
    ///
    /// The state is the current program (when the plugin has programs) and
    /// what the plugin saves: its own chunk for a plugin that declares
    /// `effFlagsProgramChunks`, else every parameter's value. The fork reads
    /// its transport from the `Env` of the graph it renders in, not the
    /// installed reader. [`ForkMode::Offline`] tells the plugin it renders
    /// offline; [`ForkMode::Live`] keeps the live instance's render mode. A fork carries no running DSP state (voices, delay lines), so
    /// it starts silent. Nothing set on either instance afterwards reaches the
    /// other.
    ///
    /// # Errors
    ///
    /// A [`PluginForkError`] naming the step that failed: the live instance is
    /// gone or would not save its state, the second instance did not load (a
    /// plugin that cannot be instantiated twice), it is a different plugin,
    /// or it refused the program or the state. A second instance that was
    /// built is closed before this returns.
    pub fn fork_instance(
        &self,
        mode: ForkMode<'_>,
    ) -> Result<InProcessVst2Client, PluginForkError> {
        Vst2Fork::of(self).instance(mode)
    }
}
