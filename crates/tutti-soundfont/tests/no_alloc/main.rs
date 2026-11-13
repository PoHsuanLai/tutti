//! The soundfont's realtime paths allocate nothing.

#[path = "../support/mod.rs"]
mod support;

mod rt_no_alloc;

/// Aborts inside an `assert_no_alloc` region; inert elsewhere.
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
