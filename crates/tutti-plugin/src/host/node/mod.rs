//! The graph node for an out-of-process plugin ([`PluginClient`]).
//!
//! # Loaded, then bound
//!
//! A client is born [`Unbound`] ([`PluginClient::new`]): a loaded plugin with
//! its control surface, not yet a node. [`bind`](PluginClient::bind) turns it
//! into a [`PluginClient<Bound>`], which owns the audio path (the IPC
//! [`Batcher`] and its scratch) and is the only state that
//! goes into a graph ([`tutti_graph::IntoNode`]), as the `Node` that
//! `graph_node` implements. Inserting it hands back its [`PluginControls`]:
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
//!   [`Batcher`]: whole chunks of one device callback (the
//!   graph's `MaxBlock` when the host does not say), so the plugin's latency
//!   is its own plus one callback. The `f64` wire conversion stays inside it.
//! - **The transport**, from each chunk's frame of the block's `Env`
//!   (`transport_source`), plus the meter installed on its controls.
//! - **MIDI, parameter ramps, chords and scales** on its event input, sent
//!   with the chunk their frames fall in. A clip installed on the MIDI port is
//!   still polled out of band (see the `shape` docs in `graph_node`).
//!
//! Subprocess lifetime is [`ProcessGuard`], held behind `Arc` here and in
//! [`PluginHandle`], so the subprocess dies when the
//! LAST Arc drops. Main-thread editor / state / parameter access is on
//! [`PluginHandle`].

mod capability_view;
mod fork;
mod graph_node;

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
#[cfg(test)]
mod tests;

pub use capability_view::TransportView;
pub(crate) use capability_view::is_declined;

use crate::host::subprocess_backend::SubprocessBackend;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_plugin_host::{
    InvalidateSink, ParamDelivery, ParameterChangeSink, PluginControls, PluginHandle, RefreshSink,
    SegmentPoints, TransportSource, TransportWalk,
};
use tutti_plugin_ipc::BridgeConfig;
use tutti_plugin_ipc::BridgeEvent;
use tutti_plugin_ipc::PluginBridge;
use tutti_plugin_ipc::{Batcher, BlockPayload, ProcessGuard};
use tutti_plugin_types::MidiIntake;
use tutti_plugin_types::PluginTail;
use tutti_plugin_types::{BridgeResult, MAX_CHUNK};
use tutti_plugin_types::{
    LoadedPlugin, MidiEventVec, Normalized, ParamId, PluginDescriptor, SampleFormat,
};
use tutti_plugin_types::{PluginInvalidation, ResyncClass};
use tutti_types::{SampleRate, Samples};

/// The typestate of a [`PluginClient`] that is loaded but not yet a graph node.
///
/// [`PluginClient::new`] returns this state; [`PluginClient::bind`] leaves it.
#[derive(Debug)]
pub struct Unbound {
    /// The slab's input writer, until [`PluginClient::bind`] gives it to the
    /// node's batcher, the one thing that writes a chunk.
    slab: tutti_plugin_ipc::SlabWriter,
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
    /// The transport changes inside the chunk being filled: where the walk
    /// is, between calls (`transport_walk`).
    walk: TransportWalk,
    /// The meter cell and the clocks the plugin's transport carries
    /// (`TransportSource`): the steady counter, project time and the host
    /// clock, `None` on an offline fork.
    transport: TransportSource,
    /// The project's tempo map, read through the reader `bind` was given and
    /// lent once per block: the tempo and slope every chunk's transport
    /// reports. A fork clones it on the control side (its own registration on
    /// the same cell).
    tempo: tutti_transport::TempoReader,
    /// The node's cursor into the map, kept across blocks so the lookups
    /// start where the last one ended.
    tempo_cursor: tutti_tempo::Cursor,
    /// The curve segments in flight on the event input, sent as parameter
    /// points until each lands. Its slots are allocated by `prepare`;
    /// nothing is allocated on the audio thread.
    segments: SegmentPoints,
    /// The chunk's parameter changes: a slot per parameter of the plugin,
    /// sized at bind, sending changes only (`param_delivery`).
    params: ParamDelivery,
    /// Takes the event input's MIDI into each chunk's capped list, carrying
    /// across chunks which SysEx lost a packet there (`midi_intake`).
    intake: MidiIntake,
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
    /// Subprocess lifetime, shared with `PluginClient::handle`.
    process_guard: Arc<ProcessGuard>,
    /// Where this instance came from, so a fork can load another of it
    /// ([`fork_instance`](Self::fork_instance)). Never written.
    origin: Arc<fork::Origin>,
    /// On a **fork** only: whether it has failed while rendering
    /// ([`fork_health`](Self::fork_health)).
    fork_watch: Option<Arc<fork::ForkWatch>>,
    /// The plugin's parameter addresses, read once at load: what
    /// [`bind`](PluginClient::bind) sizes the node's parameter delivery by.
    /// Empty when the plugin-server could not list them; the delivery then
    /// learns addresses into its spare slots.
    params: Arc<[ParamId]>,
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
    /// Returns a [`BridgeError`](tutti_plugin_types::BridgeError) if the plugin-server
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
        let sample_rate = sample_rate.into();
        let server = tutti_plugin_ipc::launch(&config, &plugin_path, sample_rate, state)?;
        let state_restore = server.state_restore.clone();
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
            std::sync::Arc::clone(server.slab.slab()),
            plugin_path,
            crate::format::gui::load_gui_instance,
            sample_rate,
        )?;

        let controls = PluginControls::new(
            server.loaded.latency_samples,
            server.loaded.tail,
            sample_rate,
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
        // per-parameter delivery is sized from it at bind. A `prepare` may
        // not make this round trip; a load already has.
        let params: Arc<[ParamId]> = match bridge.parameters() {
            Some(list) => list.into_iter().map(|p| p.id).collect(),
            None => {
                tracing::warn!(
                    "plugin-server listed no parameters at load; parameter \
                     delivery falls back to its spare slots"
                );
                Arc::from([])
            }
        };

        let client = Self {
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
            params,
            state: Unbound { slab: server.slab },
        };
        Ok((client, state_restore))
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
    ///
    /// `tempo` is the reader of the project's tempo map the node lends once
    /// per block to tell the plugin the tempo (and CLAP its slope):
    /// `Session` hands every plugin it inserts one of its own
    /// (`TransportSettings::tempo_reader`); a host with no session builds one
    /// over a fixed map ([`TempoReader::fixed`](tutti_transport::TempoReader::fixed)).
    /// A fork of the node clones it.
    #[must_use]
    pub fn bind(self, tempo: tutti_transport::TempoReader) -> PluginClient<Bound> {
        let chunks = self.controls.chunk_counters();
        let slab = Arc::clone(self.bridge.audio_buffer());
        chunks.attach(move |seq| slab.has_output(seq));
        self.controls
            .delivery_counters()
            .attach_bridge_midi(self.bridge.dropped_midi_cell());
        let Unbound { slab } = self.state;
        let io = Batcher::new(self.inputs, self.outputs, self.format, self.ceiling, slab)
            .with_counters(chunks);
        // A slot per parameter: every parameter can move in one chunk. The
        // chunk's point room, here and in the bridge's buffers, is sized in
        // `prepare`, once the chunk length is known.
        let params = ParamDelivery::new(
            self.params.iter().copied(),
            self.controls.delivery_counters(),
        );
        let meter = self.controls.meter().reader();
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
            params: addresses,
            state: _,
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
            params: addresses,
            state: Bound {
                io,
                pending: BlockPayload::default(),
                out_events: MidiEventVec::new(),
                walk: TransportWalk::default(),
                transport: TransportSource::new(meter),
                tempo,
                tempo_cursor: tutti_tempo::Cursor::default(),
                segments: SegmentPoints::default(),
                params,
                intake: MidiIntake::default(),
            },
        }
    }
}

impl<S> PluginClient<S> {
    /// The main-thread control handle for this plugin: editor, parameters,
    /// state.
    ///
    /// Call this before moving the client into a graph. The handle shares the
    /// subprocess's lifetime with the client, and the subprocess backend
    /// honors every capability, including the editor.
    pub fn handle(&self) -> PluginHandle {
        SubprocessBackend::new(self.bridge(), Arc::clone(self.process_guard())).into_handle(
            self.descriptor().clone(),
            self.loaded().clone(),
            (
                self.param_sink().clone(),
                self.refresh_sink().clone(),
                self.invalidate_sink().clone(),
            ),
            self.process_guard().pid(),
        )
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

    /// The subprocess's lifetime guard, shared with [`handle`](Self::handle).
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
    /// queue is full.
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        self.bridge.set_render_mode_rt(mode)
    }

    /// Used by `PluginHandle`.
    pub(crate) fn bridge(&self) -> Arc<PluginBridge> {
        Arc::clone(&self.bridge)
    }
}
