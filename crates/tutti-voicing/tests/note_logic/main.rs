//! The note logic: tuning, Universal SysEx, and the allocation-free step.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod step_no_alloc;
mod support;
mod tuning;
mod universal;
