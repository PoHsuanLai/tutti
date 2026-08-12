//! Resolve the path to the reference CLAP plugin cdylib
//! (`tutti-clap-test-plugin`) and hand it to this crate's unit tests via the
//! `TUTTI_CLAP_TEST_PLUGIN_CANDIDATES` env var.
//!
//! ## Why the crate next door emits the same variable
//!
//! `cargo:rustc-env` applies only to the crate whose build script emitted it —
//! it does not propagate to dependents — and this crate compiles its own tests.
//! The rules used to be copy-pasted between the two with a "keep them in sync"
//! note; they are `tutti-fixture-resolve`'s now, so there is nothing left to
//! keep in sync.
//!
//! ## Why we don't *build* the plugin here
//!
//! There is no stable Cargo mechanism to depend on a sibling *cdylib* artifact
//! from a test (artifact-dependencies are nightly-only), and shelling out to a
//! nested `cargo build` **deadlocks** against the outer `cargo test`'s
//! target-directory lock. Instead this crate takes `tutti-clap-test-plugin` as
//! a dev-dependency, so cargo builds the cdylib as part of the same `cargo
//! test` invocation and drops it under the profile directory.
//!
//! The VST3 side is the exception: its reference plugin is C++ compiled by
//! `tutti-vst3-host`, so rather than rebuild it, that crate's `links` metadata
//! carries the location here. See [`forward_vst3_probe_dir`].

fn main() {
    tutti_fixture_resolve::emit_candidates(
        "TUTTI_CLAP_TEST_PLUGIN_CANDIDATES",
        "tutti_clap_test_plugin",
    );
    forward_vst3_probe_dir();
}

/// Re-export `tutti-vst3-host`'s reference-plugin directory so *this* crate's
/// tests can `env!` it.
///
/// `tutti-vst3-host` builds `audio-probe` (its own C++ reference plugin) and
/// emits `cargo:dir`. Because that crate declares `links`, cargo hands the
/// value to dependents' build scripts as `DEP_TUTTI_VST3_PROBE_DIR` — the only
/// supported way to cross a package boundary, since `rustc-env` does not
/// propagate. Rebuilding the probe here instead would mean a second copy of 90
/// lines of C++ compilation, and two bundles that could disagree.
///
/// Emits an empty string when absent, which happens on two legitimate paths: the
/// `vst3` feature is off (no dependency at all), or `tutti-vst3-host` was built
/// without `conformance` (no probe). `env!` resolves at compile time, so the
/// variable must exist on every path or the *build* fails rather than the test
/// reporting anything useful.
fn forward_vst3_probe_dir() {
    println!("cargo:rerun-if-env-changed=DEP_TUTTI_VST3_PROBE_DIR");
    let dir = std::env::var("DEP_TUTTI_VST3_PROBE_DIR").unwrap_or_default();
    println!("cargo:rustc-env=TUTTI_VST3_PROBE_DIR={dir}");
}
