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
use crate::protocol::{LoadedPlugin, PluginClass, PluginDescriptor, Vst2Category};
use smallvec::SmallVec;
use tutti_vst2_host::Vst2Category as HostVst2Category;

/// Map `tutti-vst2-host`'s category mirror to `tutti-plugin`'s wire mirror.
fn map_vst2_category(c: HostVst2Category) -> Vst2Category {
    match c {
        HostVst2Category::Unknown => Vst2Category::Unknown,
        HostVst2Category::Effect => Vst2Category::Effect,
        HostVst2Category::Synth => Vst2Category::Synth,
        HostVst2Category::Analysis => Vst2Category::Analysis,
        HostVst2Category::Mastering => Vst2Category::Mastering,
        HostVst2Category::Spacializer => Vst2Category::Spacializer,
        HostVst2Category::RoomFx => Vst2Category::RoomFx,
        HostVst2Category::SurroundFx => Vst2Category::SurroundFx,
        HostVst2Category::Restoration => Vst2Category::Restoration,
        HostVst2Category::OfflineProcess => Vst2Category::OfflineProcess,
        HostVst2Category::Shell => Vst2Category::Shell,
        HostVst2Category::Generator => Vst2Category::Generator,
    }
}

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
            category: map_vst2_category(host_meta.category),
        },
        has_editor: host_meta.has_editor,
    };
    // VST2 is single-bus: one main input bus and one main output bus.
    let loaded = LoadedPlugin {
        inputs: SmallVec::from_slice(&[host_meta.num_inputs]),
        outputs: SmallVec::from_slice(&[host_meta.num_outputs]),
        latency_samples: host_meta.latency_samples,
        supports_f64: host_meta.supports_f64,
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

    let handle = PluginHandle::from_backend(
        backend,
        descriptor,
        loaded,
        latency_sink,
        param_sink,
        midi_sender,
    );

    Ok((Box::new(client), handle))
}
