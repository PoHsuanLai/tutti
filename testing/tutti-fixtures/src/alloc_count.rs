//! A counting allocator for the suites that count allocations instead of
//! gating them with `assert_no_alloc` (a crate that carries no such
//! dev-dependency).
//!
//! The binary names the static, because a `#[global_allocator]` must stay in
//! the binary it counts:
//!
//! ```ignore
//! #[global_allocator]
//! static A: tutti_fixtures::ThreadCounting = tutti_fixtures::ThreadCounting;
//! ```
//!
//! and reads a count with [`allocations`]. The count is per thread, so the
//! harness's other threads do not show up in it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: an allocation during thread teardown finds no slot.
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

/// The system allocator, counting each allocation and reallocation on the
/// calling thread.
pub struct ThreadCounting;

// SAFETY: every method forwards to `System` unchanged; the counter is the only
// addition.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for ThreadCounting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Allocations `f` made on this thread. Counts only where the binary installed
/// [`ThreadCounting`] as its global allocator.
pub fn allocations(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}
