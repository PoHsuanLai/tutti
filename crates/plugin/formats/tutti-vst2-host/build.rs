//! Compute where Cargo will drop the reference VST2 plugin cdylib
//! (`tutti-vst2-test-plugin`) and hand the candidate paths to the
//! integration tests via `TUTTI_VST2_PROBE_DIR` / `TUTTI_VST2_PROBE_LIB`.
//!
//! ## Why we don't *build* it here
//!
//! There is no stable Cargo mechanism to depend on a sibling *cdylib*
//! artifact from a test (artifact-dependencies are nightly-only). The
//! obvious workaround — shelling out to `cargo build -p
//! tutti-vst2-test-plugin` from this build script — **deadlocks**: the outer
//! `cargo test` already holds the target-directory lock for the whole build,
//! and the nested `cargo` blocks forever waiting for that same lock (this
//! workspace shares one external target dir across worktrees, so the lock is
//! always contended).
//!
//! Instead, `tutti-vst2-host` takes `tutti-vst2-test-plugin` as a
//! **dev-dependency** (it builds both `cdylib` + `rlib`). Cargo therefore
//! builds the cdylib as part of the *same* `cargo test` invocation — one
//! lock, no nested cargo. This script's only job is to compute the
//! deterministic directory; `tests/support/probe_path.rs` picks the newest
//! of the candidates at runtime and panics if there is none.
//!
//! ## Why the *directory* and not one path
//!
//! Cargo writes the cdylib to `<profile>/deps/<name>` and hardlinks it up to
//! `<profile>/<name>`, but does not always refresh the uplifted copy. Naming
//! a single path here would let a stale hardlink be loaded, which silently
//! reverses test results — see `probe_path.rs` for the incident. Emitting
//! the profile dir and letting the test compare mtimes keeps that decision
//! at runtime, where the timestamps exist.

use std::env;
use std::path::{Path, PathBuf};

/// Crate name with the `-`→`_` substitution Cargo applies to lib artifacts.
const PROBE_LIB: &str = "tutti_vst2_test_plugin";

fn main() {
    // Rerun if the probe's sources change — not strictly required for
    // correctness (the dev-dependency edge already forces a rebuild), but it
    // keeps the emitted env vars in step when the probe is edited.
    println!("cargo:rerun-if-changed=build.rs");

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_dir = resolve_target_dir().join(&profile);

    println!(
        "cargo:rustc-env=TUTTI_VST2_PROBE_DIR={}",
        profile_dir.display()
    );
    println!("cargo:rustc-env=TUTTI_VST2_PROBE_LIB={}", lib_filename());
}

/// The cdylib filename for the current platform.
fn lib_filename() -> String {
    if cfg!(target_os = "windows") {
        format!("{PROBE_LIB}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{PROBE_LIB}.dylib")
    } else {
        format!("lib{PROBE_LIB}.so")
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
