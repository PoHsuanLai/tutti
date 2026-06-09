//! In-process plugin hosts.
//!
//! VST2 — unlike VST3/CLAP/AU, VST2 fuses the editor and the audio
//! processor into a single `AEffect`, so the editor cannot live in a
//! different process from audio without remote-rendering the pixel
//! surface. Hosting in-process is the standard tradeoff (Ableton,
//! Logic, Reaper all do it for VST2): you sacrifice subprocess crash
//! isolation to get a native editor.
//!
//! WASM Component Model audio plugins (`dawai:audio-plugin@0.1.0`) are also
//! always in-process, but live in the separate `tutti-wasm-plugin` crate so
//! the heavy wasmtime dependency stays out of this crate. They reuse this
//! crate's [`PluginHandle`](crate::handles::PluginHandle) /
//! [`ControlBackend`](crate::backend::ControlBackend) machinery via the
//! [`backend`](crate::backend) module.

#[cfg(feature = "vst2-in-process")]
pub mod vst2;
