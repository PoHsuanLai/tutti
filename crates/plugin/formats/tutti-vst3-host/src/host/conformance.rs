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

use std::cell::RefCell;

use vst3::Steinberg::Vst::{ProcessData, ProcessSetup};

/// Callback invoked with the `ProcessData` the host built and the
/// `ProcessSetup` it negotiated at activation.
pub type Observer = Box<dyn FnMut(&ProcessData, &ProcessSetup)>;

thread_local! {
    /// Installed observer, if any. Thread-local because `process` runs on the
    /// audio thread and a test drives it directly on its own thread — there is
    /// no cross-thread handoff to model here.
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

/// Installs an observer for the current thread, replacing any previous one.
pub fn set_observer(f: Observer) {
    OBSERVER.with(|o| *o.borrow_mut() = Some(f));
}

/// Removes the current thread's observer.
pub fn clear_observer() {
    OBSERVER.with(|o| *o.borrow_mut() = None);
}

/// Hands the built `ProcessData` to the installed observer. Called by
/// `Vst3Active::process` just before the plugin sees it.
///
/// Borrow-safe against re-entrancy: an observer that somehow drove `process`
/// again would find the slot borrowed and be skipped rather than panicking.
pub(crate) fn observe(data: &ProcessData, setup: &ProcessSetup) {
    OBSERVER.with(|o| {
        if let Ok(mut slot) = o.try_borrow_mut() {
            if let Some(f) = slot.as_mut() {
                f(data, setup);
            }
        }
    });
}

/// Calls the host's `IComponentHandler::performEdit` through its COM vtable,
/// exactly as a plugin would — from whatever thread the caller is on.
///
/// The audio-probe plugin never reports edits on its own, so a test of what
/// the host does with queued edits (the plugin server's restart poll must not
/// drain or allocate for them) has no other way to queue some.
pub fn plugin_perform_edit(loaded: &super::Vst3Loaded, id: u32, value: f64) {
    use vst3::Steinberg::Vst::{IComponentHandler, IComponentHandlerTrait};
    if let Some(handler) = loaded.host.handler.as_com_ref::<IComponentHandler>() {
        // SAFETY: the handler is host-implemented and alive for `loaded`'s
        // lifetime; `performEdit` reads only its two scalar arguments.
        unsafe { handler.performEdit(id, value) };
    }
}

/// Calls the host's `IComponentHandler::restartComponent(flags)` through its
/// COM vtable, as a plugin would. See [`plugin_perform_edit`].
pub fn plugin_restart_component(loaded: &super::Vst3Loaded, flags: i32) {
    use vst3::Steinberg::Vst::{IComponentHandler, IComponentHandlerTrait};
    if let Some(handler) = loaded.host.handler.as_com_ref::<IComponentHandler>() {
        // SAFETY: as in `plugin_perform_edit`.
        unsafe { handler.restartComponent(flags) };
    }
}
