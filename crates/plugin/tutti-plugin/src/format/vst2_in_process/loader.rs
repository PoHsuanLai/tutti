//! Construct the `(AudioUnit, PluginHandle)` pair for an in-process VST2.

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_vst2_host::Vst2Instance;

use super::audio_unit::InProcessVst2Client;
use super::control_backend::InProcessVst2Backend;
use crate::error::{BridgeError, LoadStage, Result};
use crate::host::handles::PluginHandle;
use crate::host::node::{LatencyChangeSink, ParameterChangeSink};
use crate::protocol::{Features, LoadedPlugin, PluginClass, PluginDescriptor};
use smallvec::SmallVec;

/// Maximum block size we pre-size the plugin's render scratch for.
/// Plugins are told this is the upper bound; per-call sizes may be
/// smaller.
const MAX_BLOCK_SIZE: usize = 4096;

/// Load a VST2 plugin in-process. Returns the audio-graph node (a
/// boxed `AudioUnit`) and a control handle.
///
/// The returned audio unit and handle share the underlying
/// `tutti_vst2_host::Vst2Instance` via an `Arc<Mutex<…>>`. Drop both to drop
/// the plugin. Editors are opened through the handle; audio happens on
/// whatever thread fundsp drives the unit from.
pub fn load(
    path: &Path,
    sample_rate: f64,
) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
    let inner = Vst2Instance::load(path, sample_rate, MAX_BLOCK_SIZE).map_err(|e| {
        BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: format!("vst2-host load failed: {e}"),
        }
    })?;

    let host_meta = inner.metadata().clone();
    let descriptor = PluginDescriptor {
        id: host_meta.id.clone(),
        name: host_meta.name.clone(),
        vendor: host_meta.vendor.clone(),
        version: host_meta.version.clone(),
        class: PluginClass::Vst2 {
            category: host_meta.category,
        },
        has_editor: host_meta.has_editor,
    };
    // VST2 feature set — mirrors the out-of-process VST2 loader: f64 (advertised,
    // informational), MIDI both directions from the combined flag, editor, and a
    // transport snapshot each block. No sample-accurate automation, note
    // expression, sequencer context, or host-driven editor resize.
    let mut features = Features::empty();
    features.set(Features::F64_AUDIO, host_meta.supports_f64);
    features.set(Features::MIDI_IN, host_meta.receives_midi);
    features.set(Features::MIDI_OUT, host_meta.emits_midi);
    features.set(Features::EDITOR, host_meta.has_editor);
    features.insert(Features::TRANSPORT);

    // VST2 is single-bus: one main input bus and one main output bus.
    let loaded = LoadedPlugin {
        inputs: SmallVec::from_slice(&[host_meta.num_inputs]),
        outputs: SmallVec::from_slice(&[host_meta.num_outputs]),
        latency_samples: host_meta.latency_samples,
        features,
    };

    let inner = Arc::new(Mutex::new(inner));
    let contention = Arc::new(AtomicU64::new(0));
    let param_sink = ParameterChangeSink::new();
    let latency_sink = LatencyChangeSink::new();

    let backend = Arc::new(InProcessVst2Backend {
        inner: Arc::clone(&inner),
        param_sink: param_sink.clone(),
    });

    let client = InProcessVst2Client::new(
        Arc::clone(&inner),
        host_meta,
        sample_rate,
        Arc::clone(&contention),
    );
    let midi_sender = client.midi_sender();

    // VST2 has an embeddable editor: the same backend Arc serves the editor slot.
    let editor: Arc<dyn crate::backend::HostEditor> = backend.clone();
    let handle = PluginHandle::from_backend(
        backend,
        Some(editor),
        descriptor,
        loaded,
        latency_sink,
        param_sink,
        midi_sender,
    );

    Ok((Box::new(client), handle))
}
