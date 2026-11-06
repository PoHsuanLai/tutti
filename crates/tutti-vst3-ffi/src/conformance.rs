//! The observation seam the host-conformance tests use: a closure that sees
//! the fully built `ProcessData` just before `IAudioProcessor::process`.

use std::cell::RefCell;

use vst3::Steinberg::Vst::{ProcessData, ProcessSetup};

/// What a conformance test installs: it is shown the assembled `ProcessData`
/// and the `ProcessSetup` the plugin was given.
pub type Observer = Box<dyn FnMut(&ProcessData, &ProcessSetup)>;

thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

/// Installs `f` as this thread's observer.
pub fn set_observer(f: Observer) {
    OBSERVER.with(|o| *o.borrow_mut() = Some(f));
}

/// Removes this thread's observer.
pub fn clear_observer() {
    OBSERVER.with(|o| *o.borrow_mut() = None);
}

pub(crate) fn observe(data: &ProcessData, setup: &ProcessSetup) {
    OBSERVER.with(|o| {
        if let Ok(mut slot) = o.try_borrow_mut()
            && let Some(f) = slot.as_mut()
        {
            f(data, setup);
        }
    });
}
