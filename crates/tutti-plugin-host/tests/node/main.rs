//! The generic plugin node: a chunked processor, and an in-process VST2 node's realtime safety and retirement.
//!
//! The binary's global allocator is armed (`AllocDisabler`), so its no-alloc gates bite.

// `assert_no_alloc` is inert unless `AllocDisabler` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod chunked_node;
mod vst2_in_process_no_alloc;
mod vst2_in_process_retire;
