//! Resolve the path to the reference CLAP plugin cdylib
//! (`tutti-clap-test-plugin`) and hand it to this crate's unit tests via the
//! `TUTTI_CLAP_TEST_PLUGIN_CANDIDATES` env var.
//!
//! ## Why this is duplicated from `tutti-clap-host`
//!
//! `tutti-clap-host/build.rs` emits the same variable, but `cargo:rustc-env`
//! applies only to the crate whose build script emitted it — it does not
//! propagate to dependents. This crate compiles its own tests, so it needs its
//! own emission. The resolution rules below are deliberately identical; keep
//! them in sync.
//!
//! ## Why we don't *build* the plugin here
//!
//! There is no stable Cargo mechanism to depend on a sibling *cdylib* artifact
//! from a test (artifact-dependencies are nightly-only), and shelling out to a
//! nested `cargo build` **deadlocks** against the outer `cargo test`'s
//! target-directory lock. Instead this crate takes `tutti-clap-test-plugin` as
//! a dev-dependency, so cargo builds the cdylib as part of the same `cargo
//! test` invocation and drops it under the profile directory. This script only
//! names the places it can land.
//!
//! ## Absence is a hard failure, not a skip
//!
//! The resolver panics when no candidate exists. The plugin is a dev-dependency
//! built by the same `cargo test` run, so its absence is a build failure, not a
//! property of the machine. The tests this replaced were the other failure mode
//! — they named an absolute macOS path and failed everywhere else.

use std::env;
use std::path::{Path, PathBuf};

const PLUGIN_LIB: &str = "tutti_clap_test_plugin";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_dir = resolve_target_dir().join(&profile);
    let name = lib_filename();

    // Cargo does not promise *where* under the profile dir a dev-dependency's
    // cdylib lands. With the default layout it is `<profile>/<name>`, but under
    // an explicit `CARGO_TARGET_DIR` (or `--target <triple>`) it has been
    // observed only in `<profile>/deps/<name>`. Emit both candidates so a
    // layout shift degrades into a slower lookup rather than a silently skipped
    // suite.
    //
    // `deps/` is listed first deliberately: cargo builds into `deps/` and
    // hardlinks the result up to `<profile>/`, but it does not always refresh
    // the copy, so `<profile>/` can be an *older build of the same plugin*.
    // The resolver takes the newest of the candidates that exist rather than
    // the first, because a stale-but-present artifact is the nastier failure —
    // the suite runs green against a plugin whose behaviour no longer matches
    // what the test expects.
    let candidates = [
        profile_dir.join("deps").join(&name),
        profile_dir.join(&name),
    ];
    let joined = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(";");
    println!("cargo:rustc-env=TUTTI_CLAP_TEST_PLUGIN_CANDIDATES={joined}");
}

/// The cdylib filename for the current platform.
fn lib_filename() -> String {
    if cfg!(target_os = "windows") {
        format!("{PLUGIN_LIB}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{PLUGIN_LIB}.dylib")
    } else {
        format!("lib{PLUGIN_LIB}.so")
    }
}

/// Resolve `<target-dir>` (the dir holding `debug/`, `release/`).
///
/// Honors `CARGO_TARGET_DIR` if set (this workspace points it at an external
/// SSD). Otherwise derives it from `OUT_DIR`, which is
/// `<target>/<profile>/build/<pkg>-<hash>/out` — the 5th ancestor.
fn resolve_target_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    out_dir
        .ancestors()
        .nth(4)
        .map(Path::to_path_buf)
        .unwrap_or(out_dir)
}
