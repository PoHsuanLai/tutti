//! Bounce-time properties, the push-render path and the realtime safety of the host's audio-thread operations,
//! against the real corpus (macOS only).

#[cfg(target_os = "macos")]
#[path = "../support/mod.rs"]
mod support;

// `assert_no_alloc` is inert unless `AllocDisabler` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[cfg(target_os = "macos")]
mod au_offline;
#[cfg(target_os = "macos")]
mod au_process_no_alloc;
