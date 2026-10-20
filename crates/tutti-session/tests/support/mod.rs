//! The reference CLAP plugin and the `plugin-server` that hosts it, for the
//! plugin suite and the §5.2 example (`crates/tutti/examples`, which includes
//! this file by path).
//!
//! The plugin is a dev-dependency, so cargo builds its cdylib alongside the
//! test. The server is not (it would be a dependency cycle through
//! `tutti-plugin`), so it must be built first: `cargo build -p
//! tutti-plugin-server`. Both are looked up beside the running binary
//! (`<profile>/deps/` for a test, `<profile>/examples/` for an example).

use std::path::{Path, PathBuf};

/// `<target>/<profile>`: the directory this binary's `deps/` (or
/// `examples/`) sits in.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the binary has a path");
    exe.parent()
        .and_then(Path::parent)
        .expect("the binary lives in <profile>/deps or <profile>/examples")
        .to_path_buf()
}

/// The newest of `<profile>/deps/<name>` and `<profile>/<name>`: cargo writes
/// the first and uplifts to the second without always refreshing it.
fn beside_this_binary(name: &str) -> Option<PathBuf> {
    let profile = profile_dir();
    let candidates = [
        profile
            .join("deps")
            .join(name)
            .to_string_lossy()
            .into_owned(),
        profile.join(name).to_string_lossy().into_owned(),
    ];
    tutti_fixture_resolve::newest_existing(candidates.iter().map(String::as_str)).map(PathBuf::from)
}

/// The built `plugin-server`. Panics, naming the command that builds it,
/// when there is none: a suite that needs it fails rather than skips.
pub fn plugin_server() -> PathBuf {
    let name = if cfg!(windows) {
        "plugin-server.exe"
    } else {
        "plugin-server"
    };
    beside_this_binary(name).unwrap_or_else(|| {
        panic!(
            "`plugin-server` not found under {}. It is not a dev-dependency (that \
             would be a cycle through `tutti-plugin`), so build it first:\n\n  \
             cargo build -p tutti-plugin-server\n",
            profile_dir().display()
        )
    })
}

/// The reference CLAP cdylib, published under a `.clap` name: the server
/// picks the loader by extension, and reads a bare `.so` as VST2.
pub fn clap_probe() -> PathBuf {
    let lib = tutti_fixture_resolve::lib_filename("tutti_clap_test_plugin");
    let real = beside_this_binary(&lib).unwrap_or_else(|| {
        panic!(
            "the reference plugin `{lib}` is a dev-dependency built by this same \
             run, but is not under {}",
            profile_dir().display()
        )
    });
    tutti_fixture_resolve::publish_with_extension(&real, "clap")
}
