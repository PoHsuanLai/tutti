//! `PluginClient` — fundsp-graph-facing audio node for an out-of-process
//! plugin.
//!
//! `AudioUnit<F32> + AudioUnit<F64>` and `MidiTarget` impls live in
//! [`audio_unit`]. Subprocess lifetime guard is [`ProcessGuard`] —
//! held behind `Arc` here and in [`crate::handles::PluginHandle`], so the
//! subprocess dies when the LAST Arc drops.
//!
//! Audio batching lives in [`batcher::Batcher`] (sample-by-sample
//! `tick()` ↔ block-oriented IPC). MIDI queue + registry polling lives
//! in [`midi::Midi`]. Main-thread editor / state / parameter access is
//! on [`crate::handles::PluginHandle`]; `PluginClient` exposes only what the
//! audio path needs.

mod audio_unit;
mod batcher;
mod harmony_source;
mod listeners;
mod midi;
pub(crate) mod node_id;
mod process;
mod signal;

#[cfg(test)]
mod tests;

pub(crate) use listeners::ResyncSink;
// Public so `crate::backend` can re-export them for out-of-crate in-process
// loaders (e.g. `tutti-wasm-plugin`). `ResyncSink` stays crate-internal.
pub use listeners::{LatencyChangeSink, ParameterChangeSink};
pub use harmony_source::{HarmonySource, TimedChord, TimedScale};
pub use midi::Midi;
pub(crate) use process::ProcessGuard;
// Public so `crate::backend` can re-export it for out-of-crate in-process
// loaders (e.g. `tutti-wasm-plugin`); also used by the in-crate vst2-in-process
// path.
pub use signal::route_with_latency;

use crate::bridge::audio::BridgeEvent;
use crate::bridge::PluginBridge;
use crate::config::BridgeConfig;
use crate::error::Result;
use crate::protocol::{LoadedPlugin, PluginDescriptor, SampleFormat};
use crate::subprocess;
use batcher::Batcher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tutti_midi_types::ump::MidiEvent;

/// Cheap to clone: clones share `bridge`, `latency`, and `process_guard`
/// (all Arc) but get independent `io` and `midi` state (fundsp clones
/// nodes on graph commit).
#[derive(Clone)]
pub struct PluginClient {
    bridge: Arc<PluginBridge>,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
    format: SampleFormat,
    /// Shared across clones so runtime latency updates are seen by
    /// whichever clone fundsp is currently processing.
    latency: Arc<AtomicUsize>,
    /// Observers for plugin-originated unsolicited events. Shared with
    /// `PluginHandle` so callers can register callbacks via the handle
    /// and still see events driven by the bridge thread.
    latency_sink: LatencyChangeSink,
    param_sink: ParameterChangeSink,
    resync_sink: ResyncSink,
    /// Subprocess lifetime, shared with `PluginHandle::from_client`.
    process_guard: Arc<ProcessGuard>,
    io: Batcher,
    midi: Midi,
    harmony: Harmony,
}

/// Per-client chord/scale producer state. The optional source is shared across
/// fundsp graph-commit clones (Arc, like `Midi::source_override`); the per-block
/// drain buffer is rebuilt fresh per clone since it's scratch.
#[derive(Default)]
struct Harmony {
    source: Option<Arc<HarmonySource>>,
    drain: crate::bridge::audio::HarmonyInputs,
}

impl Clone for Harmony {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            drain: crate::bridge::audio::HarmonyInputs::default(),
        }
    }
}

impl Harmony {
    /// Fill (and return) the per-block harmony inputs from the installed
    /// source, or an empty bundle when no source is installed.
    fn drain_for_process(&mut self, block_size: usize) -> &crate::bridge::audio::HarmonyInputs {
        self.drain.chords.changes.clear();
        self.drain.scales.changes.clear();
        if let Some(src) = &self.source {
            src.fill(block_size, &mut self.drain);
        }
        &self.drain
    }
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

    pub(super) fn midi_mut(&mut self) -> &mut Midi {
        &mut self.midi
    }

    /// Fill the per-block chord/scale context from the installed harmony
    /// source (empty when none is installed). Cloned out for the bridge call.
    pub(super) fn drain_harmony(&mut self, block_size: usize) -> crate::bridge::audio::HarmonyInputs {
        self.harmony.drain_for_process(block_size).clone()
    }

    pub(super) fn bridge_ref(&self) -> &Arc<PluginBridge> {
        &self.bridge
    }
}

impl PluginClient {
    /// Spawns the plugin-server, loads `plugin_path`, and returns the
    /// audio client. The subprocess lifetime is held internally by an
    /// `Arc<ProcessGuard>` shared with any `PluginHandle` built from
    /// this client; the subprocess dies when both the fundsp graph has
    /// released the AudioUnit and all handles have dropped.
    pub fn new(config: BridgeConfig, plugin_path: PathBuf, sample_rate: f64) -> Result<Self> {
        let server = subprocess::launch(&config, &plugin_path, sample_rate)?;

        // Report the FULL input width (main + sidechain/aux input buses) so a
        // fundsp `connect(src, 0, target, 1)` lands on a real sidechain port;
        // the batcher writes each input port into the matching flat slab
        // channel (bus-ordered, base 0). The output direction reads from the
        // slab's per-direction output base so it never aliases the inputs.
        // Read the layout BEFORE the slab is moved into the bridge.
        let inputs: usize = server.loaded.total_inputs();
        let outputs: usize = server.loaded.total_outputs();
        let output_base = server.audio_buffer.layout_ref().output_base();

        let (bridge, bridge_thread) =
            PluginBridge::new(config.socket_path.clone(), server.audio_buffer, plugin_path)?;

        let latency = Arc::new(AtomicUsize::new(server.loaded.latency_samples));
        let max_buffer_size = config.max_buffer_size;
        let process_guard = Arc::new(ProcessGuard::new(server.process, bridge_thread, config));
        let latency_sink = LatencyChangeSink::new();
        let param_sink = ParameterChangeSink::new();
        let resync_sink = ResyncSink::new();

        // Route unsolicited bridge events into the shared atomic + sinks.
        // The bridge-thread callback must be cheap; it writes the latency
        // atomic directly so `AudioUnit::latency()` sees the new value on
        // the next audio read, then fires the sink for the main thread.
        let listener_latency = Arc::clone(&latency);
        let listener_latency_sink = latency_sink.clone();
        let listener_param_sink = param_sink.clone();
        let listener_resync_sink = resync_sink.clone();
        bridge.set_listener(Some(Arc::new(move |ev| match ev {
            BridgeEvent::LatencyChanged { samples } => {
                listener_latency.store(samples, Ordering::Release);
                listener_latency_sink.fire(samples);
            }
            BridgeEvent::ParameterChanged { index, value } => {
                if let Ok(id) = u32::try_from(index) {
                    listener_param_sink.fire(id, value);
                }
            }
            BridgeEvent::Resync(kind) => {
                listener_resync_sink.fire(kind);
            }
        })));

        Ok(Self {
            bridge,
            descriptor: server.descriptor,
            loaded: server.loaded,
            format: server.format,
            latency,
            latency_sink,
            param_sink,
            resync_sink,
            process_guard,
            io: Batcher::new(inputs, outputs, output_base, server.format, max_buffer_size),
            midi: Midi::new(),
            harmony: Harmony::default(),
        })
    }

    pub(crate) fn latency_sink(&self) -> &LatencyChangeSink {
        &self.latency_sink
    }

    pub(crate) fn param_sink(&self) -> &ParameterChangeSink {
        &self.param_sink
    }

    pub(crate) fn resync_sink(&self) -> &ResyncSink {
        &self.resync_sink
    }

    /// Accessor for `PluginHandle::from_client` — not for end users.
    pub(crate) fn process_guard(&self) -> &Arc<ProcessGuard> {
        &self.process_guard
    }

    pub fn latency(&self) -> usize {
        self.latency.load(Ordering::Acquire)
    }

    /// Runtime latency update. RT-safe.
    ///
    /// Normally driven by the bridge thread when the plugin-server emits
    /// `BridgeMessage::LatencyChanged` (installed by `PluginClient::new`);
    /// exposed publicly so callers can also force a value. Note: updating
    /// what `AudioUnit::latency()` reports does **not** re-run PDC on its
    /// own — a graph edit (`GraphNet::commit()`) is required. Register a
    /// callback via `PluginHandle::on_latency_changed` to get notified.
    pub fn set_latency(&self, samples: usize) {
        self.latency.store(samples, Ordering::Release);
    }

    /// Catalog identity (id, name, vendor, version, native class, editor).
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Engine-wiring data from load (per-bus channel widths, latency, f64).
    pub fn loaded(&self) -> &LoadedPlugin {
        &self.loaded
    }

    pub fn format(&self) -> SampleFormat {
        self.format
    }

    /// When true, all audio processing produces silence.
    pub fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }

    /// RT-safe, fire-and-forget. Main-thread parameter reads/writes live
    /// on [`crate::handles::PluginHandle`]; this method exists because registry
    /// builders push initial parameter values through the `PluginClient`
    /// before any `PluginHandle` has been constructed.
    pub fn set_parameter(&self, param_id: u32, value: f32) {
        let _ = self.bridge.set_parameter_rt(param_id, value);
    }

    /// Push the host automation read/write state to the plugin (VST3
    /// `IAutomationState`). RT-safe, fire-and-forget; a no-op for plugins /
    /// formats without the concept. `state` is the VST3 `AutomationStates`
    /// bitmask (`0=none, 1=read, 2=write, 3=read|write`).
    pub fn set_automation_state(&self, state: i32) {
        let _ = self.bridge.set_automation_state_rt(state);
    }

    /// Producer handle for this plugin's MIDI inbox.
    pub fn midi_sender(&self) -> tutti_midi_runtime::MidiSender {
        self.midi.sender()
    }

    /// Events are buffered and sent on the next `process()`.
    pub fn queue_midi(&mut self, events: &[MidiEvent]) {
        self.midi.queue(events);
    }

    pub fn clear_midi(&mut self) {
        self.midi.clear();
    }

    /// Install a [`tutti_midi_types::MidiSource`] override (typically
    /// [`tutti_midi_runtime::MidiClipSource`] from a track's MIDI
    /// clips) that the plugin polls per block instead of its live
    /// `MidiReceiver`. Mirrors `PolySynth::set_midi_source` so
    /// MIDI clips drive plugin synths the same way they drive
    /// built-in synths.
    ///
    /// The source is held in an `Arc`, so the same instance survives
    /// the unit-clone fundsp performs on each `commit()`.
    pub fn set_midi_source(&mut self, source: std::sync::Arc<dyn tutti_midi_types::MidiSource>) {
        self.midi.set_source(source);
    }

    /// Install a [`HarmonySource`] override that supplies per-block chord/scale
    /// context (VST3 `kChordEvent` / `kScaleEvent`) from a track's chord/scale
    /// lanes. Mirrors [`set_midi_source`](Self::set_midi_source); the source is
    /// held in an `Arc` so it survives fundsp's graph-commit clones.
    pub fn set_harmony_source(&mut self, source: std::sync::Arc<HarmonySource>) {
        self.harmony.source = Some(source);
    }

    /// Drop a previously-installed harmony source. Subsequent blocks feed the
    /// plugin empty chord/scale context.
    pub fn clear_harmony_source(&mut self) {
        self.harmony.source = None;
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
