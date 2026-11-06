//! Test-only observation seam for the `ProcessData` this host hands the
//! plugin.
//!
//! Behind the off-by-default `conformance` feature. When enabled, `process`
//! calls `observe` with the fully-built `ProcessData` immediately before
//! `IAudioProcessor::process`, plus the `ProcessSetup` that was negotiated
//! at activation. A test installs an observer, drives the *real* host path,
//! and inspects exactly what the plugin would have received.
//!
//! This exists because the interesting VST3 host bugs are in the struct the
//! host assembles — bus counts, channel pointers, event ordering, param
//! queues — and that struct is otherwise never visible outside the one
//! `unsafe` call that consumes it. Reproducing the assembly in a test would
//! test the reproduction, not the host.
//!
//! Not compiled into normal builds: the hot path pays nothing, and no
//! observer state exists in a release binary.
//!
//! It also lets a test play the plugin's side of `IComponentHandler`
//! ([`plugin_perform_edit`], [`plugin_restart_component`]), for host paths the
//! reference plugin never drives itself.

pub use tutti_vst3_ffi::{Observer, clear_observer, set_observer};

/// Calls the host's `IComponentHandler::performEdit` through its COM vtable,
/// exactly as a plugin would — from whatever thread the caller is on.
///
/// The audio-probe plugin never reports edits on its own, so a test of what
/// the host does with queued edits (the plugin server's restart poll must not
/// drain or allocate for them) has no other way to queue some.
pub fn plugin_perform_edit(loaded: &super::Vst3Loaded, id: u32, value: f64) {
    loaded.plugin.simulate_perform_edit(id, value);
}

/// Calls the host's `IComponentHandler::restartComponent(flags)` through its
/// COM vtable, as a plugin would. See [`plugin_perform_edit`].
pub fn plugin_restart_component(loaded: &super::Vst3Loaded, flags: i32) {
    loaded.plugin.simulate_restart_component(flags);
}
