//! The mode and the transcript through the plugin-server, a separate process:
//! the mode goes in as state, the transcript comes out as state, a parameter
//! reads as a parameter. The server picks the format from the path's
//! extension, so VST3 goes by its `.vst3` bundle.
//!
//! Needs `cargo build -p tutti-plugin-server --no-default-features --features
//! clap,vst2,vst3` first.

use std::path::PathBuf;

use tutti_plugin::BridgeConfig;
use tutti_plugin::handles::{ParamId, PluginClient, PluginHandle};
use tutti_plugin_contract::spec::{Mode, Param};
use tutti_plugin_contract::{
    Clap, Format, Transcript, Vst2, Vst3, Vst3Cpp, bundle_of, clap_path, library, patch_mode,
    vst3_cpp_module, vst3_module,
};

fn through_server<F: Format>(path: PathBuf) {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let client = PluginClient::new(
        BridgeConfig::default(),
        path,
        tutti_plugin_types::SampleRate::new(48_000.0),
    )
    .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
    let handle = PluginHandle::from_client(&client);
    let pid = handle.server_pid().expect("the plugin lives in a server");
    assert_ne!(pid, std::process::id(), "a process other than this one");

    let state = handle.state().save_state().expect("saves");
    let blob = patch_mode(F::extract_blob(&state), Mode::Level);
    handle
        .state()
        .load_state(&main_thread(), &F::embed_blob(&state, &blob))
        .expect("loads");

    let saved = handle.state().save_state().expect("saves again");
    let after = Transcript::parse(&F::extract_blob(&saved));
    assert_eq!(after.mode, Mode::Level.index(), "the mode crossed");
    let level = handle
        .params()
        .parameter_value(ParamId::new(Param::Level.id().get()))
        .map(|v| v.get());
    assert_eq!(level, Some(0.5), "a parameter read across the boundary");
}

// Mutation (run): the plugin saving a mode word of 0 fails all four.
#[test]
fn clap_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Clap>(clap_path());
}

#[test]
fn vst2_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst2>(library());
}

#[test]
fn vst3_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}
