//! [`Guard`]: the panic guard of an input's driver callback.
//!
//! CoreMIDI calls the input callback on its own thread, through an Objective-C
//! block: a panic that unwinds out of the closure unwinds through C frames
//! that are not built to be unwound, which is undefined behaviour or an abort
//! of the whole process (the safe `coremidi` crate does not catch it). The
//! pump of an ALSA input is a Rust thread of ours, where a panic only ends the
//! thread, but it ends it with the input still held and reading as alive. So
//! both run their body through a guard: a panic is caught at the callback's
//! edge, logged, and marks the input dead
//! ([`InputConnection::is_alive`](crate::InputConnection::is_alive) reports
//! it, so the session retires the device as it does one whose reader failed),
//! and every call after it does nothing: what the body held when it panicked
//! (the producer's inlet) may be half-written.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Runs a driver callback's body so that a panic stops at the callback.
#[derive(Clone, Debug)]
pub(crate) struct Guard {
    died: Arc<AtomicBool>,
}

impl Guard {
    /// A guard that sets `died` when a body panics.
    pub(crate) fn new(died: Arc<AtomicBool>) -> Self {
        Self { died }
    }

    /// Runs `body`, unless an earlier one panicked. A panic is caught, logged
    /// and marks the input dead; the result is `None` for it and for a call
    /// made after it.
    pub(crate) fn run<R>(&self, body: impl FnOnce() -> R) -> Option<R> {
        if self.died.load(Ordering::Acquire) {
            return None;
        }
        match catch_unwind(AssertUnwindSafe(body)) {
            Ok(result) => Some(result),
            Err(_) => {
                tracing::error!("a MIDI input callback panicked; the input is stopped");
                self.died.store(true, Ordering::Release);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
