//! In-process plugin hosts.
//!
//! Two formats live here:
//!
//! - VST2 — unlike VST3/CLAP/AU, VST2 fuses the editor and the audio
//!   processor into a single `AEffect`, so the editor cannot live in a
//!   different process from audio without remote-rendering the pixel
//!   surface. Hosting in-process is the standard tradeoff (Ableton,
//!   Logic, Reaper all do it for VST2): you sacrifice subprocess crash
//!   isolation to get a native editor.
//! - WASM — Component Model audio plugins (`dawai:audio-plugin@0.1.0`).
//!   The wasmtime sandbox already provides memory isolation and trap
//!   containment, so a subprocess wrapper would add IPC overhead without
//!   buying additional safety. Always in-process.

#[cfg(feature = "vst2-in-process")]
pub mod vst2;

#[cfg(feature = "wasm")]
pub mod wasm;
