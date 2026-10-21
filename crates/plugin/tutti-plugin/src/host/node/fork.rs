//! Forking a hosted plugin **by state transfer**: a fresh instance of the same
//! plugin, handed the live instance's saved state. The graph's export
//! (`tutti_graph::Editor::fork`) needs this to render a graph holding a
//! plugin offline.
//!
//! # Why not a copy of the node
//!
//! A [`PluginClient`]'s audio *is* another process, reached through one
//! bridge: a copy of the node would drive the live plugin from the render
//! thread — two callers interleaving blocks into one instance's state. So the
//! node is not `Clone`, and a bound client hands the editor its **own**
//! [`ForkSource`] through [`IntoNode`] instead.
//!
//! # What a fork is
//!
//! [`PluginClient::fork_instance`], which the fork source calls:
//!
//! 1. **Ask the live instance for its state** — the one thing the live side
//!    is asked, over the control channel `PluginHandle::save_state` already
//!    uses. Nothing else of the live instance is touched: no reset, no rate
//!    change, no block. It is not free, though: the save runs on the live
//!    bridge thread, which queues the live instance's blocks behind it for
//!    as long as the plugin takes to serialise — the same stall a project
//!    save causes. (Blocks it misses meanwhile read as silence, as for any
//!    late reply.)
//!
//!    **Whether a parameter set just before the fork is in the state depends
//!    on the write reaching the plugin, not on the fork.** The save goes down
//!    the same command queue as `set_parameter`, after it, so the ordering is
//!    right; and for CLAP (the reference probe; verified by `clap_fork.rs`)
//!    a delivered write is in the next save. Not in general:
//!    - `set_parameter` is fire-and-forget, and a write refused by a full
//!      command queue is dropped — neither the live plugin nor the state
//!      ever has it;
//!    - CLAP skips a direct write to a `REQUIRES_PROCESS` parameter while the
//!      plugin is processing (it can only arrive through a process call), so
//!      until the next block delivers it, the state is the old value;
//!    - VST3's host write goes to the edit controller, and a component whose
//!      `getState` does not reflect controller-side values saves without it
//!      until the processor has seen the change.
//!
//!    A host that needs a value in the fork sets it through the path the
//!    plugin processes (automation), or renders a block before forking.
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
//! 4. **Give it the live node's meter.** Everything else it reads comes
//!    from its own graph: the transport from its `Env` (for an offline
//!    fork, the render's), and parameter automation, chords and
//!    scales and MIDI from the graph's event edges, whose nodes fork
//!    themselves: an exported instrument plays the clip node that feeds it.
//!    Just before the state, step 1 also reads **what the live node has
//!    delivered** per parameter (`param_delivery`'s record), and step 5
//!    seeds the fork's delivery with it. The node sends changes only, so a
//!    host value the live plugin was sent once and then edited away from in
//!    its own editor is never re-sent live; seeded, the fork does not send
//!    it either, and the export renders what playback plays.
//! 5. **Bind it**, and **offline only:** tell it
//!    [`RenderMode::Offline`](crate::RenderMode), and make its batcher wait for
//!    each chunk (see `Batcher::set_offline_wait`) — the live pipeline never
//!    waits, which on a render worker would turn every chunk the subprocess
//!    had not finished yet into silence.
//!
//! Any failure is a [`PluginForkError`] naming the step, and the fresh
//! instance (if one started) is dropped with it, which kills and reaps its
//! process. Through the graph it is `ForkError::Source { key, cause }`, the
//! cause downcastable to `PluginForkError`. There is no fallback.
//!
//! # Threads
//!
//! Through the graph's split fork (`Editor::fork_gather`, then
//! `PendingFork::build`) the steps divide where the slow part starts. The
//! **gather**, on the control thread at the moment of the fork, is step 1
//! and the reads of the live node's rate and meter: everything the fork
//! takes of the live node, so a parameter set or a meter installed after
//! it does not reach the fork. Steps 2 to 5 (the `plugin-server` launch and
//! the state load, half a second or more) are a **deferred build**
//! (`ForkBuild::deferred`) that reads nothing live and may run on any
//! thread: bevy-tutti's export runs it on its compute pool, so the frame
//! the export starts in does not wait for it. [`PluginClient::fork_instance`]
//! runs all five on the calling thread.
//!
//! # When the fork fails while rendering
//!
//! The fork's server can die, or hang, mid-render. `process` cannot say so,
//! and silence is a valid output, so every fork carries a [`ForkWatch`]
//! ([`PluginClient::fork_health`], and through the graph the
//! `tutti_graph::ForkHealth` probe `Editor::fork_health` reads):
//! `ForkFaultKind::Crashed` when the fork's bridge has latched a crash,
//! `ForkFaultKind::TimedOut` when a block missed its budget
//! (`BridgeConfig::timeout_ms`), the cause a [`PluginRenderFault`]. After
//! the first miss an offline fork stops waiting, so a hung server costs one
//! budget, not one per block. A renderer checks the probe after rendering
//! and reports the render as failed (tutti-export: `Error::ForkFailed`).
//!
//! # What a fork does not have
//!
//! - **Live MIDI.** What a keyboard sends reaches the live graph's queue
//!   node, whose fork is silent.
//! - **Running state.** Voices, delay lines, a reverb's tail: the state blob
//!   is what a plugin saves for a project, not a snapshot of its DSP. A fork
//!   starts silent, as every fork does.
//! - **A fork of its own.** The node the fork source builds is inserted
//!   without one, like every forked node.
//!
//! In-process VST2 (`InProcessVst2Client`) forks by state transfer too, into a
//! second instance in this process from the same library
//! (`format::vst2_in_process`'s `fork` module).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tutti_core::Samples;
use tutti_graph::{
    ForkBuild, ForkCause, ForkFaultKind, ForkHealth, ForkMode, ForkSource, Forked, IntoNode, Node,
    NodeParts,
};
use tutti_types::Latency;

use super::controls::ForkedSources;
use super::{Bound, PluginClient, PluginControls, ProcessGuard};
use crate::error::{PluginForkError, PluginRenderFault};
use crate::host::ipc_client::PluginBridge;
use crate::protocol::RenderMode;
use crate::util::config::{unique_socket_path, BridgeConfig};

/// What a [`PluginClient`] was loaded from: enough to load it again.
pub(super) struct Origin {
    pub(super) config: BridgeConfig,
    pub(super) plugin_path: PathBuf,
}

/// Whether a forked instance has failed while rendering: its
/// `tutti_graph::ForkHealth` probe. See "When the fork fails while
/// rendering" in the module docs.
pub(super) struct ForkWatch {
    /// The fork's controls: the latency it declares now.
    controls: PluginControls,
    /// The latency the fork's graph was compiled against, recorded by the
    /// node's `prepare` once the plugin has settled (`usize::MAX`: not
    /// prepared yet).
    planned: AtomicUsize,
    /// The fork's bridge and process. Weak: a probe the editor keeps must not
    /// keep the fork alive after its node is gone (a dropped fork has nothing
    /// left to fail), and the fork's own batcher holds this watch.
    bridge: Weak<PluginBridge>,
    server: Weak<ProcessGuard>,
    /// Latched by the batcher when a block first misses `budget`.
    gave_up: AtomicBool,
    /// Latched, with its exit status, when the server was found dead.
    died: Mutex<Option<String>>,
    budget: Duration,
}

impl ForkWatch {
    /// The per-block budget of an offline wait (`BridgeConfig::timeout_ms`).
    pub(super) fn budget(&self) -> Duration {
        self.budget
    }

    /// Whether an offline wait should not wait at all any more: a block
    /// already missed its budget, or the server is dead.
    pub(super) fn stopped_waiting(&self) -> bool {
        self.gave_up.load(Ordering::Acquire) || lock(&self.died).is_some()
    }

    /// Latch a missed budget.
    pub(super) fn give_up(&self) {
        self.gave_up.store(true, Ordering::Release);
    }

    /// Latch a crash the bridge reported, unless a death is latched already.
    pub(super) fn latch_crash(&self, cause: Option<String>) {
        let mut died = lock(&self.died);
        if died.is_none() {
            *died = Some(cause.unwrap_or_else(|| "no cause latched".to_string()));
        }
    }

    /// Record the latency the fork's graph is compiled against: what the
    /// node's `Shape` declares right after this `prepare`.
    pub(super) fn plan(&self, latency: Latency) {
        self.planned
            .store(latency.samples().get(), Ordering::Release);
    }

    /// The fork's latency, if it moved since [`plan`](Self::plan): `(planned,
    /// now)`.
    fn latency_moved(&self) -> Option<(Latency, Latency)> {
        let planned = self.planned.load(Ordering::Acquire);
        if planned == usize::MAX {
            return None;
        }
        let planned = Latency::new(Samples(planned));
        let now = self.controls.declared_latency();
        (now != planned).then_some((planned, now))
    }

    /// Ask the process whether it has exited, and latch it if so.
    pub(super) fn server_died(&self) -> bool {
        let mut died = lock(&self.died);
        if died.is_some() {
            return true;
        }
        let status = self.server.upgrade().and_then(|s| s.exited());
        if let Some(status) = status {
            *died = Some(format!("plugin-server exited: {status}"));
        }
        died.is_some()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ForkHealth for ForkWatch {
    /// A timeout first if one was latched (it happened before anything the
    /// bridge noticed later), then a dead process or a crash the bridge
    /// latched, then a latency that moved after the fork's graph was compiled
    /// (`Failed`: the render ran, misaligned).
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)> {
        if self.gave_up.load(Ordering::Acquire) {
            let cause = PluginRenderFault::TimedOut {
                budget: self.budget,
            };
            return Some((ForkFaultKind::TimedOut, ForkCause::new(cause)));
        }
        let crashed = |cause: String| {
            let cause = PluginRenderFault::Crashed { cause };
            (ForkFaultKind::Crashed, ForkCause::new(cause))
        };
        if self.server_died() {
            return lock(&self.died).clone().map(crashed);
        }
        if let Some(bridge) = self.bridge.upgrade() {
            if bridge.is_crashed() {
                return Some(crashed(
                    bridge
                        .crash_cause()
                        .unwrap_or_else(|| "no cause latched".to_string()),
                ));
            }
        }
        self.latency_moved().map(|(planned, now)| {
            let cause = PluginRenderFault::LatencyChanged { planned, now };
            (ForkFaultKind::Failed, ForkCause::new(cause))
        })
    }
}

/// A plugin node's [`ForkSource`], and what [`PluginClient::fork_instance`]
/// runs: everything a fork needs from the live node, and nothing that keeps
/// it alive, so a source held by an editor does not outlive the plugin it
/// forks.
struct PluginFork {
    /// The live bridge — asked for the state, nothing else. Weak: once every
    /// node and handle on the live instance is gone, a fork answers
    /// [`PluginForkError::LiveGone`] at once instead of queueing a save on a
    /// bridge with no process behind it.
    bridge: Weak<PluginBridge>,
    origin: Arc<Origin>,
    /// The live instance's plugin id, which the fresh one must match.
    id: String,
    /// The live node's controls: its installed sources and its rate.
    controls: PluginControls,
}

impl PluginFork {
    fn of<S>(client: &PluginClient<S>) -> Self {
        Self {
            bridge: Arc::downgrade(&client.bridge),
            origin: Arc::clone(&client.origin),
            id: client.descriptor.id.clone(),
            controls: client.controls.clone(),
        }
    }

    /// The five steps in the module docs, in order.
    fn instance(&self, mode: ForkMode) -> Result<PluginClient<Bound>, PluginForkError> {
        self.capture(mode)?.build()
    }

    /// Step 1, and everything else the fork reads of the live node: what the
    /// gather takes on the control thread ("Threads" in the module docs).
    fn capture(&self, mode: ForkMode) -> Result<Capture, PluginForkError> {
        // First, so a plugin that cannot save costs no subprocess. The `Arc`
        // is dropped straight after: the fork holds no strong reference.
        let live = self.bridge.upgrade().ok_or(PluginForkError::LiveGone)?;
        // What the live node has delivered, read *before* the state. Each
        // value the record shows was stored (`Release`) after its chunk's
        // `Process` command was pushed, and is read here with `Acquire`, so
        // that push happens before the `SaveState` pushed below and the
        // bridge's FIFO delivers it first: every value read is in the state.
        // A value delivered in between is in the state too and is re-sent to
        // the fork once, harmlessly. Read after, a value delivered in between
        // would be recorded as the fork's though its state predates it.
        let delivered = self.controls.delivered();
        let state = live.save_state().map_err(PluginForkError::SaveState)?;
        drop(live);
        Ok(Capture {
            state,
            delivered,
            origin: Arc::clone(&self.origin),
            id: self.id.clone(),
            sample_rate: self.controls.sample_rate(),
            sources: self.controls.sources_now(),
            offline: matches!(mode, ForkMode::Offline),
        })
    }
}

/// What a plugin fork's gather read of the live node: its saved state, its
/// rate and sources at that moment, and what to load. Steps 2 to 5 build the
/// fork from this alone, on any thread, so nothing the host does to the live
/// node after the gather reaches the fork.
struct Capture {
    state: Vec<u8>,
    /// What the live node had delivered per parameter when the state was
    /// saved: the fork's delivery is seeded with it, so a host value the
    /// live plugin holds unchanged (under an edit made in its editor, say)
    /// is not sent to the fork either, and the export renders what live
    /// playback plays.
    delivered: Vec<(crate::protocol::ParamAddress, f64)>,
    origin: Arc<Origin>,
    /// The live instance's plugin id, which the fresh one must match.
    id: String,
    /// The live node's rate, which the fresh instance is launched at.
    sample_rate: tutti_core::SampleRate,
    sources: ForkedSources,
    offline: bool,
}

impl Capture {
    /// Steps 2 to 5 of the module docs: the slow part (a `plugin-server`
    /// launch and a state load), free to run off the control thread.
    fn build(self) -> Result<PluginClient<Bound>, PluginForkError> {
        let path = self.origin.plugin_path.clone();
        let config = BridgeConfig {
            // Its own rendezvous: two bridges on one socket path is the
            // collision `unique_socket_path` exists to prevent.
            socket_path: unique_socket_path(),
            ..self.origin.config.clone()
        };
        let timeout = Duration::from_millis(config.timeout_ms);
        let fork = PluginClient::new(config, path.clone(), self.sample_rate).map_err(|source| {
            PluginForkError::Load {
                path: path.clone(),
                source,
            }
        })?;
        if fork.descriptor.id != self.id {
            return Err(PluginForkError::Mismatch {
                path,
                expected: self.id,
                found: fork.descriptor.id.clone(),
            });
        }
        fork.bridge
            .load_state(&self.state)
            .map_err(PluginForkError::LoadState)?;

        self.sources.install(&fork.controls);
        let watch = Arc::new(ForkWatch {
            controls: fork.controls.clone(),
            planned: AtomicUsize::new(usize::MAX),
            bridge: Arc::downgrade(&fork.bridge),
            server: Arc::downgrade(&fork.process_guard),
            gave_up: AtomicBool::new(false),
            died: Mutex::new(None),
            budget: timeout,
        });
        let mut fork = fork.bind();
        fork.state.params.seed(&self.delivered);
        if self.offline {
            // Advisory: a plugin without the concept keeps rendering as it
            // would live, which is not an error (`Features::RENDER_MODE`).
            let _ = fork.set_render_mode(RenderMode::Offline);
            fork.state.io.set_offline_wait(Arc::clone(&watch));
        }
        fork.fork_watch = Some(watch);
        Ok(fork)
    }
}

/// The fork as the graph inserts it: boxed bare, so it carries no fork
/// source of its own, with its health probe.
fn forked(fork: PluginClient<Bound>) -> Forked {
    let health = fork.fork_health();
    let forked = Forked::new(Box::new(fork));
    match health {
        Some(health) => forked.with_health(health),
        None => forked,
    }
}

impl ForkSource for PluginFork {
    fn fork(&self, mode: ForkMode) -> Result<Forked, ForkCause> {
        self.instance(mode).map(forked).map_err(ForkCause::new)
    }

    /// The live instance's state (and rate, and meter) is read here, at the
    /// gather; the `plugin-server` launch and the state load, half a second
    /// or more, are the build's, on whichever thread runs it (bevy-tutti's
    /// export: its compute pool).
    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        let capture = self.capture(mode).map_err(ForkCause::new)?;
        Ok(ForkBuild::deferred(move || {
            capture.build().map(forked).map_err(ForkCause::new)
        }))
    }
}

impl<S> PluginClient<S> {
    /// Creates a fresh instance of this plugin carrying this one's saved state,
    /// bound and ready to insert.
    ///
    /// This is a fork by state transfer. Call on a control thread; it blocks
    /// on a subprocess launch and two state transfers (half a second or
    /// more).
    ///
    /// `mode` is the graph's: [`ForkMode::Offline`] tells the plugin it is
    /// rendering offline and makes it wait for each chunk; [`ForkMode::Live`]
    /// does neither. Either way the fork reads the transport from the `Env`
    /// of the graph it is rendered in. A fork does not carry live MIDI or running
    /// DSP state (voices, delay lines, a reverb's tail): the state is what the
    /// plugin saves for a project, so a fork starts silent.
    ///
    /// This instance is only asked for its state. What the fork renders, and
    /// any parameter changed on either afterwards, does not reach the other.
    ///
    /// Inserting a bound `PluginClient` into a graph ([`IntoNode`]) hands the
    /// editor a fork source that calls this, so `Editor::fork` forks a graph
    /// holding a plugin.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginForkError`] naming the step that failed: the live
    /// instance is gone or would not save its state, the new instance failed
    /// to load, it turned out to be a different plugin, or it refused the
    /// state. A partly started fork is shut down before this returns.
    pub fn fork_instance(&self, mode: ForkMode) -> Result<PluginClient<Bound>, PluginForkError> {
        PluginFork::of(self).instance(mode)
    }

    /// The [`ForkSource`] [`IntoNode::into_parts`] hands the editor: a fork
    /// of this plugin by state transfer ([`fork_instance`](Self::fork_instance)).
    ///
    /// For a host that inserts the plugin through its own node builder rather
    /// than `IntoNode`, and must still hand the editor a way to fork it:
    /// `NodeParts { node, controls, fork: Some(client.fork_source()) }`.
    /// Holds nothing that keeps this instance alive (see
    /// [`PluginForkError::LiveGone`]).
    pub fn fork_source(&self) -> Box<dyn ForkSource> {
        Box::new(PluginFork::of(self))
    }

    /// On a fork ([`fork_instance`](Self::fork_instance)): the probe that
    /// says whether it has failed while rendering — crashed, or timed out and
    /// now rendering silence (the cause is a [`PluginRenderFault`]). `None`
    /// on an instance that is not a fork. Check it after rendering; see
    /// "When the fork fails while rendering" in the `fork` module docs.
    pub fn fork_health(&self) -> Option<Arc<dyn ForkHealth>> {
        self.fork_watch
            .as_ref()
            .map(|w| Arc::clone(w) as Arc<dyn ForkHealth>)
    }
}

/// A bound plugin as a graph node: the node itself, the [`PluginControls`] a
/// host drives it through from then on, and a [`ForkSource`] that forks it by
/// state transfer (a fresh instance in its own subprocess, loaded with the
/// live instance's saved state).
///
/// Only [`Bound`] implements this: an unbound plugin is not a node.
impl IntoNode for PluginClient<Bound> {
    type Controls = PluginControls;

    fn into_node(self) -> (Box<dyn Node>, PluginControls) {
        let controls = self.controls();
        (Box::new(self), controls)
    }

    fn into_parts(self) -> NodeParts<PluginControls> {
        let fork = PluginFork::of(&self);
        let (node, controls) = self.into_node();
        NodeParts {
            node,
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::SampleRate;
    use tutti_plugin_types::PluginTail;

    /// A fork's health reports a latency that moved after its graph was
    /// compiled — `Failed`, with `PluginRenderFault::LatencyChanged` naming
    /// both figures — and nothing before `plan` or while the figure holds.
    ///
    /// Mutation: have `latency_moved` return `None` → the moved figure is not
    /// reported → fails. Mutation: compare the plugin's own latency (without
    /// the pipeline chunk) → the unmoved plan reads as moved → fails.
    #[test]
    fn a_latency_moved_after_the_plan_is_a_fault() {
        let controls = PluginControls::new(
            Samples(137),
            PluginTail::default(),
            SampleRate(48_000.0),
            false,
        );
        let watch = ForkWatch {
            controls: controls.clone(),
            planned: AtomicUsize::new(usize::MAX),
            bridge: Weak::new(),
            server: Weak::new(),
            gave_up: AtomicBool::new(false),
            died: Mutex::new(None),
            budget: Duration::from_secs(1),
        };
        assert!(watch.fault().is_none(), "not planned yet");
        controls.set_pipeline(Samples(64));
        watch.plan(controls.declared_latency());
        assert!(watch.fault().is_none(), "the plan holds");
        controls.set_latency(Samples(161));
        let (kind, cause) = watch.fault().expect("the latency moved");
        assert_eq!(kind, ForkFaultKind::Failed);
        match cause.downcast_ref::<PluginRenderFault>() {
            Some(PluginRenderFault::LatencyChanged { planned, now }) => {
                assert_eq!(planned.samples(), Samples(137 + 64));
                assert_eq!(now.samples(), Samples(161 + 64));
            }
            other => panic!("expected LatencyChanged, got {other:?}"),
        }
    }
}
