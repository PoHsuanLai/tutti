//! The client of a served plugin ([`PluginClient`]): the generic plugin node
//! (`tutti_plugin_host::PluginNode`) over the served hosting
//! (`tutti_plugin_ipc::Ipc`), with the control surface the host keeps beside it.
//!
//! There is one node. `PluginClient` is what loads and launches the
//! `plugin-server`, builds the node over its [`Ipc`] (the
//! pipeline, the MIDI-out, the transport walk across a chunk, the parameter
//! delivery are the generic node's and `Ipc`'s own), and keeps what is not the
//! node's: the [`PluginControls`], the handle's sinks, and a fork source.
//!
//! # Loaded, then bound
//!
//! A client is born [`Unbound`] ([`PluginClient::new`]): a loaded plugin with
//! its control surface, not yet a node. [`bind`](PluginClient::bind) turns it
//! into a [`PluginClient<Bound>`], which holds the node and is the only state
//! that goes into a graph ([`tutti_graph::IntoNode`]). Inserting it hands back
//! its [`PluginControls`]:
//!
//! ```no_run
//! use tutti_graph::Editor;
//! use tutti_plugin::handles::PluginClient;
//! use tutti_plugin_host::PluginControls;
//! use tutti_graph::NodeKey;
//! use tutti_transport::TempoReader;
//! fn insert(client: PluginClient, tempo: TempoReader, editor: &mut Editor) -> PluginControls {
//!     editor.insert(NodeKey(1), "plugin", client.bind(tempo))
//! }
//! ```
//!
//! An unbound plugin can be neither inserted into a graph —
//!
//! ```compile_fail,E0277
//! use tutti_graph::Editor;
//! use tutti_plugin::handles::PluginClient;
//! use tutti_graph::NodeKey;
//! fn insert(client: PluginClient, editor: &mut Editor) {
//!     // `PluginClient` is `PluginClient<Unbound>`: not a node.
//!     editor.insert(NodeKey(1), "plugin", client);
//! }
//! ```
//!
//! — nor boxed as a node to be processed by hand:
//!
//! ```compile_fail,E0277
//! use tutti_graph::Node;
//! use tutti_plugin::handles::PluginClient;
//! fn boxed(client: PluginClient) -> Box<dyn Node> {
//!     Box::new(client)
//! }
//! ```
//!
//! (Mutation-tested: an `IntoNode` impl for `PluginClient<Unbound>` makes the
//! first compile and fail as a doctest; a `Node` impl makes both.)
//!
//! Inserting a bound client hands back its [`PluginControls`] (its
//! `IntoNode::Controls`) and a fork source, so the host never needs to reach
//! into the graph for the node again.
//!
//! # What the node reads
//!
//! - **Audio**, planar `f32`, through the pipelined IPC
//!   `Batcher` (`IpcProcessor`'s): whole chunks of one device callback (the
//!   graph's `MaxBlock` when the host does not say), so the plugin's latency
//!   is its own plus one callback. The `f64` wire conversion stays inside it.
//! - **The transport**, from each chunk's frame of the block's `Env`
//!   (`transport_source`), plus the meter installed on its controls.
//! - **MIDI, parameter ramps, chords and scales** on its event input, sent
//!   with the chunk their frames fall in. A clip installed on the MIDI port is
//!   still polled out of band (see the `shape` docs of the generic node).
//!
//! Subprocess lifetime is `ProcessGuard`, held behind `Arc` here and in
//! [`PluginHandle`], so the subprocess dies when the
//! LAST Arc drops. Main-thread editor / state / parameter access is on
//! [`PluginHandle`].

mod capability_view;
mod fork;

#[cfg(test)]
mod ipc_node_tests;
#[cfg(test)]
mod mock_server;
#[cfg(test)]
mod param_delivery_tests;
// The block-budget suite driven by a REAL plugin subprocess, as opposed to
// `tutti-plugin-ipc`'s `process_pipeline_tests` mock servers. A unit test rather than an
// integration one because it asserts on `PluginBridge::settled_replies`, which
// is `#[cfg(test)]` — deliberately, since nothing in the shipping host reads it
// and an always-compiled accessor with no caller reads as API someone may
// depend on. `clap` because it needs a `plugin-server` built with that loader.
#[cfg(all(test, feature = "clap"))]
mod fork_late_rate_tests;
#[cfg(all(test, feature = "clap"))]
mod real_stall_tests;
// `IpcController`'s two surfaces against the real plugin, which answers every
// call (`tutti-plugin-ipc`'s mock server answers a few).
#[cfg(all(test, feature = "clap"))]
mod served_control_tests;
#[cfg(test)]
mod tests;

pub use capability_view::TransportView;
pub(crate) use capability_view::is_declined;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_graph::{IntoNode, Node, NodeParts, Shape};
use tutti_plugin_host::{
    InProcClient, InvalidateSink, ParameterChangeSink, PluginControls, PluginHandle, PluginNode,
    RefreshSink, client_over,
};
use tutti_plugin_ipc::{
    BridgeConfig, BridgeEvent, Ipc, IpcController, IpcOrigin, Launched, PluginBridge,
};
use tutti_plugin_types::{
    BridgeError, BridgeResult, Features, LoadStage, LoadedPlugin, MaxBlock, Normalized, ParamId,
    ParameterChanges, PluginDescriptor, PluginInvalidation, PluginTail, Prepare, ResyncClass,
    SampleFormat,
};
use tutti_types::{ChannelLayout, SampleRate, Samples};

use self::fork::{ForkWatch, ServedFork};

/// The typestate of a [`PluginClient`] that is loaded but not yet a graph node.
///
/// [`PluginClient::new`] returns this state; [`PluginClient::bind`] leaves it.
#[derive(Debug)]
pub struct Unbound {
    /// The generic client over the served plugin, activated at load. Boxed: it
    /// is tens of KiB, and the client moves several times on its way into a
    /// graph (a debug build's frames hold a copy each).
    client: Box<InProcClient<Ipc>>,
}

/// The typestate of a [`PluginClient`] that holds its node and can go into a
/// graph.
///
/// Reached through [`PluginClient::bind`]. Only `PluginClient<Bound>` is an
/// [`IntoNode`].
pub struct Bound {
    /// The generic node over the served hosting: what the graph runs. Boxed
    /// for the reason [`Unbound`]'s client is.
    node: Box<PluginNode<Ipc>>,
    /// The project's tempo map reader the node was bound to, kept so a fork
    /// clones it on the control side (its own registration on the same cell).
    tempo: tutti_transport::TempoReader,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound").finish_non_exhaustive()
    }
}

/// An out-of-process plugin as a graph node: [`Unbound`] when loaded,
/// [`Bound`] once it can go into a graph.
///
/// [`PluginClient::new`] launches the plugin-server subprocess and loads the
/// plugin. [`bind`](PluginClient::bind) hands the project's tempo map to the
/// node and returns a `PluginClient<Bound>`, the only state that implements
/// [`IntoNode`]. Inserting it into a graph hands back
/// its [`PluginControls`], so the host never needs to reach into the graph for
/// the node again. Most hosts get a client through
/// [`Plugin::open`](crate::catalog::Plugin::open) instead of calling `new`.
///
/// The node processes planar `f32` audio in chunks of one device callback
/// through a pipelined IPC path, so its latency is the plugin's own plus one
/// callback ([`PluginControls::declared_latency`] reports both). MIDI,
/// parameter automation, harmony and note expression come in through the
/// node's inputs and [`PluginControls`]; the transport is read from each
/// block. Editor, state and main-thread parameter access are on
/// [`PluginHandle`]. The subprocess
/// exits when the node and every handle built from it have been dropped.
///
/// Not `Clone`: a node exists once, in the graph that owns it.
///
/// # Examples
///
/// ```no_run
/// use tutti_graph::Editor;
/// use tutti_plugin::handles::PluginClient;
/// use tutti_plugin_host::PluginControls;
/// use tutti_graph::NodeKey;
/// use tutti_transport::TempoReader;
/// fn insert(client: PluginClient, tempo: TempoReader, editor: &mut Editor) -> PluginControls {
///     editor.insert(NodeKey(1), "plugin", client.bind(tempo))
/// }
/// ```
pub struct PluginClient<S = Unbound> {
    bridge: Arc<PluginBridge>,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
    format: SampleFormat,
    /// Total input ports (main + sidechain/aux) and output ports: the node's
    /// audio widths.
    inputs: usize,
    outputs: usize,
    /// The input slots, meter, latency, tail and sample rate — every cell
    /// shared with the node and the handles a host keeps, so a host holding
    /// [`controls`](Self::controls) reaches the running node without it.
    controls: PluginControls,
    /// Observers for plugin-originated unsolicited events. Shared with
    /// `PluginHandle` so callers can register callbacks via the handle
    /// and still see events driven by the bridge thread.
    param_sink: ParameterChangeSink,
    refresh_sink: RefreshSink,
    invalidate_sink: InvalidateSink,
    /// The served plugin's control side (its bridge and the server's lifetime
    /// guard), shared with `PluginClient::handle`.
    served: IpcController,
    /// Where this instance came from, so a fork can load another of it.
    origin: Option<IpcOrigin>,
    /// On a **fork** only: whether it has failed while rendering
    /// ([`fork_health`](PluginClient::fork_health)).
    fork_watch: Option<Arc<ForkWatch>>,
    /// [`Unbound`] or [`Bound`].
    state: S,
}

impl<S> std::fmt::Debug for PluginClient<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginClient")
            .field("name", &self.descriptor.name)
            .field("state", &std::any::type_name::<S>())
            .finish_non_exhaustive()
    }
}

impl PluginClient<Unbound> {
    /// Spawns the plugin-server, loads `plugin_path` and returns the loaded
    /// plugin, [`Unbound`].
    ///
    /// Blocks while the subprocess launches and completes its handshake. The
    /// subprocess exits when the graph has dropped the node and every
    /// [`PluginHandle`] built from this
    /// client has been dropped.
    ///
    /// # Errors
    ///
    /// Returns a [`BridgeError`] if the plugin-server
    /// cannot be found or spawned, the handshake fails or times out, the
    /// plugin fails to load, or the IPC bridge cannot be set up.
    pub fn new(
        config: BridgeConfig,
        plugin_path: PathBuf,
        sample_rate: impl Into<SampleRate>,
    ) -> BridgeResult<Self> {
        Self::new_with_state(config, plugin_path, sample_rate, None).map(|(client, _)| client)
    }

    /// [`new`](Self::new), restoring `state` into the plugin **before it is
    /// activated**, the order every format allows and a plugin that takes its
    /// mode from the state at activation needs. The second value is what became
    /// of it: `None` when none was given, else the plugin's refusal as text (the
    /// plugin is then loaded at its defaults, which beats no plugin).
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn new_with_state(
        config: BridgeConfig,
        plugin_path: PathBuf,
        sample_rate: impl Into<SampleRate>,
        state: Option<&[u8]>,
    ) -> BridgeResult<(Self, Option<std::result::Result<(), String>>)> {
        let Launched { ipc, restored } = Ipc::launch(
            config,
            plugin_path,
            sample_rate.into(),
            state,
            crate::format::gui::load_gui_instance,
        )?;
        Self::over(ipc).map(|client| (client, restored))
    }

    /// The client over a served plugin that is running: the controls and the
    /// sinks the bridge's events go to, and the generic node, activated at the
    /// slab's chunk ceiling (a graph's `prepare` then narrows it). Needs no
    /// main thread.
    fn over(ipc: Ipc) -> BridgeResult<Self> {
        let bridge = Arc::clone(ipc.bridge());
        let descriptor = ipc.descriptor().clone();
        let loaded = ipc.loaded().clone();
        let rate = ipc.rate();
        let failed = |reason: String| BridgeError::LoadFailed {
            path: PathBuf::new(),
            stage: LoadStage::Activation,
            reason,
        };
        // The pipeline's chunk ceiling, matching the slab (`Ipc` clamps it to
        // `MAX_CHUNK` for the same reason): the raw `max_buffer_size` would
        // stage 8192 samples per channel to ship 64.
        let prepare = MaxBlock::new(Samples(ipc.ceiling()))
            .and_then(|block| Prepare::new(rate, block))
            .ok_or_else(|| failed(format!("the plugin cannot run at {rate:?} Hz")))?;

        let controls = PluginControls::with_chunks(
            loaded.latency_samples,
            loaded.tail,
            rate,
            Arc::clone(ipc.chunk_counters()),
        );
        controls
            .delivery_counters()
            .attach_bridge_midi(bridge.dropped_midi_cell());
        let param_sink = ParameterChangeSink::new();
        let refresh_sink = RefreshSink::new();
        let invalidate_sink = InvalidateSink::new();

        // Route the bridge's events into the shared cells and sinks, through
        // `Ipc::forward_events`. The listener runs on the bridge thread and
        // must be cheap. A latency change writes the latency cell directly, so
        // `PluginControls::declared_latency` reads the new figure, then fires
        // the *invalidate* sink (latency is a structural invalidation — it
        // re-plans PDC, which a host does with `Editor::set_latency`). Resync
        // signals split by consequence into the refresh (cosmetic) vs
        // invalidate (structural) sinks via `ResyncKind::classify`.
        let listener_latency = controls.latency_cell();
        let listener_tail = controls.tail_cell();
        let listener_param_sink = param_sink.clone();
        let listener_refresh_sink = refresh_sink.clone();
        let listener_invalidate_sink = invalidate_sink.clone();
        ipc.forward_events(Some(Arc::new(move |ev| match ev {
            BridgeEvent::LatencyChanged { samples } => {
                // The atomic is the terminus: `AtomicUsize` needs a
                // primitive, so the unit type stops here.
                listener_latency.store(samples.get(), Ordering::Release);
                listener_invalidate_sink.fire(PluginInvalidation::Latency { samples });
            }
            BridgeEvent::TailChanged { tail } => {
                *tutti_plugin_host::lock_tail(&listener_tail) = tail;
                listener_invalidate_sink.fire(PluginInvalidation::Tail { tail });
            }
            BridgeEvent::ParameterChanged { index, value } => {
                listener_param_sink.fire((index, value));
            }
            BridgeEvent::Resync(kind) => match kind.classify() {
                ResyncClass::Refresh(r) => listener_refresh_sink.fire(r),
                ResyncClass::Invalidate(i) => listener_invalidate_sink.fire(i),
            },
            BridgeEvent::RateRefusal(Some((rate, reason))) => {
                listener_invalidate_sink.fire(PluginInvalidation::RateRefused { rate, reason });
            }
            BridgeEvent::RateRefusal(None) => {
                listener_invalidate_sink.fire(PluginInvalidation::RateRestored);
            }
            // Structural and terminal. No cell is updated alongside it the
            // way latency and tail are: there is no new value to cache, and the
            // crash flag this mirrors was already set by the bridge thread
            // before it fired — see `thread::crash`.
            BridgeEvent::Crashed { cause } => {
                listener_invalidate_sink.fire(PluginInvalidation::Crashed { cause });
            }
        })));

        // The parameter list, once, while loading blocks anyway: the node's
        // per-parameter delivery is sized from it. A `prepare` may not make
        // this round trip; a load already has.
        let params: Vec<ParamId> = match bridge.parameters() {
            Some(list) => list.into_iter().map(|p| p.id).collect(),
            None => {
                tracing::warn!(
                    "plugin-server listed no parameters at load; parameter \
                     delivery falls back to its spare slots"
                );
                Vec::new()
            }
        };

        let origin = ipc.origin().cloned();
        let format = ipc.format();
        let served = ipc.controller();
        // Report the FULL input width (main + sidechain/aux input buses) so a
        // graph edge into port 1 lands on a real sidechain port.
        let (inputs, outputs) = (loaded.total_inputs(), loaded.total_outputs());
        let (processor, controller) = ipc
            .activate_anywhere(prepare)
            .map_err(|refused| failed(refused.reason.to_string()))?;
        let client = client_over::<Ipc>(
            processor,
            controller,
            prepare,
            descriptor.clone(),
            &loaded,
            params,
            ParameterChanges::SPARE_QUEUES,
            Some(controls.clone()),
        );
        Ok(Self {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            served,
            origin,
            fork_watch: None,
            state: Unbound {
                client: Box::new(client),
            },
        })
    }

    /// Hands the node the reader of the project's tempo map it tells the
    /// plugin the tempo from (and CLAP its slope), lent once per block, and
    /// returns the [`PluginClient<Bound>`] that can go into a graph.
    ///
    /// Call on the control thread. Insert the result with `Editor::insert`,
    /// which prepares it and hands back its [`PluginControls`].
    ///
    /// Install per-block sources before or after — they live on shared cells
    /// ([`controls`](Self::controls)), so an install through a handle reaches
    /// the running node.
    ///
    /// `Session` hands every plugin it inserts a reader of its own
    /// (`TransportSettings::tempo_reader`); a host with no session builds one
    /// over a fixed map ([`TempoReader::fixed`](tutti_transport::TempoReader::fixed)).
    /// A fork of the node clones it.
    #[must_use]
    pub fn bind(self, tempo: tutti_transport::TempoReader) -> PluginClient<Bound> {
        let Self {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            served,
            origin,
            fork_watch,
            state: Unbound { client },
        } = self;
        PluginClient {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            served,
            origin,
            fork_watch,
            state: Bound {
                node: Box::new((*client).bind(tempo.clone())),
                tempo,
            },
        }
    }
}

impl<S> PluginClient<S> {
    /// The shape the plugin's node declares: its channels as audio ports, one
    /// MIDI event input, one MIDI event output if it has MIDI-out, and its
    /// latency and tail now (the node's `shape`). Read before binding, so a
    /// host can tell a replacement's ports from the running node's.
    pub fn declared_shape(&self) -> Shape {
        let midi_out = u16::from(self.loaded.features.contains(Features::MIDI_OUT));
        let width = |n: usize| ChannelLayout::from_count(u16::try_from(n).unwrap_or(u16::MAX));
        // Ports only: the node states the plugin's buses when it is bound.
        Shape::EMPTY
            .with_ports(width(self.inputs), width(self.outputs))
            .with_events(1, midi_out)
            .with_latency(self.controls.declared_latency())
            .with_tail(self.controls.tail())
    }

    /// The main-thread control handle for this plugin: editor, parameters,
    /// state.
    ///
    /// Call this before moving the client into a graph. The handle shares the
    /// subprocess's lifetime with the client, and the subprocess backend
    /// honors every capability, including the editor.
    pub fn handle(&self) -> PluginHandle {
        let served = Arc::new(self.served.clone());
        let pid = served.server_pid();
        PluginHandle::from_backend(
            Arc::clone(&served),
            Some(served),
            self.descriptor().clone(),
            self.loaded().clone(),
            self.param_sink().clone(),
        )
        .with_refresh_sink(self.refresh_sink().clone())
        .with_invalidate_sink(self.invalidate_sink().clone())
        .with_server_pid(pid)
    }

    pub(crate) fn param_sink(&self) -> &ParameterChangeSink {
        &self.param_sink
    }

    pub(crate) fn refresh_sink(&self) -> &RefreshSink {
        &self.refresh_sink
    }

    pub(crate) fn invalidate_sink(&self) -> &InvalidateSink {
        &self.invalidate_sink
    }

    /// Returns the plugin's reported latency in **frames**: its own figure, without
    /// the chunk the IPC pipeline adds
    /// ([`PluginControls::declared_latency`] has both).
    ///
    /// RT-safe: one atomic load, no allocation.
    pub fn latency(&self) -> Samples {
        self.controls.latency()
    }

    /// Overrides the plugin's reported latency. RT-safe.
    ///
    /// Normally driven by the bridge thread when the plugin-server emits
    /// `BridgeMessage::LatencyChanged` (forwarded by `PluginClient::new`);
    /// exposed publicly so callers can also force a value. It changes what
    /// [`PluginControls::declared_latency`] reports, and a graph re-plans PDC
    /// around it when the host hands that to `Editor::set_latency` — the
    /// node's `Shape` changes at the next commit. Register a callback via
    /// `PluginHandle::on_invalidate` (latency arrives as
    /// `PluginInvalidation::Latency`) to get notified.
    pub fn set_latency(&self, samples: impl Into<Samples>) {
        self.controls.set_latency(samples);
    }

    /// Returns what the plugin currently reports for its tail.
    ///
    /// Starts as the value read at load and tracks runtime changes for formats
    /// that signal them (CLAP).
    pub fn tail(&self) -> PluginTail {
        self.controls.tail()
    }

    /// Overrides the plugin's reported tail. Control side: takes a short lock
    /// (nothing on the audio thread reads the tail).
    ///
    /// Normally driven by the bridge thread when the plugin-server emits
    /// `BridgeMessage::TailChanged`; exposed publicly so callers can also force
    /// a value. Like `set_latency`, this changes what the node reports without
    /// re-running anything — an offline render reads the tail when it sizes
    /// itself, so a bounce already in flight keeps the length it started with.
    pub fn set_tail(&self, tail: PluginTail) {
        self.controls.set_tail(tail);
    }

    /// Returns a handle on this node's host-side controls that stays valid
    /// after the node moves into a graph.
    ///
    /// The controls cover the input slots, meter, latency, tail and sample
    /// rate. Inserting a bound client hands the same handle back.
    ///
    /// Every cell in it is shared with this node, so a host takes this and
    /// drives the running plugin through it from then on, instead of reaching
    /// back into the graph for the node. See [`PluginControls`].
    pub fn controls(&self) -> PluginControls {
        self.controls.clone()
    }

    /// Returns the plugin's catalog identity (id, name, vendor, version, class,
    /// editor).
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Returns what the plugin reported at load: per-bus channel widths,
    /// latency and `f64` support.
    pub fn loaded(&self) -> &LoadedPlugin {
        &self.loaded
    }

    /// Returns the sample format negotiated with the plugin at load.
    pub fn format(&self) -> SampleFormat {
        self.format
    }

    /// Returns the number of audio input ports: main plus sidechain/aux buses.
    pub fn inputs(&self) -> usize {
        self.inputs
    }

    /// Returns the number of audio output ports.
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// Returns `true` once the connection to the plugin-server has failed (for
    /// example, the subprocess crashed); the node then outputs silence.
    pub fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }

    /// How many parameter-change events the plugin sent that the bridge
    /// dropped, over the client's life: more arrived during one command (a
    /// preset or state load) than the bridge holds between two listener
    /// calls, and the oldest went. Each drop also delivers a
    /// [`PluginRefresh::ParamValues`](tutti_plugin_types::PluginRefresh::ParamValues), so a
    /// host that re-reads on it loses nothing; latency, tail and resync events
    /// are never dropped.
    pub fn dropped_parameter_events(&self) -> u64 {
        self.bridge.dropped_parameter_events()
    }

    /// Queues a parameter change for the plugin. RT-safe, fire-and-forget.
    ///
    /// Main-thread parameter reads and writes are on
    /// [`PluginHandle`]; this lets a
    /// builder set initial values before any handle exists.
    pub fn set_parameter(&self, param_id: ParamId, value: Normalized) {
        let _ = self.bridge.set_parameter_rt(param_id, value);
    }

    /// Sends the host [`AutomationMode`](tutti_plugin_types::AutomationMode) to the
    /// plugin.
    ///
    /// RT-safe, fire-and-forget; a no-op for plugins and formats without an
    /// automation-state concept.
    pub fn set_automation_state(&self, mode: tutti_plugin_types::AutomationMode) {
        let _ = self.bridge.set_automation_state_rt(mode);
    }

    /// Sends the [`RenderMode`](tutti_plugin_types::RenderMode) to the plugin.
    ///
    /// Queued on the same command bus as the blocks around it, so the change
    /// lands between two blocks rather than overtaking one in flight.
    ///
    /// Returns whether the command was *queued*, which is not whether the
    /// plugin honoured it: the server applies it asynchronously, and whether a
    /// given plugin acts on the mode is the load-time
    /// [`Features::RENDER_MODE`](tutti_plugin_types::Features) capability the
    /// caller already has. `false` here means the bridge is crashed or its
    /// queue is full. The served control answers from any thread.
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        self.bridge.set_render_mode_rt(mode)
    }

    /// The bridge, for the tests that script or read the server.
    #[cfg(all(test, feature = "clap"))]
    pub(crate) fn bridge(&self) -> Arc<PluginBridge> {
        Arc::clone(&self.bridge)
    }
}

/// A bound plugin as a graph node: the node itself (the generic
/// `PluginNode<Ipc>`), the [`PluginControls`] a host drives it through from
/// then on, and a [`ForkSource`](tutti_graph::ForkSource) that forks it by state
/// transfer (a fresh instance in its own subprocess, loaded with the live
/// instance's saved state).
///
/// Only [`Bound`] implements this: an unbound plugin is not a node.
impl IntoNode for PluginClient<Bound> {
    type Controls = PluginControls;

    fn into_node(self) -> (Box<dyn Node>, PluginControls) {
        let controls = self.controls();
        (self.state.node, controls)
    }

    fn into_parts(self) -> NodeParts<PluginControls> {
        let fork = ServedFork::of(&self);
        let (node, controls) = self.into_node();
        NodeParts {
            node,
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}
