//! The mode and the transcript through the plugin-server, a separate process:
//! the mode goes in as state, the transcript comes out as state, a parameter
//! reads as a parameter.
//!
//! Needs `cargo build -p tutti-plugin-server --no-default-features --features
//! clap` first.

use tutti_plugin::BridgeConfig;
use tutti_plugin::handles::{ParamId, PluginClient, PluginHandle};
use tutti_plugin_contract::spec::{Mode, Param};
use tutti_plugin_contract::{Clap, Format, Transcript, clap_path, patch_mode};

// Mutation (run): the plugin saving a mode word of 0 fails it.
#[test]
fn clap_mode_and_transcript_cross_the_process_boundary() {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let client = PluginClient::new(BridgeConfig::default(), clap_path(), 48_000.0)
        .expect("the server loads the plugin");
    let handle = PluginHandle::from_client(&client);
    let pid = handle.server_pid().expect("the plugin lives in a server");
    assert_ne!(pid, std::process::id(), "a process other than this one");

    let state = handle.state().save_state().expect("saves");
    let blob = patch_mode(Clap::extract_blob(&state), Mode::Level);
    handle
        .state()
        .load_state(&Clap::embed_blob(&state, &blob))
        .expect("loads");

    let saved = handle.state().save_state().expect("saves again");
    let after = Transcript::parse(&Clap::extract_blob(&saved));
    assert_eq!(after.mode, Mode::Level.index(), "the mode crossed");
    let level = handle
        .params()
        .parameter_value(ParamId::new(Param::Level.id().get()))
        .map(|v| v.get());
    assert_eq!(level, Some(0.5), "a parameter read across the boundary");
}
