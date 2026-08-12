//! Name the paths cargo can drop the reference VST2 plugin
//! (`tutti-vst2-test-plugin`) at, for this crate's tests to resolve.
//!
//! The plugin is a dev-dependency, so cargo builds the cdylib as part of the
//! same `cargo test` invocation and drops it under the profile directory. There
//! is no stable way to *ask* cargo where a sibling cdylib landed
//! (artifact-dependencies are nightly-only), and shelling out to a nested
//! `cargo build` deadlocks on the outer invocation's target-directory lock — so
//! the candidates are named here and resolved at test time.
//!
//! The logic is `tutti-fixture-resolve`'s, shared with the three other crates
//! that do this. It used to be copy-pasted into each, and had drifted; that
//! crate's docs carry the two rules that matter (absence panics, newest wins)
//! and what each cost when it was broken.

fn main() {
    tutti_fixture_resolve::emit_candidates("TUTTI_VST2_PROBE_CANDIDATES", "tutti_vst2_test_plugin");
}
