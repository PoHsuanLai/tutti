//! The reference CLAP plugin and the `plugin-server` that hosts it, for the
//! suites that load a real plugin (`plugin_capture`, `export_fork`).
//!
//! The plugin is a dev-dependency, so cargo builds its cdylib alongside the
//! test. The server is not (it would be a dependency cycle through
//! `tutti-plugin`), so it must be built first: `cargo build -p
//! tutti-plugin-server`. Both are looked up beside the test binary.

use std::path::PathBuf;

/// The built `plugin-server`. Panics, naming the command that builds it, when
/// there is none: a suite that needs it fails rather than skips.
pub fn plugin_server() -> PathBuf {
    tutti_fixture_resolve::plugin_server()
}

/// The reference CLAP cdylib, published under a `.clap` name: the server picks
/// the loader by extension, and reads a bare `.so` as VST2.
///
/// `tutti-plugin`'s suites publish the same link from their own processes;
/// `tutti_fixture_resolve::publish_with_extension` says how it is shared.
pub fn clap_probe() -> PathBuf {
    let real = tutti_fixture_resolve::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    tutti_fixture_resolve::publish_with_extension(&real, "clap")
}

/// The reference VST2 cdylib (`tutti-vst2-test-plugin`), under its own name:
/// a bare library is what the in-process VST2 loader takes.
///
/// Used by `plugin_vst2_meter.rs` only; the crate's unit tests, which include
/// this file too, do not load a VST2.
#[allow(dead_code)]
pub fn vst2_probe() -> PathBuf {
    tutti_fixture_resolve::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin")
}
