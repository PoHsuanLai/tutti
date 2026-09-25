//! Forking a hosted plugin **by state transfer**: a fresh instance of the same
//! plugin, handed the live instance's saved state. Doc 013 Phase 3 PR 16
//! (gap 7), which the graph's export (`tutti_graph::Editor::fork`, PR 12)
//! needs before a graph holding a plugin can be rendered offline.
//!
//! # Why not clone and `isolate`, as every other node forks
//!
//! A [`PluginClient`]'s clones share its one bridge to its one plugin process,
//! and no `isolate` can cut that: the audio *is* the other process. A fork
//! built from a clone would drive the live plugin from the render thread —
//! two callers interleaving blocks into one instance's state. So
//! `AudioUnit::forkable()` stays `false` (its promise is about `isolate`, and
//! [`Legacy`] would otherwise fork the node by cloning it), and the node hands
//! the editor its **own** [`ForkSource`] through [`IntoNode`] instead.
//!
//! # What a fork is
//!
//! [`PluginClient::fork_instance`], which the fork source calls:
//!
//! 1. **Ask the live instance for its state** — the one thing the live side
//!    is asked, over the control channel `PluginHandle::save_state` already
//!    uses. It goes down the same command queue as every parameter write
//!    before it, so a value set before the fork is in the state. Nothing
//!    else of the live instance is touched: no reset, no rate change, no
//!    block.
//! 2. **Load a fresh instance** of the same file with the same bridge
//!    settings in a **new `plugin-server` process**, on a socket of its own.
//!    A server hosts exactly one plugin (`LoadPlugin` replaces whatever it
//!    held), so a second instance in the live server is not something the
//!    architecture offers; and a process of its own is what a render worker
//!    wants anyway — the fork's crash cannot take the live plugin down, and
//!    its CPU is not spent on the live server's audio thread. It is launched
//!    at the live node's rate; the editor then prepares it at the fork's
//!    rate, which reaches the new server as a rate change.
//! 3. **Check it is the same plugin** (its id), then **load the state into
//!    it**. Parameters come with it: the state is the format's own
//!    save/load (CLAP `clap.state`, VST3 `getState`/`setState`, AU class
//!    info), which is what a project save restores parameters from.
//! 4. **Rebind its per-block sources** (transport, parameter automation,
//!    harmony, note expression): each installed live source is copied onto
//!    the fork reading the offline timeline ([`ForkMode::Offline`] with an
//!    `OfflineTransport`), or the live transport ([`ForkMode::Live`]). An
//!    offline context of any other type binds nothing: the fork's slots stay
//!    empty rather than read the live playhead.
//! 5. **Offline only:** tell it [`RenderMode::Offline`](crate::RenderMode),
//!    and make its batcher wait for each block (see `Batcher::set_offline_wait`)
//!    — the live pipeline never waits, which on a render worker would turn
//!    every block the subprocess had not finished yet into silence.
//!
//! Any failure is a [`PluginForkError`] naming the step, and the fresh
//! instance (if one started) is dropped with it, which kills its process.
//! Through the graph it is `ForkError::Source { key, cause }`, the cause
//! downcastable to `PluginForkError`. There is no fallback.
//!
//! # What a fork does not have
//!
//! - **MIDI.** A fresh instance has a fresh MIDI port: no live inbox, no
//!   installed clip source, no MIDI-out routing — the `PolySynth::isolate`
//!   rule. A render that needs notes installs its own source on the fork.
//! - **Running state.** Voices, delay lines, a reverb's tail: the state blob
//!   is what a plugin saves for a project, not a snapshot of its DSP. A fork
//!   starts silent, as every fork does.
//! - **A fork of its own.** The node the fork source builds is inserted
//!   without one, like every forked node.
//!
//! In-process VST2 (`InProcessVst2Client`) has no fork source yet and stays
//! not forkable; see its `forkable`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tutti_core::transport::{LoopRange, OfflineTransport, Timeline, TransportState};
use tutti_core::{Beat, Bpm};
use tutti_graph::{ForkCause, ForkMode, ForkSource, IntoNode, Legacy, Node, NodeParts};

use super::{PluginClient, PluginControls};
use crate::error::PluginForkError;
use crate::host::ipc_client::PluginBridge;
use crate::protocol::RenderMode;
use crate::util::config::{unique_socket_path, BridgeConfig};

/// What a [`PluginClient`] was loaded from: enough to load it again.
pub(super) struct Origin {
    pub(super) config: BridgeConfig,
    pub(super) plugin_path: PathBuf,
}

/// Which transport a fork's per-block sources read.
pub(super) enum Rebind {
    /// The live transport each source already reads ([`ForkMode::Live`]).
    Live,
    /// The render's timeline ([`ForkMode::Offline`] with an `OfflineTransport`).
    Offline(OfflineTransport),
    /// None: an offline fork whose context is not an `OfflineTransport`.
    Sever,
}

impl Rebind {
    fn of(mode: ForkMode<'_>) -> Self {
        match mode {
            ForkMode::Live => Self::Live,
            ForkMode::Offline(ctx) => match ctx.downcast_ref::<OfflineTransport>() {
                Some(timeline) => Self::Offline(Arc::clone(timeline)),
                None => Self::Sever,
            },
        }
    }

    /// The transport state a copy of a source reading `live` reads.
    pub(super) fn state(&self, live: &Arc<dyn TransportState>) -> Option<Arc<dyn TransportState>> {
        match self {
            Self::Live => Some(Arc::clone(live)),
            Self::Offline(timeline) => Some(Arc::new(OfflineState(Arc::clone(timeline)))),
            Self::Sever => None,
        }
    }

    /// The timeline a copy of a source reading `live` reads.
    pub(super) fn timeline(&self, live: &Arc<dyn Timeline>) -> Option<Arc<dyn Timeline>> {
        match self {
            Self::Live => Some(Arc::clone(live)),
            Self::Offline(timeline) => Some(Arc::clone(timeline)),
            Self::Sever => None,
        }
    }
}

/// An offline timeline as the [`TransportState`] the plugin sources read.
///
/// The answers are the ones `TransportState` documents for an offline render:
/// not recording, no loop region (an `OfflineTimeline` folds its loop into its
/// own `advance`, so the beat it reports is already wrapped), and no
/// free-running sample counter (`0`, which the plugin ABIs read as exactly
/// that).
struct OfflineState(OfflineTransport);

impl Timeline for OfflineState {
    fn beat(&self) -> Beat {
        self.0.beat()
    }

    fn tempo(&self) -> Bpm {
        self.0.tempo()
    }

    fn is_rolling(&self) -> bool {
        self.0.is_rolling()
    }
}

impl TransportState for OfflineState {
    fn is_recording(&self) -> bool {
        false
    }

    fn loop_range(&self) -> Option<LoopRange> {
        None
    }

    fn steady_time(&self) -> i64 {
        0
    }
}

/// A plugin node's [`ForkSource`], and what [`PluginClient::fork_instance`]
/// runs: everything a fork needs from the live node, and nothing that keeps
/// its process alive (no `ProcessGuard`), so a source held by an editor does
/// not outlive the plugin it forks.
struct PluginFork {
    /// The live bridge — asked for the state, nothing else.
    bridge: Arc<PluginBridge>,
    origin: Arc<Origin>,
    /// The live instance's plugin id, which the fresh one must match.
    id: String,
    /// The live node's controls: its installed sources and its rate.
    controls: PluginControls,
}

impl PluginFork {
    fn of(client: &PluginClient) -> Self {
        Self {
            bridge: Arc::clone(&client.bridge),
            origin: Arc::clone(&client.origin),
            id: client.descriptor.id.clone(),
            controls: client.controls.clone(),
        }
    }

    /// The five steps in the module docs, in order.
    fn instance(&self, mode: ForkMode<'_>) -> Result<PluginClient, PluginForkError> {
        // First, so a plugin that cannot save costs no subprocess.
        let state = self
            .bridge
            .save_state()
            .map_err(PluginForkError::SaveState)?;

        let path = self.origin.plugin_path.clone();
        let config = BridgeConfig {
            // Its own rendezvous: two bridges on one socket path is the
            // collision `unique_socket_path` exists to prevent.
            socket_path: unique_socket_path(),
            ..self.origin.config.clone()
        };
        let timeout = Duration::from_millis(config.timeout_ms);
        let mut fork = PluginClient::new(config, path.clone(), self.controls.sample_rate())
            .map_err(|source| PluginForkError::Load {
                path: path.clone(),
                source,
            })?;
        if fork.descriptor.id != self.id {
            return Err(PluginForkError::Mismatch {
                path,
                expected: self.id.clone(),
                found: fork.descriptor.id.clone(),
            });
        }
        fork.bridge
            .load_state(&state)
            .map_err(PluginForkError::LoadState)?;

        let bind = Rebind::of(mode);
        self.controls.rebind_sources_into(&fork.controls, &bind);
        if let ForkMode::Offline(_) = mode {
            // Advisory: a plugin without the concept keeps rendering as it
            // would live, which is not an error (`Features::RENDER_MODE`).
            let _ = fork.set_render_mode(RenderMode::Offline);
            fork.io.set_offline_wait(timeout);
        }
        Ok(fork)
    }
}

impl ForkSource for PluginFork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Box<dyn Node>, ForkCause> {
        let fork = self.instance(mode).map_err(ForkCause::new)?;
        // The fork runs as the live node does, through `Legacy`; `into_node`
        // so it carries no fork source of its own.
        Ok(Legacy::new(fork).into_node().0)
    }
}

impl PluginClient {
    /// A fresh instance of this plugin carrying this one's saved state: a
    /// fork by state transfer. Control thread; blocks on a subprocess launch
    /// and two state transfers (half a second or more).
    ///
    /// `mode` is the graph's: [`ForkMode::Offline`] with a
    /// `&OfflineTransport` binds the fork's transport-driven sources to that
    /// timeline, tells the plugin it is rendering offline, and makes it wait
    /// for each block; [`ForkMode::Live`] keeps its sources on the live
    /// transport. See the `fork` module docs (`src/host/node/fork.rs`) for
    /// the steps and what a fork does not carry (MIDI, running DSP state).
    ///
    /// This instance is only asked for its state. What the fork renders, and
    /// any parameter changed on either afterwards, does not reach the other.
    ///
    /// Inserting a `PluginClient` into a graph ([`IntoNode`]) hands the
    /// editor a fork source that calls this, so `Editor::fork` forks a graph
    /// holding a plugin.
    pub fn fork_instance(&self, mode: ForkMode<'_>) -> Result<PluginClient, PluginForkError> {
        PluginFork::of(self).instance(mode)
    }
}

/// A hosted plugin as a graph node: run through [`Legacy`] (a plugin is an
/// `AudioUnit`), with a [`ForkSource`] that forks it by state transfer.
impl IntoNode for PluginClient {
    type Controls = ();

    fn into_node(self) -> (Box<dyn Node>, ()) {
        Legacy::new(self).into_node()
    }

    fn into_parts(self) -> NodeParts<()> {
        let fork = PluginFork::of(&self);
        let (node, ()) = Legacy::new(self).into_node();
        NodeParts {
            node,
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}
