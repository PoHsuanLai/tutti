//! The hardware MIDI input: the MIDI-1 wire into MIDI-CI, and the input port polled on the audio thread.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod ci_wire;
mod hardware_poll_no_alloc;
