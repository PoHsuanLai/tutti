//! What the test files share: a handshake that is safe inside a counted
//! window.

use std::sync::atomic::{AtomicU8, Ordering};

/// A handshake that neither allocates nor frees on any platform: one thread
/// [`reach`](Step::reach)es a step, the other [`wait`](Step::wait)s for it.
///
/// Mutation: make `reach` allocate (`drop(black_box(Box::new(n)))` before the
/// store), as a lazily boxed barrier does on macOS.
/// `an_arc_lend_loan_frees_nothing_on_the_reader` then reads
/// `Counts { allocs: 1, frees: 1, .. }`,
/// `a_promoted_arc_swap_guard_frees_on_the_reader` reads `allocs: 1, frees: 3`
/// and `another_threads_allocations_are_not_counted` reads `allocs: 1`.
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
