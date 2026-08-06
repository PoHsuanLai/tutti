//! End-to-end test of the in-process VST2 backend.
//!
//! Requires TAL-NoiseMaker installed at the standard macOS path. CI
//! doesn't ship plugins, so every plugin-touching test is `#[ignore]`'d
//! and only runs under `cargo test -- --ignored`.

#![cfg(feature = "vst2")]

use std::path::Path;
use std::sync::Mutex;

use tutti_plugin::server::EditorPresence;
use tutti_plugin_types::ParamAddress;

const VST2_PLUGIN: &str = "/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst";

/// VST2 addresses parameters by position, so every id here is an `Index`.
const PARAM_0: ParamAddress = ParamAddress::Index(0);

/// Serialize plugin loads — racing two concurrent VSTPluginMain calls
/// against the same library makes some plugins crash.
static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

#[test]
#[ignore]
fn load_in_process_returns_audio_unit_and_handle() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (_unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");

    let descriptor = handle.descriptor();
    assert!(!descriptor.name.is_empty());
    assert_eq!(handle.loaded().total_outputs(), 2);
    assert_eq!(descriptor.editor, EditorPresence::Present);
}

#[test]
#[ignore]
fn handle_parameter_roundtrip() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (_unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");

    let params = handle.parameters().expect("params should be Some");
    assert!(!params.is_empty());

    handle.set_parameter(PARAM_0, 0.5);
    let v = handle.parameter(PARAM_0).expect("param 0 should exist");
    assert!((v - 0.5).abs() < 0.01, "expected ~0.5, got {v}");
}

#[test]
#[ignore]
fn handle_state_roundtrip() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (_unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");

    handle.set_parameter(PARAM_0, 0.25);
    let state = handle.save_state().expect("save_state should be Some");
    assert!(!state.is_empty());

    handle.set_parameter(PARAM_0, 0.9);
    handle.load_state(&state);

    let restored = handle.parameter(PARAM_0).expect("param 0 should exist");
    assert!(
        (restored - 0.25).abs() < 0.02,
        "expected ~0.25 after restore, got {restored}"
    );
}

#[test]
#[ignore]
fn handle_is_not_crashed_in_process() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (_unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");
    assert!(!handle.is_crashed());
}

#[test]
#[ignore]
fn vst2_builder_routes_to_in_process() {
    // The public `tutti_plugin::vst2(...)` builder should detect a `.vst`
    // path and route to the in-process backend (with the `vst2` feature
    // on, which the integration test gate requires). Confirm by checking
    // has_editor reports true and no crash.
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (_unit, handle) = tutti_plugin::vst2(48_000.0, VST2_PLUGIN)
        .build()
        .expect("vst2() builder should load TAL-NoiseMaker in-process");

    assert!(!handle.is_crashed());
    assert!(
        handle.has_editor(),
        "in-process backend should report has_editor"
    );
}

#[test]
#[ignore]
fn handle_midi_sender_available() {
    // PR E migration: midi_sender is on PluginHandle (not just on the
    // concrete PluginClient), so callers using Plugins::load through
    // the catalog can still send MIDI events into the plugin.
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (_unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");
    let _sender = handle.midi_sender();
    // Cloning is cheap and the sender outlives the function — that's
    // the surface contract we care about.
}
