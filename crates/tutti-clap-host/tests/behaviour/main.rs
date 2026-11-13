//! What the CLAP host does with a plugin: audio, events, parameters, bundles and event translation.

#[path = "../support/mod.rs"]
mod support;

// `assert_no_alloc` is inert unless `AllocDisabler` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod clap_audio_correctness;
mod clap_audio_thread_claim_no_alloc;
mod clap_editor_instance;
mod clap_event_lists;
mod clap_midi_groups;
mod clap_multi_plugin_bundle;
mod clap_param_gain;
mod clap_ports_rescan;
mod clap_process_no_alloc;
mod clap_thread_pool;
mod clap_traits;
mod unit_tests;
