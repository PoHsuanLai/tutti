//! Per-format [`tutti_plugin::server::PluginInstance`] adapters.
//!
//! Each submodule wraps one format-host crate and hides its quirks behind
//! the unified trait the server dispatches to. Modules are feature-gated;
//! a build without any loader feature still compiles but has no formats
//! to load.

pub(crate) mod common;

// `all(feature, target_os)`, not `feature` alone — the pattern every AU site in
// `plugin.rs` already uses, and this declaration was the one place that missed
// it. AU is macOS-only: `tutti-au-host` compiles everywhere but is *empty* off
// macOS (its AudioToolbox deps sit under `[target.'cfg(target_os = "macos")']`
// and `topology.rs` carries an inner `#![cfg(target_os = "macos")]`). Gating on
// the feature alone therefore compiled 1,800 lines of AU loader against an empty
// crate on Linux — 11 unresolved-name errors — and `au` is in `default`, so it
// broke the default build of the binary rather than an opt-in one.
#[cfg(all(feature = "au", target_os = "macos"))]
pub(crate) mod au;
#[cfg(feature = "clap")]
pub(crate) mod clap;
#[cfg(feature = "vst2")]
pub(crate) mod vst2;
#[cfg(feature = "vst3")]
pub(crate) mod vst3;
