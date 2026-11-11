//! VST3 host conformance, correctness and robustness against the reference plugin (feature `conformance`).

#[cfg(feature = "conformance")]
#[path = "../support/conformance.rs"]
mod conformance_support;
// The one copy of `active`: `conformance_support` already loads it.
#[cfg(feature = "conformance")]
use conformance_support::active;

mod vst3_audio_correctness;
mod vst3_busy_list;
mod vst3_com_dispatch;
mod vst3_conformance_blocks;
mod vst3_conformance_controller;
mod vst3_conformance_interfaces;
mod vst3_conformance_modes;
mod vst3_misbehaving_plugin;
mod vst3_run_loop;
mod vst3_view_input;
mod vst3_view_teardown;
