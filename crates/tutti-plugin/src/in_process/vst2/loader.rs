//! Construct the `(AudioUnit, PluginHandle)` pair for an in-process VST2.

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_vst2_host::Vst2Instance;

use super::audio_unit::InProcessVst2Client;
use super::control_backend::InProcessVst2Backend;
use crate::audio_node::{LatencyChangeSink, ParameterChangeSink};
use crate::error::{BridgeError, LoadStage, Result};
use crate::handles::PluginHandle;
use crate::protocol::PluginInfo;

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
    let plugin_info = PluginInfo::new(host_meta.id.clone(), host_meta.name.clone())
        .author(host_meta.vendor.clone())
        .version(host_meta.version.clone())
        .audio_io(host_meta.num_inputs, host_meta.num_outputs)
        .midi(host_meta.receives_midi)
        .f64_support(host_meta.supports_f64)
        .editor(host_meta.has_editor, None)
        .latency(host_meta.latency_samples);

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

    let handle =
        PluginHandle::from_backend(backend, plugin_info, latency_sink, param_sink, midi_sender);

    Ok((Box::new(client), handle))
}
