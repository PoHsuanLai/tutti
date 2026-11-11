//! Construct the `(InProcClient<F>, PluginHandle)` pair for an in-process
//! plugin of format `F`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tutti_plugin_types::MainThread;

use parking_lot::Mutex;

use super::client::{Declared, InProcClient};
use super::control_backend::ControlBackend;
use super::instance::Instance;
use crate::host::handles::PluginHandle;
use crate::host::node::ParameterChangeSink;
use tutti_plugin_ipc::{BridgeError, Result};
use tutti_plugin_types::{Features, InProcess, Load};
use tutti_types::ChannelLayout;

/// The maximum block size the plugin is told (`Prepare::max_block`). A graph
/// block longer than this (the engine renders up to 8192, more with
/// `Engine::with_capacity`) is split by the Processor into calls of at most
/// this length, so the plugin is never entered with more frames than it was
/// told. Splitting rather than re-announcing the graph's maximum: the
/// announcement is a reconfiguration that belongs on the main thread and
/// reaches a live node only through the editor pump, so it cannot be relied
/// on to have happened before the first block.
const MAX_BLOCK_SIZE: usize = 4096;

/// Loads a plugin of format `F` in the host process, returning its graph node
/// and control handle.
///
/// [`Plugin::open`](crate::catalog::Plugin::open) calls this for `.vst` files
/// when the `vst2` feature is on; call it directly for another [`InProcess`]
/// format, or to get the concrete [`InProcClient`] type. `load` names the
/// plugin in a file that holds several. Insert the node with `Editor::insert`
/// (it is a `tutti_graph::Node`). The node and handle share the plugin
/// instance; drop both to drop the plugin. Editors are opened through the
/// handle, and audio runs on whatever thread the graph's executor runs on.
/// Blocks while the plugin loads, so call it off the audio thread.
///
/// # Errors
///
/// Returns [`BridgeError::LoadFailed`] when the library cannot be opened or
/// the plugin fails to initialize.
pub fn load_client<F>(
    main: &MainThread,
    path: &Path,
    load: &Load,
    sample_rate: impl Into<tutti_types::SampleRate>,
) -> Result<(InProcClient<F>, PluginHandle)>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    load_client_restoring(main, path, load, sample_rate, None)
        .map(|(client, handle, _)| (client, handle))
}

/// [`load_client`], with `restore` run on the plugin before it is activated
/// (see the instance's `load`). The outcome is returned as the third value.
///
/// # Errors
///
/// As [`load_client`].
pub fn load_client_restoring<F>(
    main: &MainThread,
    path: &Path,
    load: &Load,
    sample_rate: impl Into<tutti_types::SampleRate>,
    restore: Option<&mut dyn FnMut(&mut F) -> std::result::Result<(), String>>,
) -> Result<(
    InProcClient<F>,
    PluginHandle,
    Option<std::result::Result<(), String>>,
)>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    // `.get()` here and not at the caller: this is the last hop before
    // `Instance::load`, which takes a bare rate into the activation.
    let sample_rate = sample_rate.into().get();
    let (mut inner, rt, restored) =
        Instance::<F>::load(main, path, load, sample_rate, MAX_BLOCK_SIZE, restore).map_err(
            |(stage, reason)| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage,
                reason,
            },
        )?;

    let origin = inner.origin().clone();
    let Some(control) = inner.control() else {
        return Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: tutti_plugin_types::LoadStage::Activation,
            reason: "the plugin was lost in its activation".into(),
        });
    };
    let descriptor = control.info().clone();
    // What the format's wiring declares, asked of the plugin now: its widths,
    // latency, tail and the capabilities the node gates its per-block sends
    // on. The snapshot of the transport is the host's to give, not the
    // plugin's to request, and the node honours the claim by reading it from
    // the block's `Cx` into each `ProcessContext`.
    let mut loaded = control.wiring(main);
    let parameters = control.parameters(main);
    // A plugin with presets can be listed and sent one. A format whose
    // wiring already says so keeps it; one with none says nothing either
    // way, so a plugin with none is not marked as declining them.
    if !control.presets(main).is_empty() {
        loaded.features.insert(Features::PRESET_LIST);
        loaded.features.insert(Features::PRESET_LOAD);
    }

    let declared = Declared {
        inputs: ChannelLayout::clamped(loaded.total_inputs()),
        outputs: ChannelLayout::clamped(loaded.total_outputs()),
        latency: loaded.latency_samples,
        tail: loaded.tail,
    };
    let inner = Arc::new(Mutex::new(inner));
    let unlent = Arc::new(AtomicU64::new(0));
    let param_sink = ParameterChangeSink::new();
    let invalidate_sink = crate::host::node::InvalidateSink::new();

    // Built here rather than inside the node so the node's producer end and the
    // backend's drain end are the same cell: the audio thread parks a rate in
    // it, `editor_idle` dispatches from it.
    //
    // **Limit:** `editor_idle` is the only main-thread tick an in-process
    // plugin has, and bevy-tutti calls it only while an editor is open. So a
    // parked rate change, and the re-reads of latency, tail and pin counts
    // (`Instance::pump`, raised as `PluginInvalidation` through the handle's
    // `on_invalidate`), happen only while the editor pump runs. The session
    // does not tick an in-process plugin's handle; a host that wants them
    // without an editor calls `PluginHandle::editor_idle` on its own tick.
    let pending_sample_rate = Arc::new(AtomicU64::new(super::client::NO_PENDING_RATE));

    let backend = Arc::new(ControlBackend {
        inner: Arc::clone(&inner),
        param_sink: param_sink.clone(),
        pending_sample_rate: Arc::clone(&pending_sample_rate),
        invalidate_sink: invalidate_sink.clone(),
    });

    // `loaded.features`, not a local copy, so the node gates its per-block
    // transport send on the exact value the handle reports. One value, read
    // twice, cannot drift into a declared-but-undelivered capability.
    let client = InProcClient::new(
        Arc::clone(&inner),
        rt,
        descriptor.clone(),
        origin,
        declared,
        loaded.features,
        sample_rate,
        pending_sample_rate,
        Arc::clone(&unlent),
        parameters.iter().map(|p| p.id),
    );
    // The same backend Arc serves every optional slot, so a mode set through
    // the handle reaches the very instance the node renders.
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

    Ok((client, handle, restored))
}
