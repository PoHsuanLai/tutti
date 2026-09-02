//! Name the paths cargo can drop this crate's test fixtures at, for its tests
//! to resolve.
//!
//! Two reference plugins and one binary:
//!
//! - `tutti-vst2-test-plugin` — the in-process VST2 suites' probe.
//! - `tutti-clap-test-plugin` — the CLAP probe the *out-of-process* correctness
//!   suites drive. It is loaded by the `plugin-server` subprocess, not by this
//!   process, which is what makes those tests exercise the real IPC path.
//! - `plugin-server` — the subprocess itself. Every out-of-process test points
//!   `TUTTI_PLUGIN_SERVER` at it, so the suite runs against the binary this same
//!   `cargo test` built rather than whatever happens to be on `PATH`.
//!
//! The plugins are dev-dependencies, so cargo builds the cdylibs as part of the
//! same `cargo test` invocation and drops them under the profile directory.
//! There is no stable way to *ask* cargo where a sibling cdylib landed
//! (artifact-dependencies are nightly-only), and shelling out to a nested
//! `cargo build` deadlocks on the outer invocation's target-directory lock — so
//! the candidates are named here and resolved at test time.
//!
//! The logic is `tutti-fixture-resolve`'s, shared with the three other crates
//! that do this. It used to be copy-pasted into each, and had drifted; that
//! crate's docs carry the two rules that matter (absence panics, newest wins)
//! and what each cost when it was broken.

use std::path::PathBuf;

fn main() {
    tutti_fixture_resolve::emit_candidates("TUTTI_VST2_PROBE_CANDIDATES", "tutti_vst2_test_plugin");
    tutti_fixture_resolve::emit_candidates(
        "TUTTI_CLAP_TEST_PLUGIN_CANDIDATES",
        "tutti_clap_test_plugin",
    );
    emit_plugin_server_candidates();
}

/// Name the paths cargo can drop the `plugin-server` **binary** at.
///
/// Not `emit_candidates`, which spells a *cdylib* filename (`lib…so`); a binary
/// has neither the `lib` prefix nor the shared-object extension. The directories
/// are the same two, and for the same reason: cargo writes to `<profile>/deps/`
/// and hardlinks up to `<profile>/`, without always refreshing the uplifted
/// copy, so both are offered and `resolve_or_panic` takes the newest.
///
/// `tutti-plugin-server` is **not** a dev-dependency of this crate and must not
/// become one — it depends on `tutti-plugin`, so the edge would be a cycle. The
/// binary is therefore built by the workspace rather than by this crate's own
/// `cargo test`, which is why the test-side resolver reports absence as "build
/// the server first" rather than as a build failure the way a missing cdylib is.
fn emit_plugin_server_candidates() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_dir = tutti_fixture_resolve::target_dir().join(profile);
    let name = if cfg!(target_os = "windows") {
        "plugin-server.exe"
    } else {
        "plugin-server"
    };
    let candidates: Vec<PathBuf> = vec![profile_dir.join("deps").join(name), profile_dir.join(name)];

    println!(
        "cargo:rustc-env=TUTTI_PLUGIN_SERVER_CANDIDATES={}",
        tutti_fixture_resolve::join_candidates(&candidates)
    );
}
