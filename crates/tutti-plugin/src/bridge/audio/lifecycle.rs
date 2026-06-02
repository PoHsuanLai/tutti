//! Shared running/crashed flags between the audio-bridge handle and the
//! bridge thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Lifecycle {
    running: Arc<AtomicBool>,
    crashed: Arc<AtomicBool>,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            crashed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::Acquire)
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub(super) fn mark_crashed(&self) {
        self.crashed.store(true, Ordering::Release);
    }

    pub(super) fn request_shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
