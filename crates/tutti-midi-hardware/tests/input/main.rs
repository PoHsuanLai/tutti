//! The hardware MIDI input: the MIDI-1 wire into MIDI-CI, the input port polled on the audio thread, and the ALSA loopback.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[cfg(all(target_os = "linux", feature = "sequencer-tests"))]
mod alsa_loopback;
mod ci_wire;
mod hardware_poll_no_alloc;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    target_os = "macos"
))]
mod listener;
#[cfg(all(target_os = "linux", feature = "sequencer-tests"))]
mod virtual_endpoint;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    target_os = "macos"
))]
mod virtual_loopback;
