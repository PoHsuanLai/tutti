//! The VST3 host against real and reference plugins: loading, event lists, MIDI groups and the FFI plugin.

#[path = "../support/active.rs"]
mod active;

// Every test target is declared because `autotests = false` (see above);
// a new file under tests/ needs an entry here or it will not run.
mod integration_tests;
mod vst3_event_lists;
mod vst3_ffi_plugin;
mod vst3_midi_groups;
