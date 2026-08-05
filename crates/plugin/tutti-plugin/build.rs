//! Compute where Cargo will drop the reference VST2 plugin cdylib
//! (`tutti-vst2-test-plugin`) and hand the candidate paths to the integration
//! tests via `TUTTI_VST2_PROBE_DIR` / `TUTTI_VST2_PROBE_LIB`.
//!
//! The same computation `tutti-vst2-host/build.rs` does, and for the same
//! reasons: artifact-dependencies are nightly-only, and a nested `cargo build`
//! deadlocks against the target-directory lock the outer `cargo test` holds, so
//! the probe is a dev-dependency Cargo builds within this invocation. It emits
//! the profile *directory* rather than one path because Cargo writes the cdylib
//! to `<profile>/deps/<name>` and hardlinks it up without always refreshing the
//! uplifted copy; `tests/support/probe_path.rs` picks the newest by mtime.
//!
//! Duplicated rather than shared: a build script cannot be `include!`d across a
//! package boundary without a third crate whose only content is this file, and
//! the values it emits are per-package (`OUT_DIR` differs). The two must agree
//! on the two env-var names and the cdylib filename, which is all they share.

use std::env;
use std::path::{Path, PathBuf};

/// Crate name with the `-`→`_` substitution Cargo applies to lib artifacts.
const PROBE_LIB: &str = "tutti_vst2_test_plugin";

fn main() {
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
