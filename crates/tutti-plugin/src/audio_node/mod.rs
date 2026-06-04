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
mod listeners;
mod midi;
mod process;
mod signal;

#[cfg(test)]
mod tests;

pub(crate) use listeners::{LatencyChangeSink, ParameterChangeSink};
pub(crate) use midi::Midi;
pub(crate) use process::ProcessGuard;
#[cfg(any(feature = "vst2-in-process", feature = "wasm"))]
pub(crate) use signal::route_with_latency;

use crate::bridge::audio::BridgeEvent;
use crate::bridge::PluginBridge;
use crate::config::BridgeConfig;
use crate::error::Result;
use crate::protocol::{PluginInfo, SampleFormat};
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
    metadata: PluginInfo,
    format: SampleFormat,
    /// Shared across clones so runtime latency updates are seen by
    /// whichever clone fundsp is currently processing.
    latency: Arc<AtomicUsize>,
    /// Observers for plugin-originated unsolicited events. Shared with
    /// `PluginHandle` so callers can register callbacks via the handle
    /// and still see events driven by the bridge thread.
    latency_sink: LatencyChangeSink,
    param_sink: ParameterChangeSink,
    /// Subprocess lifetime, shared with `PluginHandle::from_client`.
    process_guard: Arc<ProcessGuard>,
    io: Batcher,
    midi: Midi,
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
        let inputs: usize = server.metadata.input_bus_channels().iter().sum();
        let outputs: usize = server.metadata.output_bus_channels().iter().sum();
        let output_base = server.audio_buffer.layout_ref().output_base();

        let (bridge, bridge_thread) =
            PluginBridge::new(config.socket_path.clone(), server.audio_buffer, plugin_path)?;

        let latency = Arc::new(AtomicUsize::new(server.metadata.latency_samples));
        let max_buffer_size = config.max_buffer_size;
        let process_guard = Arc::new(ProcessGuard::new(server.process, bridge_thread, config));
        let latency_sink = LatencyChangeSink::new();
        let param_sink = ParameterChangeSink::new();

        // Route unsolicited bridge events into the shared atomic + sinks.
        // The bridge-thread callback must be cheap; it writes the latency
        // atomic directly so `AudioUnit::latency()` sees the new value on
        // the next audio read, then fires the sink for the main thread.
        let listener_latency = Arc::clone(&latency);
        let listener_latency_sink = latency_sink.clone();
        let listener_param_sink = param_sink.clone();
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
        })));

        Ok(Self {
            bridge,
            metadata: server.metadata,
            format: server.format,
            latency,
            latency_sink,
            param_sink,
            process_guard,
            io: Batcher::new(inputs, outputs, output_base, server.format, max_buffer_size),
            midi: Midi::new(),
        })
    }

    pub(crate) fn latency_sink(&self) -> &LatencyChangeSink {
        &self.latency_sink
    }

    pub(crate) fn param_sink(&self) -> &ParameterChangeSink {
        &self.param_sink
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

    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
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
