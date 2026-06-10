//! Format-specific FFI — the only code in this crate that touches a concrete
//! plugin SDK.
//!
//! Everything else (catalog, IPC client, fundsp node, control surface) is
//! format-agnostic. Per-format knowledge is confined here, in two groups:
//!
//! - [`gui`] — **in-process editors** for the out-of-process formats
//!   (VST3/CLAP/AU). Their *audio* runs in the `tutti-plugin-server`
//!   subprocess (via the `tutti-*-host` crates), but the editor must open in
//!   the host process because platform GUI toolkits are process-local, so each
//!   format dlopens the plugin a second time purely for its window.
//! - [`vst2_in_process`] — VST2's **in-process audio + editor** path. Unlike
//!   VST3/CLAP/AU, VST2 fuses the editor and audio processor into one
//!   `AEffect`, so the editor can't live in a different process from audio
//!   without remote-rendering pixels. Hosting in-process is the standard
//!   tradeoff (Ableton/Logic/Reaper all do it for VST2): no subprocess crash
//!   isolation, but a native editor.
//!
//! WASM Component Model audio plugins (`dawai:audio-plugin@0.1.0`) are also
//! always in-process, but live in the separate `tutti-wasm-plugin` crate so
//! the heavy wasmtime dependency stays out of this crate; they reuse this
//! crate's [`PluginHandle`](crate::host::handles::PluginHandle) /
//! [`ControlBackend`](crate::backend::ControlBackend) machinery.

pub mod gui;

#[cfg(feature = "vst2-in-process")]
pub mod vst2_in_process;
