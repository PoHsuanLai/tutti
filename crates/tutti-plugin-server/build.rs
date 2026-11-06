//! Forward `tutti-vst3-host`'s reference-plugin directory to this crate's tests.
//!
//! The VST3 reference plugin is C++ compiled by `tutti-vst3-host`, so rather
//! than rebuild it, that crate's `links` metadata carries the location here.
//! See [`forward_vst3_probe_dir`].

fn main() {
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
