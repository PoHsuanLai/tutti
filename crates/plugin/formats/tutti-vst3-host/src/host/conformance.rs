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

/// Install an observer for the current thread, replacing any previous one.
pub fn set_observer(f: Observer) {
    OBSERVER.with(|o| *o.borrow_mut() = Some(f));
}

/// Remove the current thread's observer.
pub fn clear_observer() {
    OBSERVER.with(|o| *o.borrow_mut() = None);
}

/// Hand the built `ProcessData` to the installed observer. Called by
/// `Vst3Instance::process` just before the plugin sees it.
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
