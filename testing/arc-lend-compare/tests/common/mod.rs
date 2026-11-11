//! What the test files share: a handshake that is safe inside a counted
//! window.

use std::sync::atomic::{AtomicU8, Ordering};

/// A handshake that neither allocates nor frees on any platform: one thread
/// [`reach`](Step::reach)es a step, the other [`wait`](Step::wait)s for it.
#[derive(Default)]
pub struct Step(AtomicU8);

impl Step {
    pub fn reach(&self, n: u8) {
        self.0.store(n, Ordering::Release);
    }
    pub fn wait(&self, n: u8) {
        while self.0.load(Ordering::Acquire) < n {
            std::hint::spin_loop();
        }
    }
}
