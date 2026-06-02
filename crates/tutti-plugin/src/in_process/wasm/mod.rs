//! In-process WASM Component Model audio plugin host.
//!
//! Targets the `dawai:audio-plugin@0.1.0` WIT world. Pairs a wasmtime
//! `Store` with the `PluginHandle` API the other backends expose. The
//! audio thread reaches the plugin through `try_lock` on a shared
//! `Arc<Mutex<WasmInstance>>` — on contention with the GUI thread
//! (parameter editing, state save), audio emits silence for one block
//! and bumps a contention counter rather than blocking.
//!
//! Unlike VST2/VST3/CLAP/AU, this loader runs entirely in the host
//! process; there is no `plugin-server` involvement. The wasmtime
//! sandbox already provides equivalent crash containment to a
//! subprocess.

mod audio_unit;
mod control_backend;
mod instance;
mod loader;
mod runtime;

#[allow(unused_imports)]
pub use audio_unit::InProcessWasmClient;
pub use loader::load;
