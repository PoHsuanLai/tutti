//! In-process WASM Component Model audio plugin host.
//!
//! Targets the `dawai:audio-plugin@0.1.0` WIT world. Pairs a wasmtime
//! `Store` with the [`PluginHandle`](tutti_plugin::handles::PluginHandle) API
//! the other tutti backends expose. The audio thread reaches the plugin
//! through `try_lock` on a shared `Arc<Mutex<WasmInstance>>` — on contention
//! with the GUI thread (parameter editing, state save), audio emits silence
//! for one block and bumps a contention counter rather than blocking.
//!
//! Unlike VST2/VST3/CLAP/AU (hosted out-of-process by `tutti-plugin` /
//! `tutti-plugin-server`), this loader runs entirely in the host process;
//! there is no `plugin-server` involvement. The wasmtime sandbox already
//! provides equivalent crash containment to a subprocess.
//!
//! # Entry point
//!
//! [`load`] returns the `(Box<dyn AudioUnit>, PluginHandle)` pair, mirroring
//! `tutti_plugin::Plugins::load` for the native formats. Hosts that route by
//! format dispatch WASM records here and everything else to
//! `tutti_plugin::Plugins::load`.

mod audio_unit;
mod control_backend;
mod instance;
mod loader;
mod runtime;

pub use audio_unit::InProcessWasmClient;
pub use loader::load;
