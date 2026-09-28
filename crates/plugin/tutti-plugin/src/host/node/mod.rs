//! The graph node for an out-of-process plugin ([`PluginClient`]).
//!
//! # Loaded, then bound
//!
//! A client is born [`Unbound`] ([`PluginClient::new`]): a loaded plugin with
//! its control surface, not yet a node. [`bind`](PluginClient::bind) turns it
//! into a [`PluginClient<Bound>`], which owns the audio path (the IPC
//! [`Batcher`](batcher::Batcher) and its scratch) and is the only state that
//! goes into a graph ([`tutti_graph::IntoNode`]), as the `Node` that
//! `graph_node` implements. Inserting it hands back its [`PluginControls`]:
//!
//! ```no_run
//! use tutti_graph::Editor;
//! use tutti_plugin::handles::{PluginClient, PluginControls};
//! use tutti_types::NodeKey;
//! fn insert(client: PluginClient, editor: &mut Editor) -> PluginControls {
//!     editor.insert(NodeKey(1), "plugin", client.bind())
//! }
//! ```
//!
//! An unbound plugin can be neither inserted into a graph —
//!
//! ```compile_fail,E0277
//! use tutti_graph::Editor;
//! use tutti_plugin::handles::PluginClient;
//! use tutti_types::NodeKey;
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
//!   [`Batcher`](batcher::Batcher): whole chunks of one device callback (the
//!   graph's `MaxBlock` when the host does not say), so the plugin's latency
//!   is its own plus one callback. The `f64` wire conversion stays inside it.
//! - **The transport**, from each chunk's frame of the block's `Env`
//!   (`transport_source`), plus the meter installed on its controls.
//! - **MIDI, parameter ramps, chords and scales** on its event input, sent
//!   with the chunk their frames fall in. A clip installed on the MIDI port is
//!   still polled out of band (see the `shape` docs in `graph_node`).
//!
//! Subprocess lifetime is [`ProcessGuard`], held behind `Arc` here and in
//! [`crate::host::handles::PluginHandle`], so the subprocess dies when the
//! LAST Arc drops. Main-thread editor / state / parameter access is on
//! [`crate::host::handles::PluginHandle`].

mod batcher;
mod capability_view;
mod controls;
mod fork;
mod graph_node;
// `input_slot` / `transport_source` are `pub(crate)` rather than private: the
// in-process VST2 node (`crate::format::vst2_in_process`) is a peer host, not a
// subprocess client, and reuses the same gated per-block plumbing and the same
// transport mapping rather than hand-rolling a second copy.
mod automation_node;
// Only the in-process VST2 node still polls a per-block input slot.
#[cfg(feature = "vst2")]
pub(crate) mod input_slot;
mod param_automation_source;
mod process;
pub(crate) mod transport_source;

#[cfg(test)]
mod machine_lock;
#[cfg(test)]
mod process_pipeline_tests;
// The block-budget suite driven by a REAL plugin subprocess, as opposed to
// `process_pipeline_tests`' mock servers. A unit test rather than an
// integration one because it asserts on `PluginBridge::settled_replies`, which
// is `#[cfg(test)]` — deliberately, since nothing in the shipping host reads it
// and an always-compiled accessor with no caller reads as API someone may
// depend on. `clap` because it needs a `plugin-server` built with that loader.
#[cfg(all(test, feature = "clap"))]
mod real_stall_tests;
#[cfg(test)]
mod tests;

// The shared node primitives (change sinks) live in `crate::util::node`;
// re-exported here so `crate::host::node::...` paths resolve.
pub use crate::util::node::ParameterChangeSink;
pub(crate) use crate::util::node::{InvalidateSink, RefreshSink};
pub use automation_node::{AutomationControls, PluginAutomation, AUTOMATION_EVENT_CAPACITY};
pub(crate) use capability_view::is_declined;
pub use capability_view::TransportView;
pub use controls::PluginControls;
pub use param_automation_source::{
    LfoCurve, LfoOffset, OffsetCurve, PluginParamTarget, TimedParam,
};
pub(crate) use process::ProcessGuard;
// The largest chunk that can cross the process edge. Re-exported because
// `subprocess::launch` sizes the shared-memory slab from it — the slab and the
// batcher must agree on the per-chunk ceiling or one of them is wrong.
pub(crate) use batcher::MAX_CHUNK;

use crate::error::Result;
use crate::host::ipc_client::audio::HarmonyInputs;
use crate::host::ipc_client::audio::{BridgeEvent, PluginInvalidation, ResyncClass};
use crate::host::ipc_client::PluginBridge;
use crate::host::subprocess;
use crate::protocol::{
    LoadedPlugin, MidiEventVec, Normalized, ParamAddress, ParameterChanges, PluginDescriptor,
    SampleFormat, TransportInfo,
};
use crate::util::config::BridgeConfig;
use batcher::Batcher;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tutti_core::meter::MeterMap;
use tutti_core::{SampleRate, Samples};
use tutti_plugin_types::PluginTail;

/// The typestate of a [`PluginClient`] that is loaded but not yet a graph node.
///
/// [`PluginClient::new`] returns this state; [`PluginClient::bind`] leaves it.
#[derive(Debug)]
pub struct Unbound {
    _private: (),
}

/// The typestate of a [`PluginClient`] that owns its audio path and can go into
/// a graph.
///
/// Reached through [`PluginClient::bind`]. Only `PluginClient<Bound>` is a
/// [`Node`](tutti_graph::Node).
pub struct Bound {
    /// The pipelined IPC path, sized for the chunk ceiling at bind and
    /// narrowed by `prepare`.
    io: Batcher,
    /// The payload of the chunk being filled — its MIDI, automation, harmony
    /// and note expression, and the transport at its first frame: gathered
    /// when the chunk begins, sent when it is submitted (possibly a block
    /// later; see the batcher's FIFO).
    pending: BlockPayload,
    /// The plugin's MIDI-out for this call, at this call's frames, staged
    /// while the batcher holds the audio outputs and written to the event
    /// output after it. Inline: past its capacity the rest is dropped.
    out_events: MidiEventVec,
    /// The free-running sample counter the plugin's transport carries:
    /// frames this node has rendered, monotonic across a re-prepare.
    steady: transport_source::SteadyTime,
    /// The meter the transport snapshot reads when none is installed on the
    /// controls: built once here, on the control thread, because building a
    /// `MeterMap` allocates.
    default_meter: MeterMap,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound")
            .field("chunk", &self.io.chunk())
            .finish_non_exhaustive()
    }
}

/// An out-of-process plugin as a graph node: [`Unbound`] when loaded,
/// [`Bound`] once it can go into a graph.
///
/// [`PluginClient::new`] launches the plugin-server subprocess and loads the
/// plugin. [`bind`](PluginClient::bind) allocates the audio path and returns a
/// `PluginClient<Bound>`, the only state that implements
/// [`IntoNode`](tutti_graph::IntoNode). Inserting it into a graph hands back
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
/// [`PluginHandle`](crate::host::handles::PluginHandle). The subprocess
/// exits when the node and every handle built from it have been dropped.
///
/// Not `Clone`: a node exists once, in the graph that owns it.
///
/// # Examples
///
/// ```no_run
/// use tutti_graph::Editor;
/// use tutti_plugin::handles::{PluginClient, PluginControls};
/// use tutti_types::NodeKey;
/// fn insert(client: PluginClient, editor: &mut Editor) -> PluginControls {
///     editor.insert(NodeKey(1), "plugin", client.bind())
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
    /// The chunk ceiling the slab was launched with: `MAX_CHUNK`, or the
    /// host's smaller `max_buffer_size`.
    ceiling: usize,
    /// The input slots, meter, latency, tail and sample rate — every cell
    /// shared with the handles a host keeps, so a host holding
    /// [`controls`](Self::controls) reaches the running node without it.
    controls: PluginControls,
    /// Observers for plugin-originated unsolicited events. Shared with
    /// `PluginHandle` so callers can register callbacks via the handle
    /// and still see events driven by the bridge thread.
    param_sink: ParameterChangeSink,
    refresh_sink: RefreshSink,
    invalidate_sink: InvalidateSink,
    /// Subprocess lifetime, shared with `PluginHandle::from_client`.
    process_guard: Arc<ProcessGuard>,
    /// Where this instance came from, so a fork can load another of it
    /// ([`fork_instance`](Self::fork_instance)). Never written.
    origin: Arc<fork::Origin>,
    /// On a **fork** only: whether it has failed while rendering
    /// ([`fork_health`](Self::fork_health)).
    fork_watch: Option<Arc<fork::ForkWatch>>,
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

/// Everything the host produces for one chunk, aggregated for the bridge
/// call. Host-side only — the batcher unpacks it into the (unchanged)
/// positional `bridge.submit` arguments, so the IPC wire shape is untouched.
#[derive(Default)]
pub(super) struct BlockPayload {
    pub midi: crate::protocol::MidiEventVec,
    pub params: ParameterChanges,
    pub note_expression: crate::protocol::NoteExpressionChanges,
    pub harmony: HarmonyInputs,
    pub transport: TransportInfo,
}

impl PluginClient<Unbound> {
    /// Spawns the plugin-server, loads `plugin_path` and returns the loaded
    /// plugin, [`Unbound`].
    ///
    /// Blocks while the subprocess launches and completes its handshake. The
    /// subprocess exits when the graph has dropped the node and every
    /// [`PluginHandle`](crate::host::handles::PluginHandle) built from this
    /// client has been dropped.
    ///
    /// # Errors
    ///
    /// Returns a [`BridgeError`](crate::BridgeError) if the plugin-server
    /// cannot be found or spawned, the handshake fails or times out, the
    /// plugin fails to load, or the IPC bridge cannot be set up.
    pub fn new(
        config: BridgeConfig,
        plugin_path: PathBuf,
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Self> {
        let sample_rate = sample_rate.into();
        // `.get()` at the wire: `launch` hands the rate to the subprocess.
        let server = subprocess::launch(&config, &plugin_path, sample_rate.get())?;
        // VST2 addresses its parameters by position, every other format by
        // an opaque handle: what a parameter ramp's number means to this node.
        let indexed = crate::host::discovery::format_from_path(&plugin_path)
            == Some(crate::host::discovery::PluginFormat::Vst2);
        // Guard the process before the first fallible step below: an `Err`
        // from `PluginBridge::new` would otherwise drop a bare `Child`, which
        // std neither kills nor waits.
        let mut process_guard = ProcessGuard::launched(server.process, config.clone());
        let origin = Arc::new(fork::Origin {
            config: config.clone(),
            plugin_path: plugin_path.clone(),
        });

        // Report the FULL input width (main + sidechain/aux input buses) so a
        // graph edge into port 1 lands on a real sidechain port; the batcher
        // writes each input port into the matching channel of the slab's
        // input region (bus-ordered). Outputs are indexed from 0 within their
        // own region.
        let inputs: usize = server.loaded.total_inputs();
        let outputs: usize = server.loaded.total_outputs();

        let (bridge, bridge_thread) = PluginBridge::new(
            config.socket_path.clone(),
            server.audio_buffer,
            plugin_path,
            sample_rate.get(),
        )?;

        let controls = PluginControls::new(
            server.loaded.latency_samples,
            server.loaded.tail,
            sample_rate,
            indexed,
        );
        // The chunk ceiling, matching the slab — `slab_layout_for` clamps to
        // `MAX_CHUNK` for the same reason. The raw `max_buffer_size` would
        // stage 8192 samples per channel to ship 64 and leave the two sizes
        // free to disagree.
        let ceiling = config.max_buffer_size.min(MAX_CHUNK);
        process_guard.attach(bridge_thread);
        let process_guard = Arc::new(process_guard);
        let param_sink = ParameterChangeSink::new();
        let refresh_sink = RefreshSink::new();
        let invalidate_sink = InvalidateSink::new();

        // Route unsolicited bridge events into the shared cells + sinks. The
        // bridge-thread callback must be cheap. A latency change writes the
        // latency cell directly, so `PluginControls::declared_latency` reads
        // the new figure, then fires the *invalidate* sink (latency is a
        // structural invalidation — it re-plans PDC, which a host does with
        // `Editor::set_latency`). Resync signals split by consequence into the
        // refresh (cosmetic) vs invalidate (structural) sinks via
        // `ResyncKind::classify`.
        let listener_latency = controls.latency_cell();
        let listener_tail = controls.tail_cell();
        let listener_param_sink = param_sink.clone();
        let listener_refresh_sink = refresh_sink.clone();
        let listener_invalidate_sink = invalidate_sink.clone();
        bridge.set_listener(Some(Arc::new(move |ev| match ev {
            BridgeEvent::LatencyChanged { samples } => {
                // The atomic is the terminus: `AtomicUsize` needs a
                // primitive, so the unit type stops here.
                listener_latency.store(samples.get(), Ordering::Release);
                listener_invalidate_sink.fire(PluginInvalidation::Latency { samples });
            }
            BridgeEvent::TailChanged { tail } => {
                listener_tail.store(Arc::new(tail));
                listener_invalidate_sink.fire(PluginInvalidation::Tail { tail });
            }
            BridgeEvent::ParameterChanged { index, value } => {
                if let Ok(id) = u32::try_from(index) {
                    listener_param_sink.fire(id, value);
                }
            }
            BridgeEvent::Resync(kind) => match kind.classify() {
                ResyncClass::Refresh(r) => listener_refresh_sink.fire(r),
                ResyncClass::Invalidate(i) => listener_invalidate_sink.fire(i),
            },
            // Structural and terminal. No cell is updated alongside it the
            // way latency and tail are: there is no new value to cache, and the
            // crash flag this mirrors was already set by the bridge thread
            // before it fired — see `thread::crash`.
            BridgeEvent::Crashed { cause } => {
                listener_invalidate_sink.fire(PluginInvalidation::Crashed { cause });
            }
        })));

        Ok(Self {
            bridge,
            descriptor: server.descriptor,
            loaded: server.loaded,
            format: server.format,
            inputs,
            outputs,
            ceiling,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            process_guard,
            origin,
            fork_watch: None,
            state: Unbound { _private: () },
        })
    }

    /// Allocates the audio path and returns the [`PluginClient<Bound>`] that
    /// can go into a graph.
    ///
    /// Call on the control thread; allocates. Insert the result with
    /// `Editor::insert`, which prepares it and hands back its
    /// [`PluginControls`].
    ///
    /// Install per-block sources before or after — they live on shared cells
    /// ([`controls`](Self::controls)), so an install through a handle reaches
    /// the running node.
    pub fn bind(self) -> PluginClient<Bound> {
        let io = Batcher::new(self.inputs, self.outputs, self.format, self.ceiling);
        let Self {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            ceiling,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            process_guard,
            origin,
            fork_watch,
            state: Unbound { _private: () },
        } = self;
        PluginClient {
            bridge,
            descriptor,
            loaded,
            format,
            inputs,
            outputs,
            ceiling,
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            process_guard,
            origin,
            fork_watch,
            state: Bound {
                io,
                pending: BlockPayload::default(),
                out_events: MidiEventVec::new(),
                steady: transport_source::SteadyTime::default(),
                default_meter: MeterMap::default(),
            },
        }
    }
}

impl<S> PluginClient<S> {
    pub(crate) fn param_sink(&self) -> &ParameterChangeSink {
        &self.param_sink
    }

    pub(crate) fn refresh_sink(&self) -> &RefreshSink {
        &self.refresh_sink
    }

    pub(crate) fn invalidate_sink(&self) -> &InvalidateSink {
        &self.invalidate_sink
    }

    /// Accessor for `PluginHandle::from_client` — not for end users.
    pub(crate) fn process_guard(&self) -> &Arc<ProcessGuard> {
        &self.process_guard
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
    /// `BridgeMessage::LatencyChanged` (installed by `PluginClient::new`);
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

    /// Overrides the plugin's reported tail. RT-safe.
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

    /// Queues a parameter change for the plugin. RT-safe, fire-and-forget.
    ///
    /// Main-thread parameter reads and writes are on
    /// [`PluginHandle`](crate::host::handles::PluginHandle); this lets a
    /// builder set initial values before any handle exists.
    pub fn set_parameter(&self, param_id: ParamAddress, value: Normalized) {
        let _ = self.bridge.set_parameter_rt(param_id, value);
    }

    /// Sends the host [`AutomationMode`](crate::protocol::AutomationMode) to the
    /// plugin.
    ///
    /// RT-safe, fire-and-forget; a no-op for plugins and formats without an
    /// automation-state concept.
    pub fn set_automation_state(&self, mode: crate::protocol::AutomationMode) {
        let _ = self.bridge.set_automation_state_rt(mode);
    }

    /// Sends the [`RenderMode`](crate::protocol::RenderMode) to the plugin.
    ///
    /// Queued on the same command bus as the blocks around it, so the change
    /// lands between two blocks rather than overtaking one in flight.
    ///
    /// Returns whether the command was *queued*, which is not whether the
    /// plugin honoured it: the server applies it asynchronously, and whether a
    /// given plugin acts on the mode is the load-time
    /// [`Features::RENDER_MODE`](crate::protocol::Features) capability the
    /// caller already has. `false` here means the bridge is crashed or its
    /// queue is full.
    pub fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        self.bridge.set_render_mode_rt(mode)
    }

    /// Installs the project meter, from which the plugin's transport gets its
    /// time signature and bar.
    ///
    /// The transport itself needs no install: the node reads it from each
    /// block. See [`PluginControls::set_meter`].
    pub fn set_meter(&self, meter: Arc<tutti_core::RtPublish<MeterMap>>) {
        self.controls.set_meter(meter);
    }

    /// Creates an automation node that samples one curve per parameter, to
    /// wire to this plugin node's event input.
    ///
    /// See [`PluginControls::automation`].
    pub fn automation(&self, params: impl IntoIterator<Item = TimedParam>) -> PluginAutomation {
        self.controls.automation(params)
    }

    /// Creates a [`PluginParamTarget`] for one of this plugin's parameters.
    ///
    /// The target is a [`ModTarget`](tutti_nodes::ModTarget) a modulation
    /// router accumulates into; the plugin receives its value as a per-block
    /// parameter change.
    ///
    /// The returned `Arc` is usable as BOTH a `ModTarget` (route to it) and a
    /// [`Curve`](tutti_nodes::automation::Curve) (a `TimedParam`'s, for
    /// [`automation`](Self::automation));
    /// keep the same `Arc` for both so accumulation is visible to the per-block
    /// read.
    ///
    /// `[min, max]` is the param's range (plugins are normalized `0..1`; the
    /// caller supplies it, e.g. from `ParameterInfo::to_range`).
    pub fn param_target(
        &self,
        param_id: u32,
        base: f32,
        min: f32,
        max: f32,
    ) -> std::sync::Arc<PluginParamTarget> {
        // param_id is carried by the caller into the `TimedParam` at install
        // time; the target itself only accumulates a value.
        self.controls.param_target(param_id, base, min, max)
    }

    /// Used by `PluginHandle`.
    pub(crate) fn bridge(&self) -> Arc<PluginBridge> {
        Arc::clone(&self.bridge)
    }
}

/// A hosted plugin implements the same `ModParams` trait as a native node.
/// A plugin speaks the opaque-id vocabulary, so
/// it answers on [`ParamAddr::Id`](tutti_core::ParamAddr::Id) (its numeric param
/// id) and returns `None` for a native [`Unit`](tutti_core::ParamAddr::Unit)
/// param it does not have.
///
/// The returned target is a [`PluginParamTarget`] — a keyed accumulator whose
/// value the plugin receives over the per-block `ParameterChanges` path (vs a
/// native node's target, which mirrors into an atomic). The caller hands it
/// to the plugin's automation node (as a `TimedParam`, through
/// `PluginControls::automation`) after routing.
impl<S: Send + Sync> tutti_nodes::ModParams for PluginClient<S> {
    fn mod_target(
        &self,
        param: tutti_core::ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn tutti_nodes::ModTarget>> {
        match param {
            tutti_core::ParamAddr::Id(param_id) => {
                Some(self.param_target(param_id, base, min, max))
            }
            // A native `UnitParam` is not a hosted plugin's vocabulary.
            tutti_core::ParamAddr::Unit(_) => None,
        }
    }
}
