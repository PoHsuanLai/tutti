//! `PluginClient` — fundsp-graph-facing audio node for an out-of-process
//! plugin.
//!
//! `AudioUnit<F32> + AudioUnit<F64>` impls (and the inherent `midi_unit_id`)
//! live in [`audio_unit`]. Subprocess lifetime guard is [`ProcessGuard`] —
//! held behind `Arc` here and in [`crate::host::handles::PluginHandle`], so the
//! subprocess dies when the LAST Arc drops.
//!
//! Audio batching lives in [`batcher::Batcher`] (sample-by-sample
//! `tick()` ↔ block-oriented IPC). MIDI queue + registry polling lives
//! in [`midi::Midi`]. Main-thread editor / state / parameter access is
//! on [`crate::host::handles::PluginHandle`]; `PluginClient` exposes only what the
//! audio path needs.

mod audio_unit;
mod batcher;
mod capability_view;
mod controls;
mod fork;
mod harmony_source;
// `input_slot` / `transport_source` are `pub(crate)` rather than private: the
// in-process VST2 node (`crate::format::vst2_in_process`) is a peer host, not a
// subprocess client, and reuses the same gated per-block transport plumbing
// rather than hand-rolling a second copy of the shared-cell contract.
pub(crate) mod input_slot;
mod note_expression_source;
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

// The shared node primitives (MIDI inbox, change sinks, routing helper) live
// in `crate::util::node`; re-exported here so the existing
// `crate::host::node::{Midi, ...}` paths (used by `crate::backend`) keep
// resolving.
pub use crate::util::node::{route_with_latency, Midi, ParameterChangeSink};
pub(crate) use crate::util::node::{InvalidateSink, RefreshSink};
pub(crate) use capability_view::is_declined;
pub use capability_view::{
    HarmonyView, MidiInView, MidiOutView, NoteExpressionView, TransportView,
};
pub use controls::PluginControls;
pub use harmony_source::{HarmonySource, TimedChord, TimedScale};
pub use note_expression_source::NoteExpressionSource;
pub use param_automation_source::{
    LfoCurve, LfoOffset, OffsetCurve, ParamAutomationSource, PluginParamTarget, TimedParam,
};
pub(crate) use process::ProcessGuard;
// The largest block that can cross the process edge. Re-exported because
// `subprocess::launch` sizes the shared-memory slab from it — the slab and the
// batcher must agree on the per-block ceiling or one of them is wrong.
pub(crate) use batcher::BATCH_SIZE;

use crate::error::Result;
use crate::host::ipc_client::audio::HarmonyInputs;
use crate::host::ipc_client::audio::{BridgeEvent, PluginInvalidation, ResyncClass};
use crate::host::ipc_client::PluginBridge;
use crate::host::node::input_slot::BlockCtx;
use crate::host::subprocess;
use crate::protocol::{
    LoadedPlugin, Normalized, ParamAddress, ParameterChanges, PluginDescriptor, SampleFormat,
    TransportInfo,
};
use crate::util::config::BridgeConfig;
use batcher::{Batcher, PIPELINE_LATENCY_FRAMES};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tutti_core::{SampleRate, Samples};
use tutti_plugin_types::PluginTail;

/// Cheap to clone: clones share `bridge`, `process_guard` and every cell in
/// [`controls`](Self::controls) (all Arc) but get independent `io` and `midi`
/// scratch (fundsp clones nodes on graph commit).
#[derive(Clone)]
pub struct PluginClient {
    bridge: Arc<PluginBridge>,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
    format: SampleFormat,
    /// The input slots, latency, tail and sample rate — every cell shared
    /// across clones, so runtime updates are seen by whichever clone fundsp is
    /// currently processing, and a host holding [`controls`](Self::controls)
    /// reaches the running node without it.
    controls: PluginControls,
    /// Observers for plugin-originated unsolicited events. Shared with
    /// `PluginHandle` so callers can register callbacks via the handle
    /// and still see events driven by the bridge thread.
    param_sink: ParameterChangeSink,
    refresh_sink: RefreshSink,
    invalidate_sink: InvalidateSink,
    /// Subprocess lifetime, shared with `PluginHandle::from_client`.
    process_guard: Arc<ProcessGuard>,
    io: Batcher,
    midi: Midi,
    /// Per-block scratch the subprocess plugin's MIDI-out is drained into, then
    /// re-injected into routing via [`Midi::emit`]. Cleared at the start of each
    /// `process`; steady-state capacity makes the drain alloc-free.
    midi_out: crate::protocol::MidiEventVec,
    /// Where this instance came from, so a fork can load another of it
    /// ([`fork_instance`](Self::fork_instance)). Shared across clones and
    /// never written.
    origin: Arc<fork::Origin>,
    /// On a **fork** only: whether it has failed while rendering
    /// ([`fork_health`](Self::fork_health)).
    fork_watch: Option<Arc<fork::ForkWatch>>,
}

/// Everything the host produces for one process block, aggregated for the bridge
/// call. Host-side only — the batcher unpacks it into the (unchanged) positional
/// `bridge.process` arguments, so the IPC wire shape is untouched.
#[derive(Default)]
pub(super) struct BlockPayload {
    pub midi: crate::protocol::MidiEventVec,
    pub params: ParameterChanges,
    pub note_expression: crate::protocol::NoteExpressionChanges,
    pub harmony: HarmonyInputs,
    pub transport: TransportInfo,
}

// Sibling-module access (audio_unit.rs). Field access stays private.
impl PluginClient {
    pub(super) fn io_mut(&mut self) -> &mut Batcher {
        &mut self.io
    }

    pub(super) fn io_ref(&self) -> &Batcher {
        &self.io
    }

    pub(super) fn midi_ref(&self) -> &Midi {
        &self.midi
    }

    /// Install the outbound routing target so this subprocess plugin's MIDI-out
    /// re-enters the graph. See [`Midi::set_out`]. Off-RT; call at wiring time.
    pub fn set_midi_out(&self, sink: Arc<tutti_midi_runtime::MidiOutSink>) {
        self.midi.set_out(sink);
    }

    /// Drop the outbound routing target; subsequent blocks discard MIDI-out.
    pub fn clear_midi_out(&self) {
        self.midi.clear_out();
    }

    /// Assemble this block's [`BlockPayload`]: MIDI (from the receiver-fallback
    /// [`Midi`]) plus each gated [`InputSlot`](input_slot::InputSlot) (harmony / params / transport /
    /// note-expression). Every send/gate decision lives in [`InputSlot::drain`]
    /// keyed on the plugin's [`Features`](crate::protocol::Features) — never on the plugin's format. The
    /// note-expression slot's producer currently emits nothing (reader deferred),
    /// so it drains empty until a note-expression lane reader is wired in.
    pub(super) fn build_block_payload(&mut self, block_size: usize) -> BlockPayload {
        let ctx = BlockCtx { block_size };
        let features = self.loaded.features;
        BlockPayload {
            midi: self
                .midi
                .drain_for_process(block_size, self.controls.sample_rate())
                .clone(),
            params: self.controls.inputs.params.drain(ctx, features).clone(),
            harmony: self.controls.inputs.harmony.drain(ctx, features).clone(),
            transport: *self.controls.inputs.transport.drain(ctx, features),
            note_expression: self
                .controls
                .inputs
                .note_expression
                .drain(ctx, features)
                .clone(),
        }
    }

    pub(super) fn bridge_ref(&self) -> &Arc<PluginBridge> {
        &self.bridge
    }

    /// Hand the plugin's MIDI-out to the post-block phase — but only if the
    /// plugin declared [`Features::MIDI_OUT`](crate::protocol::Features::MIDI_OUT). Gating the *emit* on the
    /// self-reported capability mirrors how the per-block input feeds gate their
    /// sends on their `Features` bit: a plugin that never advertised MIDI output
    /// has its emission dropped rather than silently re-injected. (Without the
    /// gate, `emit` fired whenever an out-target was installed, regardless of
    /// the bit.)
    ///
    /// `emit` only *collects* now; the fan-out happens once the graph has
    /// rendered. See [`Midi::emit`](crate::util::node::Midi::emit).
    #[inline]
    fn emit_midi_out_if_declared(&mut self) {
        if self
            .loaded
            .features
            .contains(crate::protocol::Features::MIDI_OUT)
        {
            self.shift_midi_out_into_this_block();
            // The count is deliberately dropped here rather than propagated:
            // this is a `process` path with no caller that could act on it. The
            // sink records the overflow (`MidiOutSink::overflowed`) so the loss
            // is observable off-RT instead of silent.
            let _ = self.midi.emit(&self.midi_out);
        }
    }

    /// Re-base the plugin's MIDI-out onto the block it is actually collected in.
    ///
    /// The reply drained here belongs to the block submitted *last* time, so each
    /// `frame_offset` counts from that earlier block's start. Relative to now that
    /// is `offset - PIPELINE_LATENCY_SAMPLES`, always negative because an offset
    /// cannot exceed its own block's length — so every such event is already due
    /// and clamps to frame 0. Left unshifted they would land a full block *early*,
    /// audible as an early-triggering sequencer.
    ///
    /// Saturating rather than dropping: the event is late regardless, frame 0 is
    /// the closest representable position, and dropping would silently lose an
    /// arpeggiator's notes.
    ///
    /// # This shift and the post-block phase do not double-count
    ///
    /// Worth stating, because "shift by a block, then deliver a block later"
    /// reads like it should. The two act on different things:
    ///
    /// - The shift fixes an event's **position within a block**
    ///   (`frame_offset`), correcting for the reply belonging to an earlier
    ///   submission. Clamping to 0 means "due at the start of whatever block
    ///   delivers this", not "due one block from now".
    /// - The phase fixes **which block delivers it**, uniformly, for every
    ///   emitter.
    ///
    /// So the offsets stay meaningful and the delivery block is now declared
    /// rather than emergent
    /// ([`MIDI_OUT_LATENCY_BLOCKS`](tutti_midi_runtime::MIDI_OUT_LATENCY_BLOCKS)).
    /// Removing this shift would *not* cancel the phase's delay — it would
    /// restore the early-triggering-sequencer bug on top of it.
    #[inline]
    fn shift_midi_out_into_this_block(&mut self) {
        let shift = PIPELINE_LATENCY_FRAMES.get() as u32;
        for ev in self.midi_out.iter_mut() {
            ev.frame_offset = ev.frame_offset.saturating_sub(shift);
        }
    }

    /// Flush the tick batch through the bridge, then re-inject the plugin's
    /// MIDI-out into routing. Split-borrows `io` / `bridge` / `midi_out` / `midi`
    /// so the sink and the emit sidestep an aliasing `&mut self`.
    pub(in crate::host::node) fn flush_batch<T: batcher::Scalar>(&mut self, payload: BlockPayload) {
        let bridge = self.bridge.clone();
        self.io.flush::<T>(&bridge, payload, &mut self.midi_out);
        self.emit_midi_out_if_declared();
    }

    /// Block-mode counterpart of [`Self::flush_batch`].
    pub(in crate::host::node) fn process_block<T: batcher::Scalar>(
        &mut self,
        size: usize,
        input: &tutti_core::BufferRef<'_, T::Marker>,
        output: &mut tutti_core::BufferMut<'_, T::Marker>,
        payload: BlockPayload,
    ) {
        let bridge = self.bridge.clone();
        self.io
            .process::<T>(&bridge, size, input, output, payload, &mut self.midi_out);
        self.emit_midi_out_if_declared();
    }

    /// Update the sample rate stamped onto every installed per-block source.
    /// Called from
    /// the `AudioUnit::set_sample_rate` impls. Reaches the running box because
    /// the source's rate is a shared atomic; a no-op when no source is installed
    /// (it's installed later with the correct rate by the host).
    pub(super) fn restamp_source_rates(&mut self, sample_rate: SampleRate) {
        self.controls.restamp(sample_rate);
        // Not the MIDI clip: it holds no rate, and is handed this node's on
        // every poll (`build_block_payload` reads `controls.sample_rate()`,
        // which `restamp` just moved), so a fork launched at the live rate and
        // prepared at an export's places its notes at the export's.
    }
}

impl PluginClient {
    /// Spawns the plugin-server, loads `plugin_path`, and returns the
    /// audio client. The subprocess lifetime is held internally by an
    /// `Arc<ProcessGuard>` shared with any `PluginHandle` built from
    /// this client; the subprocess dies when both the fundsp graph has
    /// released the AudioUnit and all handles have dropped.
    pub fn new(
        config: BridgeConfig,
        plugin_path: PathBuf,
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Self> {
        let sample_rate = sample_rate.into();
        // `.get()` at the wire: `launch` hands the rate to the subprocess.
        let server = subprocess::launch(&config, &plugin_path, sample_rate.get())?;
        // Guard the process before the first fallible step below: an `Err`
        // from `PluginBridge::new` would otherwise drop a bare `Child`, which
        // std neither kills nor waits.
        let mut process_guard = ProcessGuard::launched(server.process, config.clone());
        let origin = Arc::new(fork::Origin {
            config: config.clone(),
            plugin_path: plugin_path.clone(),
        });

        // Report the FULL input width (main + sidechain/aux input buses) so a
        // fundsp `connect(src, 0, target, 1)` lands on a real sidechain port;
        // the batcher writes each input port into the matching channel of the
        // slab's input region (bus-ordered). Outputs are indexed from 0 within
        // their own region — there is no base to carry any more, because the two
        // directions can no longer share one.
        let inputs: usize = server.loaded.total_inputs();
        let outputs: usize = server.loaded.total_outputs();

        let (bridge, bridge_thread) = PluginBridge::new(
            config.socket_path.clone(),
            server.audio_buffer,
            plugin_path,
            sample_rate.get(),
        )?;

        // `.get()` because the cell is an `AtomicUsize` — an atomic needs a
        // primitive, so the unit type stops here rather than at a call site.
        let controls = PluginControls::new(
            server.loaded.latency_samples,
            server.loaded.tail,
            sample_rate,
        );
        // Sized to what can actually cross the boundary, matching the slab —
        // `slab_layout_for` clamps to `BATCH_SIZE` for the same reason (fundsp
        // never hands a node more than one block). Passing the raw
        // `max_buffer_size` here meant the batcher allocated 8192 samples per
        // channel to stage 64, and left the two sizes free to disagree.
        let max_buffer_size = config.max_buffer_size.min(BATCH_SIZE);
        process_guard.attach(bridge_thread);
        let process_guard = Arc::new(process_guard);
        let param_sink = ParameterChangeSink::new();
        let refresh_sink = RefreshSink::new();
        let invalidate_sink = InvalidateSink::new();

        // Route unsolicited bridge events into the shared atomic + sinks. The
        // bridge-thread callback must be cheap. A latency change writes the
        // latency atomic directly so `AudioUnit::latency()` sees the new value on
        // the next audio read, then fires the *invalidate* sink (latency is a
        // structural invalidation — it re-plans PDC). Resync signals split by
        // consequence into the refresh (cosmetic) vs invalidate (structural)
        // sinks via `ResyncKind::classify`.
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
            // Structural and terminal. No atomic is updated alongside it the
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
            // No transport source yet — the host installs one via
            // `set_transport_source` right after load; it's stamped with the
            // controls' shared `sample_rate` (updated live on device changes).
            controls,
            param_sink,
            refresh_sink,
            invalidate_sink,
            process_guard,
            io: Batcher::new(inputs, outputs, server.format, max_buffer_size),
            midi: Midi::new(),
            midi_out: crate::protocol::MidiEventVec::new(),
            origin,
            fork_watch: None,
        })
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

    /// Accessor for `PluginHandle::from_client` — not for end users.
    pub(crate) fn process_guard(&self) -> &Arc<ProcessGuard> {
        &self.process_guard
    }

    /// The plugin's reported latency, in **frames**.
    ///
    /// RT-safe: one atomic load, no allocation.
    pub fn latency(&self) -> Samples {
        self.controls.latency()
    }

    /// Runtime latency update. RT-safe.
    ///
    /// Normally driven by the bridge thread when the plugin-server emits
    /// `BridgeMessage::LatencyChanged` (installed by `PluginClient::new`);
    /// exposed publicly so callers can also force a value. Note: updating
    /// what `AudioUnit::latency()` reports does **not** re-run PDC on its
    /// own — a graph edit (`Net::commit()`) is required. Register a
    /// callback via `PluginHandle::on_invalidate` (latency arrives as
    /// `PluginInvalidation::Latency`) to get notified.
    pub fn set_latency(&self, samples: impl Into<Samples>) {
        self.controls.set_latency(samples);
    }

    /// What the plugin currently reports for its tail.
    ///
    /// Starts as the value read at load and tracks runtime changes for formats
    /// that signal them (CLAP).
    pub fn tail(&self) -> PluginTail {
        self.controls.tail()
    }

    /// Runtime tail update. RT-safe.
    ///
    /// Normally driven by the bridge thread when the plugin-server emits
    /// `BridgeMessage::TailChanged`; exposed publicly so callers can also force
    /// a value. Like `set_latency`, this changes what the node reports without
    /// re-running anything — an offline render reads the tail when it sizes
    /// itself, so a bounce already in flight keeps the length it started with.
    pub fn set_tail(&self, tail: PluginTail) {
        self.controls.set_tail(tail);
    }

    /// A handle on this node's host-side controls — its input slots, latency,
    /// tail and sample rate — that stays valid after the node moves into a
    /// graph.
    ///
    /// Every cell in it is shared with this node and with every clone of it, so
    /// a host takes this **before** inserting the node and drives the running
    /// plugin through it from then on, instead of reaching back into the graph
    /// for the node. See [`PluginControls`].
    pub fn controls(&self) -> PluginControls {
        self.controls.clone()
    }

    /// Catalog identity (id, name, vendor, version, native class, editor).
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Engine-wiring data from load (per-bus channel widths, latency, f64).
    pub fn loaded(&self) -> &LoadedPlugin {
        &self.loaded
    }

    /// The sample format negotiated with the plugin at load.
    pub fn format(&self) -> SampleFormat {
        self.format
    }

    /// When true, all audio processing produces silence.
    pub fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }

    /// RT-safe, fire-and-forget. Main-thread parameter reads/writes live
    /// on [`crate::host::handles::PluginHandle`]; this method exists because registry
    /// builders push initial parameter values through the `PluginClient`
    /// before any `PluginHandle` has been constructed.
    pub fn set_parameter(&self, param_id: ParamAddress, value: Normalized) {
        let _ = self.bridge.set_parameter_rt(param_id, value);
    }

    /// Push the host [`AutomationMode`](crate::protocol::AutomationMode) to the
    /// plugin. RT-safe, fire-and-forget; a no-op for plugins / formats without an
    /// automation-state concept. The format-neutral mode is encoded onto the
    /// format's own ABI at its FFI edge (server-side / GUI-side), not here.
    pub fn set_automation_state(&self, mode: crate::protocol::AutomationMode) {
        let _ = self.bridge.set_automation_state_rt(mode);
    }

    /// Push the [`RenderMode`](crate::protocol::RenderMode) to the plugin.
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

    /// Producer handle for this plugin's MIDI inbox. Route live MIDI to the
    /// plugin by pushing through this sender (or by inserting it into a
    /// [`tutti_midi_runtime::MidiBus`]); clip playback uses [`Self::set_midi_source`].
    pub fn midi_sender(&self) -> tutti_midi_runtime::MidiSender {
        self.midi.sender()
    }

    /// This plugin's MIDI input endpoint — address, mailbox and source-install
    /// slot together.
    ///
    /// A hosted plugin's inbox *is* an ordinary [`MidiInPort`], the same type a
    /// built-in synth exposes, so a host that resolves MIDI targets by asking a
    /// node for its port can treat plugins and synths identically instead of
    /// carrying a second, plugin-shaped path.
    ///
    /// Prefer this over [`midi_sender`](Self::midi_sender) when the caller might
    /// later want to install a source or read the unit id: handing back the
    /// whole port avoids a second downcast, and caching half of it is how a
    /// stale unit id gets stored (a `crossfade` replaces the unit while keeping
    /// its `NodeId`, and with it the port's id).
    ///
    /// [`MidiInPort`]: tutti_midi_runtime::MidiInPort
    pub fn midi_port(&self) -> &tutti_midi_runtime::MidiInPort {
        self.midi.port()
    }

    /// Install a [`tutti_midi_types::MidiUnitIn`] override (typically
    /// [`tutti_midi_runtime::MidiClipSource`] from a track's MIDI
    /// clips) that the plugin polls per block instead of its live
    /// `MidiReceiver`. Mirrors `PolySynth::set_midi_source` so
    /// MIDI clips drive plugin synths the same way they drive
    /// built-in synths.
    ///
    /// The source is held in an `Arc`, so the same instance survives
    /// the unit-clone fundsp performs on each `commit()`.
    pub fn set_midi_source(&mut self, source: std::sync::Arc<dyn tutti_midi_types::MidiUnitIn>) {
        self.midi.set_source(source);
    }

    /// Install per-block chord/scale context (VST3 `kChordEvent` /
    /// `kScaleEvent`) from a track's chord/scale lanes.
    ///
    /// Takes the lanes and the timeline, and builds the source here — the same
    /// shape as [`set_transport_source`](Self::set_transport_source), and for
    /// the same reason: the sample rate the source needs is the node's own, so
    /// a caller passing one could only ever agree with it or be wrong. The
    /// source is held in an `Arc` so it survives fundsp's graph-commit clones,
    /// and its rate is re-stamped by `restamp_source_rates`
    /// on a device change.
    pub fn set_harmony_source(
        &mut self,
        chords: impl IntoIterator<Item = TimedChord>,
        scales: impl IntoIterator<Item = TimedScale>,
        transport: impl tutti_core::transport::Timeline + 'static,
    ) {
        self.controls.set_harmony_source(chords, scales, transport);
    }

    /// Drop a previously-installed harmony source. Subsequent blocks feed the
    /// plugin empty chord/scale context.
    pub fn clear_harmony_source(&mut self) {
        self.controls.clear_harmony_source();
    }

    /// Install a [`NoteExpressionSource`] that supplies per-block note-expression
    /// (VST3 `kNoteExpressionValueEvent`) from a track's expression lanes. Mirrors
    /// [`set_harmony_source`](Self::set_harmony_source). NOTE: the producer's
    /// reader is deferred (no expression-lane storage yet), so an installed source
    /// currently drains empty — the rail exists so the data source can be dropped
    /// in without touching the plugin-node wiring.
    pub fn set_note_expression_source(&mut self, source: std::sync::Arc<NoteExpressionSource>) {
        self.controls.set_note_expression_source(source);
    }

    /// Drop a previously-installed note-expression source. Subsequent blocks feed
    /// the plugin empty note-expression.
    pub fn clear_note_expression_source(&mut self) {
        self.controls.clear_note_expression_source();
    }

    /// Install a transport reader so the plugin receives a live per-block
    /// [`TransportInfo`] (tempo, playhead, meter, bar, loop). Wrapped internally
    /// in a transport source stamped with the current sample rate (updated live
    /// on device changes). The snapshot is only sent to plugins advertising
    /// [`Features::TRANSPORT`](crate::protocol::Features::TRANSPORT); others always get a default.
    ///
    /// `meter` is a separate handle rather than something read off the transport:
    /// meter is a layer over the timeline, not transport state. Passing the same
    /// `Arc<ArcSwap<..>>` the host publishes elsewhere means a meter edit reaches
    /// running plugins without re-installing anything.
    pub fn set_transport_source(
        &mut self,
        reader: tutti_core::transport::Transport,
        meter: Arc<tutti_core::RtPublish<tutti_core::meter::MeterMap>>,
    ) {
        self.controls.set_transport_source(reader, meter);
    }

    /// Drop a previously-installed transport reader; subsequent blocks feed the
    /// plugin a default (stopped) transport snapshot.
    pub fn clear_transport_source(&mut self) {
        self.controls.clear_transport_source();
    }

    /// Install sample-accurate per-block [`ParameterChanges`] for the automated
    /// parameters — one curve per parameter id.
    ///
    /// The *only* automation path for hosted-plugin parameters; the frame-rate
    /// `set_parameter` route is never wired for them.
    ///
    /// Takes the curves and the transport and builds the source here, matching
    /// [`set_transport_source`](Self::set_transport_source) and
    /// [`set_harmony_source`](Self::set_harmony_source). The rate the source
    /// divides by is the node's own, so a caller passing one could only agree
    /// with it or be wrong. Held in an `Arc` so it survives fundsp's
    /// graph-commit clones, and re-stamped by
    /// `restamp_source_rates` on a device change.
    ///
    /// `transport` is a [`TransportState`](tutti_core::transport::TransportState),
    /// not a bare `Timeline` like harmony's: `refill` reads `loop_range()` to wrap
    /// the beat inside the active cycle, and looping lives on the live
    /// supertrait. An offline render never drives this source.
    pub fn set_param_automation_source(
        &mut self,
        params: impl IntoIterator<Item = TimedParam>,
        transport: impl tutti_core::transport::TransportState + 'static,
    ) {
        self.controls.set_param_automation_source(params, transport);
    }

    /// Drop a previously-installed parameter-automation source; subsequent
    /// blocks feed the plugin empty [`ParameterChanges`] (it keeps its current
    /// parameter values).
    pub fn clear_param_automation_source(&mut self) {
        self.controls.clear_param_automation_source();
    }

    /// Build a [`PluginParamTarget`] for one of this plugin's params — a
    /// [`ModTarget`](tutti_nodes::ModTarget) a modulation router accumulates
    /// into, whose value the plugin receives over the per-block
    /// [`ParameterChanges`] path.
    ///
    /// The returned `Arc` is usable as BOTH a `ModTarget` (route to it) and a
    /// [`Curve`](tutti_nodes::automation::Curve) (install it in a `TimedParam`
    /// via [`set_param_automation_source`](Self::set_param_automation_source));
    /// keep the same `Arc` for both so accumulation is visible to the per-block
    /// read. This mirrors how a native node's `ModParams::mod_target` returns an
    /// `AtomicTarget` — see [`impl ModParams for PluginClient`].
    ///
    /// `[min, max]` is the param's range (plugins are normalized `0..1`; the
    /// caller supplies it, e.g. from `ParameterInfo::to_range`).
    pub fn param_target(
        &self,
        _param_id: u32,
        base: f32,
        min: f32,
        max: f32,
    ) -> std::sync::Arc<PluginParamTarget> {
        // param_id is carried by the caller into the `TimedParam` at install
        // time; the target itself only accumulates a value.
        self.controls.param_target(_param_id, base, min, max)
    }

    /// Drop a previously-installed source override; subsequent ticks
    /// poll the live `MidiReceiver` again.
    pub fn clear_midi_source(&mut self) {
        self.midi.clear_source();
    }

    /// Used by `PluginHandle`.
    pub(crate) fn bridge(&self) -> Arc<PluginBridge> {
        Arc::clone(&self.bridge)
    }
}

/// A hosted plugin implements the **same** `ModParams` trait as a native node —
/// the whole point of `ParamAddr`. A plugin speaks the opaque-id vocabulary, so
/// it answers on [`ParamAddr::Id`](tutti_core::ParamAddr::Id) (its numeric param
/// id) and returns `None` for a native [`Unit`](tutti_core::ParamAddr::Unit)
/// param it does not have.
///
/// The returned target is a [`PluginParamTarget`] — a keyed accumulator whose
/// value the plugin receives over the per-block `ParameterChanges` path (vs a
/// native node's target, which mirrors into an atomic). The caller installs it
/// (as a `TimedParam` via `set_param_automation_source`) after routing.
impl tutti_nodes::ModParams for PluginClient {
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
