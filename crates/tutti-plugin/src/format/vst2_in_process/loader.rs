//! Construct the `(InProcessVst2Client, PluginHandle)` pair for an
//! in-process VST2.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;
use tutti_plugin_types::Control;

use super::client::{Declared, InProcessVst2Client};
use super::control_backend::InProcessVst2Backend;
use super::instance::Instance;
use crate::error::{BridgeError, Result};
use crate::host::handles::PluginHandle;
use crate::host::node::ParameterChangeSink;
use crate::protocol::{Features, LoadedPlugin};

/// The maximum block size the plugin is told (`effSetBlockSize`). A graph block
/// longer than this (the engine renders up to 8192, more with
/// `Engine::with_capacity`) is split by `Vst2Processor::process_f32` into calls
/// of at most this length, so the plugin is never entered with more frames than
/// it was told. Splitting rather than re-announcing the graph's maximum: the
/// announcement is an `effMainsChanged` bracket that belongs on the main thread
/// and reaches a live node only through the editor pump, so it cannot be relied
/// on to have happened before the first block.
const MAX_BLOCK_SIZE: usize = 4096;

/// Loads a VST2 plugin in the host process, returning its graph node and
/// control handle.
///
/// [`Plugin::open`](crate::catalog::Plugin::open) calls this for `.vst` files
/// when the `vst2` feature is on; call it directly only to get the concrete
/// [`InProcessVst2Client`] type. Insert the node with `Editor::insert` (it is a
/// `tutti_graph::Node`). The node and handle share the plugin instance; drop
/// both to drop the plugin. Editors are opened through the handle, and audio
/// runs on whatever thread the graph's executor runs on. Blocks while the
/// plugin loads, so call it off the audio thread.
///
/// # Errors
///
/// Returns [`BridgeError::LoadFailed`] when the library cannot be opened or
/// the plugin fails to initialize.
pub fn load_client(
    path: &Path,
    sample_rate: impl Into<tutti_core::SampleRate>,
) -> Result<(InProcessVst2Client, PluginHandle)> {
    // `.get()` here and not at the caller: this is the last hop before
    // `Instance::load`, which crosses the VST2 ABI and takes a bare rate.
    let sample_rate = sample_rate.into().get();
    let inner = Instance::load(path, sample_rate, MAX_BLOCK_SIZE).map_err(|(stage, reason)| {
        BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage,
            reason,
        }
    })?;

    let host_meta = inner.metadata().clone();
    let descriptor = Control::info(&**inner).clone();
    // VST2 feature set — what the host crate's wiring declares, the same set
    // the out-of-process VST2 loader answers: f64 (backed by
    // `processReplacingF64`), MIDI both directions, the editor, and a transport
    // snapshot each block. No sample-accurate automation, note expression,
    // sequencer context, or host-driven editor resize. The snapshot is the
    // host's to give, not the plugin's to request: VST2 exposes no query for it,
    // and every plugin can poll `audioMasterGetTime` whenever it likes. The node
    // honours the claim by draining its transport slot into each
    // `Vst2ProcessContext`, which is what fills the `TimeInfo` that callback
    // serves.
    let mut loaded: LoadedPlugin = Control::wiring(&**inner);
    // Both preset bits come from one number: VST2 has no separate "can you
    // load" query, and a plugin declaring programs can always be sent
    // `effProgramChange`. A plugin with none answers `false` to both — a
    // declination, not silence, which is why `probed::VST2` lists them.
    let has_programs = inner.program_count() > 0;
    loaded.features.set(Features::PRESET_LIST, has_programs);
    loaded.features.set(Features::PRESET_LOAD, has_programs);

    let declared = Declared {
        inputs: inner.pins().0,
        outputs: inner.pins().1,
        latency: loaded.latency_samples,
        tail: loaded.tail,
    };
    // Read once, here: the node's parameter slots are sized from it.
    let param_count = inner.parameter_count();
    let inner = Arc::new(Mutex::new(inner));
    let contention = Arc::new(AtomicU64::new(0));
    let param_sink = ParameterChangeSink::new();
    let invalidate_sink = crate::host::node::InvalidateSink::new();

    // Built here rather than inside the node so the node's producer end and the
    // backend's drain end are the same cell: the audio thread parks a rate in
    // it, `editor_idle` dispatches from it.
    //
    // **Limit:** `editor_idle` is the only main-thread tick an in-process
    // plugin has, and bevy-tutti calls it only while an editor is open. So a
    // parked rate change, and the re-reads of latency, tail and pin counts
    // (`Instance::poll_changes`, raised as `PluginInvalidation` through the
    // handle's `on_invalidate`), happen only while the editor pump runs. The
    // session does not tick an in-process plugin's handle; a host that wants
    // them without an editor calls `PluginHandle::editor_idle` on its own tick.
    let pending_sample_rate = Arc::new(AtomicU64::new(super::client::NO_PENDING_RATE));

    let backend = Arc::new(InProcessVst2Backend {
        inner: Arc::clone(&inner),
        param_sink: param_sink.clone(),
        pending_sample_rate: Arc::clone(&pending_sample_rate),
        invalidate_sink: invalidate_sink.clone(),
    });

    // `loaded.features`, not the local `features`, so the node gates its
    // per-block transport send on the exact value the handle reports. One value,
    // read twice, cannot drift into a declared-but-undelivered capability.
    let client = InProcessVst2Client::new(
        Arc::clone(&inner),
        host_meta,
        declared,
        loaded.features,
        sample_rate,
        pending_sample_rate,
        Arc::clone(&contention),
        Arc::from(path),
        param_count,
    );
    // VST2 has an embeddable editor and carries a render mode: the same backend
    // Arc serves both optional slots, so a mode set through the handle reaches
    // the very instance the node renders.
    let editor: Arc<dyn crate::backend::HostEditor> = backend.clone();
    let render_mode: Arc<dyn crate::host::handles::capabilities::HostRenderMode> = backend.clone();
    let presets: Arc<dyn crate::backend::HostPresets> = backend.clone();
    let handle = PluginHandle::from_backend(
        backend,
        crate::handles::OptionalCapabilities {
            editor: Some(editor),
            render_mode: Some(render_mode),
            presets: Some(presets),
        },
        descriptor,
        loaded,
        param_sink,
    )
    .with_invalidate_sink(invalidate_sink);

    Ok((client, handle))
}
