//! Hosted plugins in the session (feature `plugin`; design 015 §4.2-§4.4):
//! the load as a job ([`PluginLoad`]), the add ([`Session::add_plugin`]),
//! the record the session keeps for each one ([`HostedPlugin`]), and the
//! work of [`Phase::Plugins`](crate::Phase::Plugins) and
//! [`Phase::RenderMode`](crate::Phase::RenderMode).
//!
//! # The record
//!
//! Each plugin the graph holds (`Graph::insert_plugin`,
//! [`Session::add_plugin`], [`Graph::host_plugin`]) has a [`HostedPlugin`],
//! keyed by its [`AudioNode`]: its handle, its controls (an out-of-process
//! plugin's latency and meter cells), its liveness, whether it was told
//! the render mode, its last snapshot, and (`modulation`) its parameter
//! mirror. A crossfade to another plugin (`Graph::crossfade_plugin`,
//! `replace_plugin`) carries the record over to the incoming plugin, with
//! nothing of the outgoing plugin's state (its snapshot, an in-flight
//! snapshot job, its mirror); replacing it with a native node
//! (`Graph::replace`, `crossfade`) or removing the node drops it.
//!
//! # `Phase::Plugins`
//!
//! Per plugin, in node order:
//!
//! - **Health.** `tutti_plugin::Liveness` observes the handle's status at
//!   `PhaseCx::now`: dead after at least `Liveness::DEATHS` consecutive
//!   unhealthy observations spanning at least the window. A dead plugin's
//!   node is removed ([`Session::remove`]) and reported once,
//!   [`SessionEvent::PluginDied`](crate::SessionEvent::PluginDied).
//! - **Latency.** The figure its node declares now
//!   (`PluginControls::declared_latency`) is handed to the graph when it
//!   moved ([`Graph::refresh_node_latency`]):
//!   [`SessionEvent::PluginLatencyChanged`](crate::SessionEvent::PluginLatencyChanged).
//! - **GUI edits** (`modulation`). The values the plugin reported go into
//!   its mirror, and into the knob of the lane on that parameter when it
//!   has one (doc 014 §8.2). A mirror rebuilt by a crossfade to another
//!   plugin is read as a job the host places:
//!   [`SessionEvent::PluginParamsDue`](crate::SessionEvent::PluginParamsDue),
//!   whose [`run`](PluginParamRead::run) makes one round trip per
//!   parameter, landed with [`Session::land_params`]. Until then the
//!   mirror is not ready, and a lane on the plugin waits for it.
//! - **Snapshots.** A healthy plugin's state is captured every
//!   [`SNAPSHOT_INTERVAL`] of `now` (`SessionBuilder::snapshot_interval`),
//!   as a job the host places: [`SessionEvent::PluginSnapshotDue`](crate::SessionEvent::PluginSnapshotDue),
//!   whose [`run`](PluginSnapshot::run) is a blocking round trip, landed
//!   with [`Session::land_snapshot`]. One is in flight per plugin at most:
//!   the job holds the plugin's snapshot token, and a dropped job frees it.
//!
//! # `Phase::RenderMode` (with `export`)
//!
//! Offline while an export token lives (§4.3), Realtime otherwise; each
//! plugin is told per `tutti_plugin::needs_announcement`: on a change of
//! mode, or if it was never told.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_core::AudioNode;
use tutti_plugin::catalog::{AudioConfig, Plugin, PluginId};
use tutti_plugin::handles::{PluginControls, PluginHandle, PluginMeter, PluginStatus};
use tutti_plugin::{BridgeError, Liveness, Verdict};
use tutti_types::SampleRate;

use crate::{Session, SessionEvent};

/// How often a healthy plugin's state is captured, by default: 8 s, the
/// 512 frames at 60 Hz `bevy-tutti` counted before (design 015 §4.4).
/// [`SessionBuilder::snapshot_interval`](crate::SessionBuilder::snapshot_interval)
/// sets another.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(8);

/// Which plugin to load, and the state to restore into it.
#[derive(Debug, Clone)]
pub struct PluginLoadRequest {
    /// The plugin's file.
    pub id: PluginId,
    /// A saved state (`HostState::save_state`, a [`HostedPlugin::snapshot`])
    /// restored once it loads. A refusal is logged, and the plugin loads at
    /// its defaults.
    pub state: Option<Vec<u8>>,
}

/// A plugin load, as a job (design 015 §4.2): `Send`, and
/// [`run`](Self::run) blocks (a `plugin-server` launch, half a second to
/// fifteen), so the host runs it off its control thread and hands the
/// result to [`Session::add_plugin`].
///
/// Made by [`Session::plugin_load`], at the session's rate.
#[must_use = "a load does nothing until it is run and added"]
pub struct PluginLoad {
    path: PathBuf,
    rate: SampleRate,
    config: AudioConfig,
    state: Option<Vec<u8>>,
}

impl PluginLoad {
    /// Bridge settings other than the defaults (a catalog's, say).
    pub fn with_config(mut self, config: AudioConfig) -> Self {
        self.config = config;
        self
    }

    /// Instantiates at `rate` instead of the session's.
    pub fn at_rate(mut self, rate: SampleRate) -> Self {
        self.rate = rate;
        self
    }

    /// The plugin's file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the plugin is hosted in process (`Plugin::opens_in_process`:
    /// a VST2 plugin, with `vst2`): [`run`](Self::run) leaves it unopened,
    /// and [`Session::add_plugin`] opens it on the main thread, stalling
    /// that call for the library's init, the state restore and the
    /// parameter read.
    pub fn opens_in_process(&self) -> bool {
        Plugin::opens_in_process(&self.path)
    }

    /// Opens the plugin, restores the request's state, and (`modulation`)
    /// reads its parameter values, after the state: those are what a
    /// modulation lane's knob starts at. Any thread; it blocks.
    ///
    /// A plugin hosted in process (`Plugin::opens_in_process`: a VST2
    /// plugin, with `vst2`) is not opened here: VST2 confines its load to
    /// the main thread, so the job returns it unopened and
    /// [`Session::add_plugin`] opens it there (a library load and an
    /// instance's init, with no subprocess to wait for).
    pub fn run(self) -> PluginLoaded {
        let path = self.path.clone();
        let opened = if self.opens_in_process() {
            Opened::OnMain(self)
        } else {
            Opened::Done(Box::new(self.open()))
        };
        PluginLoaded { path, opened }
    }

    /// [`run`](Self::run)'s open, on this thread.
    fn open(self) -> Open {
        let plugin = Plugin::open_with(&self.config, &self.path, self.rate);
        if let (Ok(plugin), Some(state)) = (&plugin, &self.state) {
            // A refusal does not abort the load: a plugin at its defaults
            // beats no plugin. But it is not silent.
            if let Err(e) = plugin.handle().state().load_state(state) {
                tracing::warn!(
                    "{}: saved state was not restored ({e}); the plugin is loaded at its \
                     defaults",
                    plugin.handle().name()
                );
            }
        }
        #[cfg(feature = "modulation")]
        let mirror = plugin.as_ref().ok().map(|plugin| {
            let (mut mirror, read) = tutti_plugin::PluginParamValues::start(plugin.handle());
            mirror.land(read.run());
            mirror
        });
        Open {
            plugin,
            #[cfg(feature = "modulation")]
            mirror,
        }
    }
}

/// An opened plugin (or why it did not open) and (`modulation`) its
/// parameter values.
struct Open {
    plugin: Result<Plugin, BridgeError>,
    #[cfg(feature = "modulation")]
    mirror: Option<tutti_plugin::PluginParamValues>,
}

/// What a [`PluginLoad`]'s run did.
enum Opened {
    /// Opened (or failed to) on the job's thread. Boxed: a plugin is far
    /// larger than an unopened load.
    Done(Box<Open>),
    /// Hosted in process: opened by `add_plugin`, on the main thread.
    OnMain(PluginLoad),
}

/// A finished [`PluginLoad`]: the plugin, or why it did not load. `Send`;
/// [`Session::add_plugin`] takes it on the control thread.
#[must_use = "a loaded plugin goes into the graph with `Session::add_plugin`"]
pub struct PluginLoaded {
    path: PathBuf,
    opened: Opened,
}

impl PluginLoaded {
    /// The plugin's file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why the load failed, if it did. `None` for a plugin hosted in
    /// process too, which [`Session::add_plugin`] opens: its error is
    /// `add_plugin`'s.
    pub fn error(&self) -> Option<&BridgeError> {
        match &self.opened {
            Opened::Done(open) => open.plugin.as_ref().err(),
            Opened::OnMain(_) => None,
        }
    }
}

/// What the session believes about a plugin's liveness: the debounce
/// between the engine's latched `PluginStatus` and a node removed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PluginLiveness {
    /// Answering normally.
    #[default]
    Healthy,
    /// Recently unhealthy, not yet written off (`tutti_plugin::Liveness`).
    Failing {
        /// How many observations in a row found it unhealthy.
        consecutive: u8,
    },
    /// Written off: its node was removed. The engine offers no relaunch;
    /// load a replacement from its last [`snapshot`](HostedPlugin::snapshot).
    Dead {
        /// The engine's latched reason.
        cause: String,
    },
}

/// What the session keeps for one hosted plugin. See the module docs.
pub struct HostedPlugin {
    handle: PluginHandle,
    controls: Option<PluginControls>,
    meter: Option<PluginMeter>,
    liveness: Liveness,
    status: PluginLiveness,
    /// Told a render mode since it was added (or crossfaded in).
    pub(crate) told: bool,
    snapshot: Option<Vec<u8>>,
    /// Held by the snapshot job in flight, if one is.
    snapshot_token: Arc<()>,
    /// When the last snapshot was asked for, or (until the first) when the
    /// plugin was first observed.
    snapshot_at: Option<Instant>,
    #[cfg(feature = "modulation")]
    mirror: Option<tutti_plugin::PluginParamValues>,
    /// The read that fills a mirror a crossfade rebuilt, until
    /// `Phase::Plugins` hands it to the host as a job.
    #[cfg(feature = "modulation")]
    param_read: Option<tutti_plugin::ParamRead>,
    /// Held by that job: a later crossfade mints a new one, and the
    /// earlier read lands nowhere.
    #[cfg(feature = "modulation")]
    mirror_token: Arc<()>,
}

impl HostedPlugin {
    pub(crate) fn new(handle: PluginHandle) -> Self {
        Self {
            handle,
            controls: None,
            meter: None,
            liveness: Liveness::new(),
            status: PluginLiveness::Healthy,
            told: false,
            snapshot: None,
            snapshot_token: Arc::new(()),
            snapshot_at: None,
            #[cfg(feature = "modulation")]
            mirror: None,
            #[cfg(feature = "modulation")]
            param_read: None,
            #[cfg(feature = "modulation")]
            mirror_token: Arc::new(()),
        }
    }

    pub(crate) fn with_controls(mut self, controls: PluginControls) -> Self {
        self.meter = Some(controls.meter());
        self.controls = Some(controls);
        self
    }

    /// The incoming plugin of a crossfade at the same node: its handle and
    /// controls, a fresh debounce, and not yet told the render mode. Nothing
    /// of the outgoing plugin's state carries over: its snapshot is cleared
    /// (its bytes would restore the wrong plugin), a fresh snapshot token
    /// makes a job of the outgoing plugin still in flight land nowhere
    /// ([`Session::land_snapshot`]), and the snapshot clock restarts.
    ///
    /// (`modulation`) A record that kept a parameter mirror gets one for
    /// the incoming plugin, from its handle (where its reports now arrive),
    /// not yet ready: its values are read by a job (one round trip per
    /// parameter, so not on the calling thread), which the next
    /// `Phase::Plugins` hands to the host
    /// ([`SessionEvent::PluginParamsDue`](crate::SessionEvent::PluginParamsDue)).
    /// The outgoing mirror's report callback was on the outgoing handle,
    /// which reports nothing here once it is gone.
    pub(crate) fn replace(&mut self, handle: PluginHandle, controls: PluginControls) {
        self.adopt(handle);
        self.meter = Some(controls.meter());
        self.controls = Some(controls);
    }

    /// [`replace`](Self::replace)'s handle half: everything but the
    /// controls.
    fn adopt(&mut self, handle: PluginHandle) {
        #[cfg(feature = "modulation")]
        if self.mirror.is_some() {
            let (mirror, read) = tutti_plugin::PluginParamValues::start(&handle);
            self.mirror = Some(mirror);
            self.param_read = Some(read);
            self.mirror_token = Arc::new(());
        }
        self.handle = handle;
        self.liveness = Liveness::new();
        self.status = PluginLiveness::Healthy;
        self.told = false;
        self.snapshot = None;
        self.snapshot_token = Arc::new(());
        self.snapshot_at = None;
    }

    /// The plugin's control surface: editor, parameters, state.
    pub fn handle(&self) -> &PluginHandle {
        &self.handle
    }

    /// An out-of-process plugin's controls (its latency, tail and meter
    /// cells, shared with its node); `None` for a plugin hosted in process.
    pub fn controls(&self) -> Option<&PluginControls> {
        self.controls.as_ref()
    }

    /// The node's meter cell, either backend's; `None` for a plugin the
    /// host inserted itself ([`Graph::host_plugin`](crate::Graph::host_plugin)).
    pub fn meter(&self) -> Option<&PluginMeter> {
        self.meter.as_ref()
    }

    /// What the session believes about its liveness.
    pub fn liveness(&self) -> &PluginLiveness {
        &self.status
    }

    /// The last state captured while it was healthy: what a replacement
    /// load restores ([`PluginLoadRequest::state`]).
    pub fn snapshot(&self) -> Option<&[u8]> {
        self.snapshot.as_deref()
    }

    /// Its parameter values (`modulation`): read at load, kept current from
    /// what it reports; `None` for a plugin added without the read.
    #[cfg(feature = "modulation")]
    pub fn param_values(&self) -> Option<&tutti_plugin::PluginParamValues> {
        self.mirror.as_ref()
    }

    /// Parameter `id`'s value, as a modulation lane's knob starts at it:
    /// `None` while not read, `Some(None)` for one it does not list (or
    /// with no mirror: no knob to start from).
    #[cfg(feature = "modulation")]
    pub(crate) fn param_value(&self, id: u32) -> Option<Option<f32>> {
        self.mirror.as_ref().map_or(Some(None), |m| m.get(id))
    }
}

/// A healthy plugin's state capture, as a job: `Send`, and
/// [`run`](Self::run) is a blocking round trip (ten seconds at worst), so
/// the host runs it off its control thread and lands the result with
/// [`Session::land_snapshot`]. Dropping it unrun frees the plugin for the
/// next one.
#[must_use = "a snapshot does nothing until it is run and landed"]
pub struct PluginSnapshot {
    node: AudioNode,
    /// Boxed: a `SessionEvent` carries the job, and a handle is large.
    handle: Box<PluginHandle>,
    token: Arc<()>,
}

impl PluginSnapshot {
    /// The plugin's node.
    pub fn node(&self) -> AudioNode {
        self.node
    }

    /// Asks the plugin for its state. Any thread; it blocks.
    pub fn run(self) -> PluginSnapshotTaken {
        PluginSnapshotTaken {
            node: self.node,
            state: self.handle.state().save_state(),
            token: self.token,
        }
    }
}

impl std::fmt::Debug for PluginSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSnapshot")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// A finished [`PluginSnapshot`], for [`Session::land_snapshot`].
#[must_use = "land it with `Session::land_snapshot`"]
pub struct PluginSnapshotTaken {
    node: AudioNode,
    state: Result<Vec<u8>, tutti_plugin::error::StateError>,
    /// The record's token when the job was asked for: a crossfade to
    /// another plugin mints a new one, and the job no longer lands.
    token: Arc<()>,
}

impl PluginSnapshotTaken {
    /// The plugin's node.
    pub fn node(&self) -> AudioNode {
        self.node
    }
}

/// The read of a parameter mirror a crossfade rebuilt, as a job
/// (`modulation`): `Send`, and [`run`](Self::run) makes one round trip per
/// parameter, so the host runs it off its control thread and lands the
/// result with [`Session::land_params`]. Dropped unrun, the mirror stays
/// not ready (a lane on the plugin keeps waiting) until the next crossfade.
#[cfg(feature = "modulation")]
#[must_use = "a parameter read does nothing until it is run and landed"]
pub struct PluginParamRead {
    node: AudioNode,
    /// Boxed: a `SessionEvent` carries the job, and a handle is large.
    read: Box<tutti_plugin::ParamRead>,
    token: Arc<()>,
}

#[cfg(feature = "modulation")]
impl PluginParamRead {
    /// The plugin's node.
    pub fn node(&self) -> AudioNode {
        self.node
    }

    /// Reads every parameter the plugin lists. Any thread; it blocks.
    pub fn run(self) -> PluginParamsRead {
        PluginParamsRead {
            node: self.node,
            values: self.read.run(),
            token: self.token,
        }
    }
}

#[cfg(feature = "modulation")]
impl std::fmt::Debug for PluginParamRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginParamRead")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// A finished [`PluginParamRead`], for [`Session::land_params`].
#[cfg(feature = "modulation")]
#[must_use = "land it with `Session::land_params`"]
pub struct PluginParamsRead {
    node: AudioNode,
    values: std::collections::HashMap<u32, f32>,
    /// The record's mirror token when the job was asked for.
    token: Arc<()>,
}

#[cfg(feature = "modulation")]
impl PluginParamsRead {
    /// The plugin's node.
    pub fn node(&self) -> AudioNode {
        self.node
    }
}

/// `Phase::RenderMode`'s record: the mode last announced.
pub(crate) struct RenderModeState {
    pub(crate) announced: tutti_plugin::RenderMode,
}

impl Default for RenderModeState {
    fn default() -> Self {
        Self {
            announced: tutti_plugin::RenderMode::Realtime,
        }
    }
}

impl Session {
    /// A load of `request`'s plugin at the session's rate, as a job:
    /// [`run`](PluginLoad::run) it on any thread (it blocks), then hand the
    /// result to [`add_plugin`](Self::add_plugin).
    pub fn plugin_load(&self, request: PluginLoadRequest) -> PluginLoad {
        PluginLoad {
            path: request.id.path().to_path_buf(),
            rate: self.graph.prepared().sample_rate(),
            config: AudioConfig::default(),
            state: request.state,
        }
    }

    /// [`plugin_load`](Self::plugin_load) and its run, on this thread: it
    /// blocks for the whole launch.
    ///
    /// # Errors
    ///
    /// As [`add_plugin`](Self::add_plugin).
    pub fn add_plugin_blocking(
        &mut self,
        request: PluginLoadRequest,
    ) -> Result<AudioNode, BridgeError> {
        let done = self.plugin_load(request).run();
        self.add_plugin(done)
    }

    /// Inserts a loaded plugin into the graph (unwired) and hosts it: its
    /// controls and meter are captured as it goes in, the meter is the
    /// metronome's (time signature and bar), and its health, latency,
    /// parameter mirror and render mode are the session's from here
    /// ([`HostedPlugin`]). An out-of-process plugin is bound as it goes in
    /// (`PluginClient::bind`); an in-process VST2 plugin is opened here (its
    /// job left it unopened: VST2 confines the load to the main thread) and
    /// goes in as its own node. That open blocks this call for the
    /// library's init, the state restore and (`modulation`) the parameter
    /// read: a frame-driven host opens at most one per frame.
    ///
    /// A main-thread call (design 015 §4.2).
    ///
    /// # Errors
    ///
    /// Why the load failed.
    pub fn add_plugin(&mut self, loaded: PluginLoaded) -> Result<AudioNode, BridgeError> {
        crate::main_thread();
        let path = loaded.path;
        let Open {
            plugin,
            #[cfg(feature = "modulation")]
            mirror,
        } = match loaded.opened {
            Opened::Done(open) => *open,
            // A plugin hosted in process opens here, on the main thread.
            Opened::OnMain(load) => load.open(),
        };
        let plugin = plugin?;
        let handle = plugin.handle().clone();
        let meter = self.click_settings.meter_cell();
        let node = match plugin.into_client() {
            Ok(client) => {
                client.controls().set_meter(meter);
                self.graph.insert_plugin(client)
            }
            Err(plugin) => {
                let plugin_meter = plugin.meter();
                plugin_meter.set_meter(meter);
                let (node, _) = self.graph.insert(*plugin);
                let mut hosted = HostedPlugin::new(handle);
                hosted.meter = Some(plugin_meter);
                self.graph.plugins.insert(node, hosted);
                node
            }
        };
        #[cfg(feature = "modulation")]
        if let Some(hosted) = self.graph.plugins.get_mut(&node) {
            hosted.mirror = mirror;
        }
        tracing::info!("plugin '{}' added as {node:?}", path.display());
        Ok(node)
    }

    /// The plugin hosted at `node`.
    pub fn plugin(&self, node: AudioNode) -> Option<&HostedPlugin> {
        self.graph.plugins.get(&node)
    }

    /// Every hosted plugin, in node order.
    pub fn plugins(&self) -> impl Iterator<Item = (AudioNode, &HostedPlugin)> {
        self.graph.plugins.iter().map(|(&n, p)| (n, p))
    }

    /// Lands a finished snapshot: the plugin's last snapshot becomes it. A
    /// refused one is logged and keeps the previous snapshot (a good
    /// recovery point beats none). Returns whether it was stored. Frees the
    /// plugin for its next snapshot.
    ///
    /// A snapshot of a plugin no longer at its node (removed, or crossfaded
    /// to another plugin since the job was asked for) lands nowhere: it is
    /// the outgoing plugin's state, not a recovery point for the node.
    pub fn land_snapshot(&mut self, taken: PluginSnapshotTaken) -> bool {
        let PluginSnapshotTaken { node, state, token } = taken;
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(&token, &hosted.snapshot_token) {
            tracing::debug!("plugin {node:?}: a snapshot of its previous plugin was discarded");
            return false;
        }
        match state {
            Ok(state) => {
                hosted.snapshot = Some(state);
                true
            }
            Err(e) => {
                tracing::warn!("plugin {node:?}: periodic state snapshot failed: {e}");
                false
            }
        }
    }

    /// Lands a finished parameter read: the plugin's mirror holds its
    /// values and is ready. Returns whether it landed. A read of a plugin no
    /// longer at its node (removed, or crossfaded again since) lands
    /// nowhere.
    #[cfg(feature = "modulation")]
    pub fn land_params(&mut self, read: PluginParamsRead) -> bool {
        let PluginParamsRead {
            node,
            values,
            token,
        } = read;
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(&token, &hosted.mirror_token) {
            tracing::debug!(
                "plugin {node:?}: a parameter read of its previous plugin was discarded"
            );
            return false;
        }
        let Some(mirror) = hosted.mirror.as_mut() else {
            return false;
        };
        mirror.land(values);
        true
    }

    /// The render mode last announced to the hosted plugins.
    pub fn render_mode(&self) -> tutti_plugin::RenderMode {
        self.render_mode.announced
    }

    /// `Phase::Plugins`. See the module docs.
    pub(crate) fn plugins_phase(&mut self, cx: crate::PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        // `refresh_node_latency` collects (`Editor::set_latency`), and what
        // it collects may be a plugin whose drop tears down an editor.
        crate::main_thread();
        let now = cx.now;
        let interval = self.snapshot_interval;
        let mut dead = Vec::new();
        let nodes: Vec<AudioNode> = self.graph.plugins.keys().copied().collect();
        for node in nodes {
            let Some(hosted) = self.graph.plugins.get_mut(&node) else {
                continue;
            };
            if matches!(hosted.status, PluginLiveness::Dead { .. }) {
                continue;
            }
            let cause = match hosted.handle.status() {
                PluginStatus::Dead { cause } => Some(cause),
                _ => None,
            };
            match hosted.liveness.observe(cause.is_none(), now) {
                Verdict::Healthy => hosted.status = PluginLiveness::Healthy,
                Verdict::Failing { consecutive } => {
                    hosted.status = PluginLiveness::Failing { consecutive };
                }
                Verdict::Dead => {
                    // `Liveness` declares a death only on an unhealthy
                    // observation, and every unhealthy one carries the
                    // engine's latched cause.
                    let cause = cause.unwrap_or_default();
                    tracing::error!(
                        "plugin '{}' declared dead ({cause}); removing {node:?}",
                        hosted.handle.name()
                    );
                    hosted.status = PluginLiveness::Dead {
                        cause: cause.clone(),
                    };
                    dead.push((node, cause));
                    continue;
                }
            }

            // The figure its node declares now, onto the graph when it moved.
            let declared = hosted
                .controls
                .as_ref()
                .map(PluginControls::declared_latency);
            let healthy = hosted.status == PluginLiveness::Healthy;
            let due = healthy && Arc::strong_count(&hosted.snapshot_token) == 1 && {
                let since = *hosted.snapshot_at.get_or_insert(now);
                now.saturating_duration_since(since) >= interval
            };
            let snapshot = due.then(|| {
                hosted.snapshot_at = Some(now);
                PluginSnapshot {
                    node,
                    handle: Box::new(hosted.handle.clone()),
                    token: Arc::clone(&hosted.snapshot_token),
                }
            });

            #[cfg(feature = "modulation")]
            if let Some(read) = hosted.param_read.take() {
                out.push(SessionEvent::PluginParamsDue(PluginParamRead {
                    node,
                    read: Box::new(read),
                    token: Arc::clone(&hosted.mirror_token),
                }));
            }
            #[cfg(feature = "modulation")]
            if let Some(mirror) = hosted.mirror.as_mut() {
                let state = &self.mods.state;
                mirror.drain_edits(|id, value| {
                    if let Some(target) = state.plugin_target(node, id) {
                        state.set_knob(target, value);
                    }
                });
            }

            if let Some(declared) = declared {
                if self.graph.refresh_node_latency(node, declared) {
                    out.push(SessionEvent::PluginLatencyChanged {
                        node,
                        latency: self.graph.node_latency(node),
                    });
                }
            }
            if let Some(snapshot) = snapshot {
                out.push(SessionEvent::PluginSnapshotDue(snapshot));
            }
        }
        for (node, cause) in dead {
            self.remove(node);
            out.push(SessionEvent::PluginDied { node, cause });
        }
    }

    /// `Phase::RenderMode`: Offline while an export token lives, Realtime
    /// otherwise, told per `needs_announcement`. Without `export` nothing
    /// is ever announced: there is no bounce to announce.
    pub(crate) fn render_mode_phase(&mut self) {
        #[cfg(feature = "export")]
        {
            let wanted = if self.export.in_flight() {
                tutti_plugin::RenderMode::Offline
            } else {
                tutti_plugin::RenderMode::Realtime
            };
            let changed = self.render_mode.announced != wanted;
            for hosted in self.graph.plugins.values_mut() {
                if !tutti_plugin::needs_announcement(changed, hosted.told) {
                    continue;
                }
                // `false` means the plugin declined (a CLAP plugin without
                // `clap.render`): it renders the same either way, so there is
                // nothing to retry.
                let _ = hosted.handle.set_render_mode(wanted);
                hosted.told = true;
            }
            self.render_mode.announced = wanted;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The record's own rules over a fake backend (no `plugin-server`):
    //! the snapshot cadence and its one-in-flight token, a declined save,
    //! and a failing plugin's skipped snapshot. The death, the render mode
    //! and the export are pinned end to end over the CLAP test plugin, in
    //! `tests/plugin.rs`.

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tutti_plugin::handles::{Normalized, OptionalCapabilities, ParamAddress};
    use tutti_plugin::server::ParameterInfo;

    /// A backend that answers `state` (or declines), counts its saves, and
    /// is dead while `dead` is set.
    struct Fake {
        state: Option<Vec<u8>>,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        /// The parameters it lists, and their values.
        params: Vec<(ParamAddress, f32)>,
    }

    impl tutti_plugin::backend::HostParams for Fake {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            Some(
                self.params
                    .iter()
                    .map(|&(id, _)| ParameterInfo::new(id, "p"))
                    .collect(),
            )
        }
        fn parameter_value(&self, id: ParamAddress) -> Option<f32> {
            self.params.iter().find(|(a, _)| *a == id).map(|&(_, v)| v)
        }
        fn set_parameter_value(&self, _id: ParamAddress, _value: Normalized) {}
        fn is_crashed(&self) -> bool {
            self.dead.load(Ordering::SeqCst)
        }
        fn crash_cause(&self) -> Option<String> {
            self.is_crashed().then(|| "stream closed".to_string())
        }
        fn dropped_parameter_events(&self) -> u64 {
            0
        }
    }

    impl tutti_plugin::backend::HostState for Fake {
        fn save_state(&self) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.state
                .clone()
                .ok_or(tutti_plugin::error::StateError::NoStateRoute)
        }
        fn load_state(&self, _data: &[u8]) -> Result<(), tutti_plugin::error::StateError> {
            Ok(())
        }
    }

    struct Rig {
        session: Session,
        node: AudioNode,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        t0: Instant,
    }

    /// A detached session snapshotting every second, hosting a fake that
    /// answers `state` at a dc node.
    fn rig(state: Option<Vec<u8>>) -> Rig {
        let mut session = Session::builder()
            .snapshot_interval(Duration::from_secs(1))
            .build_detached(SampleRate(48_000.0))
            .unwrap();
        let saves = Arc::new(AtomicUsize::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                state,
                saves: Arc::clone(&saves),
                dead: Arc::clone(&dead),
                params: Vec::new(),
            }),
            OptionalCapabilities::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let (node, _) = session
            .graph()
            .insert(tutti_nodes::testing::Const::mono(0.0));
        assert!(session.graph().host_plugin(node, handle));
        Rig {
            session,
            node,
            saves,
            dead,
            t0: Instant::now(),
        }
    }

    /// A fake plugin's handle listing `params`, answering `state`, and the
    /// sink its reports go through (firing it is the plugin reporting).
    fn fake_handle(
        state: Option<Vec<u8>>,
        params: Vec<(ParamAddress, f32)>,
    ) -> (PluginHandle, tutti_plugin::backend::ParameterChangeSink) {
        let sink = tutti_plugin::backend::ParameterChangeSink::default();
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                state,
                saves: Arc::default(),
                dead: Arc::default(),
                params,
            }),
            OptionalCapabilities::default(),
            Default::default(),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    impl Rig {
        /// `Phase::Plugins` at `t0 + ms`: every event it reported.
        fn events_at(&mut self, ms: u64) -> Vec<SessionEvent> {
            let mut out = Vec::new();
            let cx = crate::PhaseCx::at(self.t0 + Duration::from_millis(ms));
            self.session.run_phase(crate::Phase::Plugins, cx, &mut out);
            out
        }

        /// `Phase::Plugins` at `t0 + ms`: the snapshot jobs it asked for.
        fn plugins_at(&mut self, ms: u64) -> Vec<PluginSnapshot> {
            self.events_at(ms)
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::PluginSnapshotDue(job) => Some(job),
                    _ => None,
                })
                .collect()
        }
    }

    /// **Snapshots are taken on the interval of `now`, not every update,
    /// and one at a time.** The first update starts the clock; updates
    /// inside the interval ask for nothing; the first at or past it asks
    /// once; while that job is unlanded nothing more is asked, however late
    /// (the old Bevy test could not cover this guard on a single-threaded
    /// pool: here the job is simply held); landing it stores the bytes and
    /// restarts the cadence; a dropped job frees the plugin too.
    ///
    /// Mutations (each run, each failed): the due check without the token
    /// (`strong_count == 1` taken away: the update at 5 s asks again while
    /// the first job is held); the interval check removed (the update at
    /// 0.5 s asks); `snapshot_at` not restarted on a job (the update at
    /// 6 s asks); `land_snapshot` not storing (the bytes check).
    #[test]
    fn snapshots_are_taken_on_the_interval_one_at_a_time() {
        let mut rig = rig(Some(vec![1, 2, 3]));
        assert!(
            rig.plugins_at(0).is_empty(),
            "the first update starts the clock"
        );
        assert!(rig.plugins_at(500).is_empty(), "inside the interval");
        let mut jobs = rig.plugins_at(1_000);
        assert_eq!(jobs.len(), 1, "one job at the interval");
        let job = jobs.pop().unwrap();
        assert_eq!(job.node(), rig.node);
        assert!(
            rig.plugins_at(5_000).is_empty(),
            "nothing more while the job is in flight"
        );
        let taken = std::thread::spawn(move || job.run()).join().unwrap();
        assert!(rig.session.land_snapshot(taken));
        assert_eq!(
            rig.session.plugin(rig.node).unwrap().snapshot(),
            Some(&[1u8, 2, 3][..])
        );
        assert_eq!(rig.saves.load(Ordering::SeqCst), 1, "one job is one save");
        assert_eq!(
            rig.plugins_at(5_500).len(),
            1,
            "the cadence restarted when the last job was asked for (at 1 s)"
        );
        // That job is dropped unrun (a cancelled task): the plugin is free
        // again, on the interval.
        assert!(rig.plugins_at(6_000).is_empty(), "inside the interval");
        assert_eq!(rig.plugins_at(6_500).len(), 1, "a dropped job frees it");
    }

    /// **A declined save keeps the previous snapshot.** A refusal (the
    /// plugin declines, or the bridge dropped mid-call) replacing a good
    /// recovery point with nothing is worse than a stale one.
    ///
    /// Mutation (run, failed): `land_snapshot` storing `None` on an error
    /// (the snapshot reads `None`).
    #[test]
    fn a_declined_save_keeps_the_previous_snapshot() {
        let mut rig = rig(None);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .snapshot = Some(vec![4, 2]);
        rig.plugins_at(0);
        let job = rig.plugins_at(1_000).pop().expect("a job at the interval");
        assert!(!rig.session.land_snapshot(job.run()), "declined");
        assert_eq!(
            rig.session.plugin(rig.node).unwrap().snapshot(),
            Some(&[4u8, 2][..]),
            "a declined save must keep the last good snapshot"
        );
    }

    /// **A failing plugin is not asked for its state**: the call would
    /// block against a peer that is not answering, and return nothing.
    ///
    /// Mutation (run, failed): the due check without `healthy` (a job is
    /// asked of the failing plugin at 1 s).
    #[test]
    fn a_failing_plugin_is_not_snapshotted() {
        let mut rig = rig(Some(vec![1]));
        rig.plugins_at(0);
        rig.dead.store(true, Ordering::SeqCst);
        assert!(rig.plugins_at(1_000).is_empty(), "failing: no snapshot");
        assert!(matches!(
            rig.session.plugin(rig.node).unwrap().liveness(),
            PluginLiveness::Failing { consecutive: 1 }
        ));
    }

    /// **A hosted plugin replaced by a native node is no longer hosted**
    /// (`Graph::replace`, and `Graph::crossfade` through it): its record
    /// goes, so a later death of the outgoing plugin removes nothing (the
    /// node is the native one's now) and reports nothing, and the record's
    /// handle no longer keeps the outgoing plugin alive.
    ///
    /// Mutation (run, failed): `Graph::replace` keeping the record (the
    /// `plugins.remove` taken out) → the dead fake is declared dead, the
    /// native node removed, and `PluginDied` names it.
    #[test]
    fn a_plugin_replaced_by_a_native_node_is_no_longer_hosted() {
        for crossfade in [false, true] {
            let mut rig = rig(None);
            let replacement = tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(1.0));
            let fade = tutti_core::Seconds(0.0);
            let curve = tutti_core::CrossfadeCurve::EqualAmplitude;
            if crossfade {
                assert!(matches!(
                    rig.session
                        .graph()
                        .crossfade(rig.node, replacement, fade, curve),
                    Ok(crate::Crossfade::Landed(_))
                ));
            } else {
                assert!(rig
                    .session
                    .graph()
                    .replace(rig.node, replacement, fade, curve)
                    .is_ok());
            }
            assert!(rig.session.plugin(rig.node).is_none(), "the record went");
            // The outgoing plugin dies, past the debounce's count and window.
            rig.dead.store(true, Ordering::SeqCst);
            for ms in [0, 100, 200, 300, 400] {
                let died = rig
                    .events_at(ms)
                    .into_iter()
                    .any(|e| matches!(e, SessionEvent::PluginDied { .. }));
                assert!(
                    !died,
                    "a native node is reported dead (crossfade: {crossfade})"
                );
            }
            assert!(
                rig.session.graph_ref().contains(rig.node),
                "the native replacement stays in the graph (crossfade: {crossfade})"
            );
        }
    }

    /// **A crossfade to another plugin does not inherit the outgoing
    /// plugin's recovery state.** Its last snapshot is cleared, and a
    /// snapshot job of the outgoing plugin still in flight lands nowhere
    /// (its token is not the record's any more); the incoming plugin gets
    /// its own on the interval from the crossfade. Driven through
    /// `HostedPlugin::adopt`, `replace`'s handle half (a crossfade to
    /// another plugin needs a `PluginClient`, a real `plugin-server`).
    ///
    /// Mutations (each run, each failed): `adopt` keeping the old token
    /// (the in-flight job lands, `[1, 2, 3]`); `land_snapshot` without the
    /// `ptr_eq` check (the same); `adopt` keeping the old snapshot (it
    /// reads `[1, 2, 3]` after the crossfade).
    #[test]
    fn a_crossfade_to_another_plugin_drops_the_old_snapshot() {
        let mut rig = rig(Some(vec![1, 2, 3]));
        rig.plugins_at(0);
        let first = rig.plugins_at(1_000).pop().expect("a job at the interval");
        assert!(rig.session.land_snapshot(first.run()));
        let in_flight = rig.plugins_at(2_000).pop().expect("the next job");

        let (incoming, _) = fake_handle(Some(vec![9]), Vec::new());
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming);
        assert_eq!(
            rig.session.plugin(rig.node).unwrap().snapshot(),
            None,
            "the outgoing plugin's snapshot is not the incoming one's"
        );
        assert!(
            !rig.session.land_snapshot(in_flight.run()),
            "the outgoing plugin's job must not land on the incoming plugin"
        );
        assert_eq!(rig.session.plugin(rig.node).unwrap().snapshot(), None);

        assert!(rig.plugins_at(2_000).is_empty(), "its clock starts here");
        let own = rig.plugins_at(3_000).pop().expect("its own job");
        assert!(rig.session.land_snapshot(own.run()));
        assert_eq!(
            rig.session.plugin(rig.node).unwrap().snapshot(),
            Some(&[9u8][..])
        );
    }

    /// **A crossfade to another plugin rebuilds the parameter mirror from
    /// the incoming plugin, read off the control thread.** Right after the
    /// crossfade the mirror is not ready (a lane waits rather than start
    /// from the outgoing plugin's value); the next `Phase::Plugins` hands
    /// the read to the host as a job, once; the job runs on another thread
    /// and lands the incoming plugin's values. A read asked for before a
    /// second crossfade lands nowhere. The read goes through the incoming
    /// handle, which `PluginParamValues::start` also gives the mirror's
    /// report callback; a fake's reports cannot be fired from outside
    /// `tutti-plugin`, so the callback is not asserted here.
    ///
    /// Mutations (each run, each failed): `adopt` keeping the old mirror
    /// (the value reads the outgoing plugin's 0.25); `adopt` running the
    /// read inline (`mirror.land(read.run())`, the old code: the mirror is
    /// ready before any job ran); the phase not handing the read out (no
    /// job); `land_params` without the `ptr_eq` check (the stale read
    /// lands); `adopt` keeping the old token (the same).
    #[cfg(feature = "modulation")]
    #[test]
    fn a_crossfade_to_another_plugin_rebuilds_the_mirror() {
        let param = ParamAddress::Index(3);
        let mut rig = rig(None);
        let (outgoing, _) = fake_handle(None, vec![(param, 0.25)]);
        let (mut mirror, read) = tutti_plugin::PluginParamValues::start(&outgoing);
        mirror.land(read.run());
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.mirror = Some(mirror);
        assert_eq!(hosted.param_value(3), Some(Some(0.25)), "precondition");

        let (incoming, _) = fake_handle(None, vec![(param, 0.75)]);
        hosted.adopt(incoming);
        assert_eq!(
            hosted.param_value(3),
            None,
            "not ready until the read lands: nothing was read on this thread"
        );

        let reads = |events: Vec<SessionEvent>| -> Vec<PluginParamRead> {
            events
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::PluginParamsDue(job) => Some(job),
                    _ => None,
                })
                .collect()
        };
        let mut jobs = reads(rig.events_at(0));
        assert_eq!(jobs.len(), 1, "the read is handed out as a job");
        assert!(reads(rig.events_at(10)).is_empty(), "once");
        let job = jobs.pop().unwrap();
        assert_eq!(job.node(), rig.node);
        let done = std::thread::spawn(move || job.run()).join().unwrap();
        assert!(rig.session.land_params(done));
        assert_eq!(
            rig.session.plugin(rig.node).unwrap().param_value(3),
            Some(Some(0.75)),
            "the mirror holds the incoming plugin's values"
        );

        // A read asked for, then a second crossfade: the first lands nowhere.
        let (second, _) = fake_handle(None, vec![(param, 0.5)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(second);
        let stale = reads(rig.events_at(20)).pop().expect("its read");
        let (third, _) = fake_handle(None, vec![(param, 0.125)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(third);
        assert!(
            !rig.session.land_params(stale.run()),
            "a read of the previous plugin must not land"
        );
        assert_eq!(rig.session.plugin(rig.node).unwrap().param_value(3), None);
        let own = reads(rig.events_at(30)).pop().expect("its own read");
        assert!(rig.session.land_params(own.run()));
        assert_eq!(
            rig.session.plugin(rig.node).unwrap().param_value(3),
            Some(Some(0.125))
        );
    }
}
