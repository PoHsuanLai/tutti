//! MIDI sync: the clock following the tempo map and the clock decoder.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[path = "../common/mod.rs"]
mod common;

mod clock_follower;
mod clock_tempo_map;
mod decoder_no_alloc;
mod machine_control;
