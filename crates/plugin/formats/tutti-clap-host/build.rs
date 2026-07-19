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
//! lock, no nested cargo — and drops it in the profile directory. This
//! script's only job is to compute that deterministic path. The test
//! checks the file exists at runtime and skips with a message if not.

use std::env;
use std::path::{Path, PathBuf};

const PLUGIN_LIB: &str = "tutti_clap_test_plugin";

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let target_dir = resolve_target_dir();
    let artifact = target_dir.join(&profile).join(lib_filename());
    // Emit the expected path unconditionally; the cdylib is produced by the
    // dev-dependency during this same `cargo test` run. The test verifies
    // existence at load time.
    println!("cargo:rustc-env=TUTTI_CLAP_TEST_PLUGIN={}", artifact.display());
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
