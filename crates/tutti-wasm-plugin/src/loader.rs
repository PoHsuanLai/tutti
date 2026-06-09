//! Construct the `(AudioUnit, PluginHandle)` pair for an in-process
//! WASM audio plugin.

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use parking_lot::Mutex;

use tutti_plugin::backend::{LatencyChangeSink, ParameterChangeSink};
use tutti_plugin::handles::PluginHandle;
use tutti_plugin::Result;

use crate::audio_unit::InProcessWasmClient;
use crate::control_backend::InProcessWasmBackend;
use crate::instance::WasmInstance;

/// Block size we report to the WASM guest at `init`. The guest may
/// pre-allocate scratch up to this size; we honor it from the audio
/// unit's `ensure_scratch_size` ceiling. Matches the VST2 in-process
/// equivalent (4 KiB samples = 85 ms at 48 kHz, comfortable upper bound
/// for any realistic DAW block).
const MAX_BLOCK_SIZE: usize = 4096;

/// Load a WASM Component Model audio plugin in-process. Returns the
/// audio-graph node (a boxed `AudioUnit`) and a control handle.
///
/// The returned audio unit and handle share the underlying
/// `WasmInstance` via an `Arc<Mutex<…>>`. Drop both to drop the plugin
/// (the wasmtime store, linear memory, and pool slot release on drop).
///
/// The first call to this function in a process initializes the shared
/// wasmtime engine and spawns the epoch watchdog thread. Subsequent
/// calls reuse them.
pub fn load(
    path: &Path,
    sample_rate: f64,
) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
    let mut instance = WasmInstance::load(path, sample_rate, MAX_BLOCK_SIZE)?;

    // Warm-prime trap handlers and JIT paths on the main thread so the
    // audio thread's first `process` call doesn't pay first-touch costs
    // (wasmtime installs trap signal handlers via OnceLock the first
    // time a guest runs).
    instance.warm_prime();

    let metadata = instance.metadata().clone();

    let inner = Arc::new(Mutex::new(instance));
    let contention = Arc::new(AtomicU64::new(0));
    let param_sink = ParameterChangeSink::new();
    let latency_sink = LatencyChangeSink::new();

    let backend = Arc::new(InProcessWasmBackend {
        inner: Arc::clone(&inner),
    });

    let client =
        InProcessWasmClient::new(Arc::clone(&inner), metadata.clone(), sample_rate, contention);
    let midi_sender = client.midi_sender();

    let handle =
        PluginHandle::from_backend(backend, metadata, latency_sink, param_sink, midi_sender);

    Ok((Box::new(client), handle))
}
