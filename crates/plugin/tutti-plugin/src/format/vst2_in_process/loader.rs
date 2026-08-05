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
use crate::host::node::ParameterChangeSink;
use crate::protocol::{
    EditorPresence, Features, LoadedPlugin, PluginClass, PluginDescriptor, PluginTail,
};
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
    let (client, handle) = load_client(path, sample_rate)?;
    Ok((Box::new(client), handle))
}

/// [`load`], keeping the concrete [`InProcessVst2Client`].
///
/// The same load, one boxing step earlier, so a caller that needs the client's
/// own surface (its MIDI port) is not left with only the `AudioUnit` supertrait.
/// `load` is this plus a `Box::new`, so the two cannot drift.
pub fn load_client(
    path: &Path,
    sample_rate: impl Into<tutti_core::SampleRate>,
) -> Result<(InProcessVst2Client, PluginHandle)> {
    // `.get()` here and not at the caller: this is the last hop before
    // `Vst2Instance::load`, which crosses the VST2 ABI and takes a bare rate.
    let sample_rate = sample_rate.into().get();
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
        editor: EditorPresence::measured(host_meta.has_editor),
    };
    // VST2 feature set — mirrors the out-of-process VST2 loader: f64 (backed by
    // `processReplacingF64`), MIDI both directions from the combined flag,
    // editor, and a transport snapshot each block. No sample-accurate
    // automation, note expression, sequencer context, or host-driven editor
    // resize.
    let mut features = Features::empty();
    features.set(Features::F64_AUDIO, host_meta.supports_f64);
    features.set(Features::MIDI_IN, host_meta.receives_midi);
    features.set(Features::MIDI_OUT, host_meta.emits_midi);
    features.set(Features::EDITOR, host_meta.has_editor);
    // Unconditional because the snapshot is the host's to give, not the
    // plugin's to request: VST2 exposes no query for it, and every plugin can
    // poll `audioMasterGetTime` whenever it likes. The node honours the claim by
    // draining its transport slot into each `ProcessContext`, which is what
    // fills the `TimeInfo` that callback serves.
    features.insert(Features::TRANSPORT);
    // The same five the out-of-process VST2 loader answers — this path differs
    // in where the plugin runs, not in what it is asked, so both read one
    // constant.
    let probed = crate::server::probed::VST2;

    // VST2 is single-bus: one main input bus and one main output bus.
    let loaded = LoadedPlugin {
        inputs: SmallVec::from_slice(&[host_meta.num_inputs]),
        outputs: SmallVec::from_slice(&[host_meta.num_outputs]),
        latency_samples: host_meta.latency_samples,
        // `Unknown`, matching the out-of-process VST2 loader exactly — see the
        // comment there. VST2 encodes tail inversely to every other format
        // (`0` means "no tail information, host decides", `1` means "no tail at
        // all"), so it cannot reuse `from_samples`, whose `0 => None` arm would
        // read "unknown" as "silent" and tell a bounce to add nothing after
        // every VST2 reverb.
        tail: PluginTail::Unknown,
        features,
        probed,
    };

    let inner = Arc::new(Mutex::new(inner));
    let contention = Arc::new(AtomicU64::new(0));
    let param_sink = ParameterChangeSink::new();

    // Built here rather than inside the node so the node's producer end and the
    // backend's drain end are the same cell: the audio thread parks a rate in
    // it, `editor_idle` dispatches from it.
    let pending_sample_rate = Arc::new(AtomicU64::new(super::audio_unit::NO_PENDING_RATE));

    let backend = Arc::new(InProcessVst2Backend {
        inner: Arc::clone(&inner),
        param_sink: param_sink.clone(),
        pending_sample_rate: Arc::clone(&pending_sample_rate),
    });

    // `loaded.features`, not the local `features`, so the node gates its
    // per-block transport send on the exact value the handle reports. One value,
    // read twice, cannot drift into a declared-but-undelivered capability.
    let client = InProcessVst2Client::new(
        Arc::clone(&inner),
        host_meta,
        loaded.features,
        sample_rate,
        pending_sample_rate,
        Arc::clone(&contention),
    );
    let midi_sender = client.midi_sender();

    // VST2 has an embeddable editor and carries a render mode: the same backend
    // Arc serves both optional slots, so a mode set through the handle reaches
    // the very `Vst2Instance` the node renders.
    let editor: Arc<dyn crate::backend::HostEditor> = backend.clone();
    let render_mode: Arc<dyn crate::host::handles::capabilities::HostRenderMode> = backend.clone();
    let handle = PluginHandle::from_backend(
        backend,
        Some(editor),
        Some(render_mode),
        descriptor,
        loaded,
        param_sink,
        midi_sender,
    );

    Ok((client, handle))
}
