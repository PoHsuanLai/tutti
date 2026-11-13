//! The VST2 host against the shared traits, and its realtime safety, on the reference plugin.

#[path = "../support/active.rs"]
mod active;

// `assert_no_alloc` is inert unless `AllocDisabler` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod vst2_host_process_no_alloc;
mod vst2_traits;
