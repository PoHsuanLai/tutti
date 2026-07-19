//! Per-format [`tutti_plugin::server::PluginInstance`] adapters.
//!
//! Each submodule wraps one format-host crate and hides its quirks behind
//! the unified trait the server dispatches to. Modules are feature-gated;
//! a build without any loader feature still compiles but has no formats
//! to load.
//!
//! WASM Component Model plugins are intentionally absent — they're
//! sandboxed by wasmtime and run in-process via `tutti-plugin`'s
//! `in_process::wasm` module, not through this server.

pub(crate) mod common;

#[cfg(feature = "au")]
pub(crate) mod au;
#[cfg(feature = "clap")]
pub(crate) mod clap;
#[cfg(feature = "vst2")]
pub(crate) mod vst2;
#[cfg(feature = "vst3")]
pub(crate) mod vst3;
