//! Resolve the path to the reference CLAP plugin cdylib
//! (`tutti-clap-test-plugin`) and hand it to the `clap_conformance`
//! integration test via the `TUTTI_CLAP_TEST_PLUGIN` env var.
//!
//! ## Why we don't *build* it here
//!
//! There is no stable Cargo mechanism to depend on a sibling *cdylib*
//! artifact from a test (artifact-dependencies are nightly-only). The
//! obvious workaround — shelling out to `cargo build -p
//! tutti-clap-test-plugin` from this build script — **deadlocks**: the
//! outer `cargo test` already holds the target-directory lock for the whole
//! build, and the nested `cargo` blocks forever waiting for that same lock
//! (this workspace shares one external target dir across worktrees, so the
//! lock is always contended). See `.cargo/config.toml`.
//!
//! Instead, `tutti-clap-host` takes `tutti-clap-test-plugin` as a
//! **dev-dependency** (it builds both `cdylib` + `rlib`). Cargo therefore
//! builds the cdylib as part of the *same* `cargo test` invocation — one
//! lock, no nested cargo — and drops it under the profile directory. This
//! script's only job is to name the places it can land.
//!
//! ## Absence is a hard failure, not a skip
//!
//! The tests **panic** when no candidate resolves. The plugin is a
//! dev-dependency built by the same `cargo test` run, so its absence is a
//! build failure, not a property of the machine.
//!
//! This is not hypothetical. This script used to emit a single guessed path
//! and the tests skipped when it was missing — so under an isolated
//! `CARGO_TARGET_DIR`, where cargo wrote the cdylib to `<profile>/deps/`
//! only, all 55 integration tests reported `ok` having executed nothing.
//! Any change here must keep a missing plugin loud.

use std::env;
use std::path::{Path, PathBuf};

const PLUGIN_LIB: &str = "tutti_clap_test_plugin";

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_dir = resolve_target_dir().join(&profile);
    let name = lib_filename();

    // Cargo does not promise *where* under the profile dir a dev-dependency's
    // cdylib lands. With the default layout it is `<profile>/<name>`, but under
    // an explicit `CARGO_TARGET_DIR` (or `--target <triple>`) it has been
    // observed only in `<profile>/deps/<name>`. Emit both candidates so a
    // layout shift degrades into a slower lookup rather than a silently
    // skipped suite.
    //
    // `deps/` is listed first deliberately: cargo builds into `deps/` and
    // hardlinks the result up to `<profile>/`, but it does not always refresh
    // the copy, so `<profile>/` can be an *older build of the same plugin*.
    // The resolver takes the newest of the candidates that exist rather than
    // the first, because a stale-but-present artifact is the nastier failure —
    // the suite runs green against a plugin whose behaviour switches no longer
    // match the test's expectations. That is not hypothetical: it silently
    // reversed one mutation-verification result during this crate's RT work.
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
/// Honors `CARGO_TARGET_DIR` if set (this workspace points it at an
/// external SSD). Otherwise derives it from `OUT_DIR`, which is
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
