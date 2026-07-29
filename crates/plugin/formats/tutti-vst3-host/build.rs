//! Compile Steinberg's HostChecker validation modules into a static lib the
//! `vst3_conformance` test links against, and locate the sample plugins the
//! test loads.
//!
//! Only runs when the `conformance` feature is on. Both the VST3 SDK checkout
//! and the built sample plugins are located via env vars (`VST3_SDK_DIR`,
//! `VST3_SAMPLE_PLUGIN_DIR`); if either is unset or missing, nothing is built
//! and the test skips itself at runtime.
//!
//! Why the checker rather than hand-written assertions: HostChecker's six
//! check modules encode ~185 spec rules Steinberg accumulated over a decade of
//! host bugs. They depend only on the header-only `pluginterfaces`, so they
//! compile standalone — no VSTGUI, no `base` lib, no plugin bundle.

use std::path::{Path, PathBuf};

const CHECK_MODULES: &[&str] = &[
    "hostcheck",
    "eventlistcheck",
    "parameterchangescheck",
    "processcontextcheck",
    "processsetupcheck",
    "eventlogger",
];

fn main() {
    println!("cargo:rerun-if-env-changed=VST3_SDK_DIR");
    println!("cargo:rerun-if-env-changed=VST3_SAMPLE_PLUGIN_DIR");

    if std::env::var_os("CARGO_FEATURE_CONFORMANCE").is_none() {
        return;
    }

    // Forward the sample-plugin dir to the test, empty if unset.
    let plugin_dir = std::env::var("VST3_SAMPLE_PLUGIN_DIR").unwrap_or_default();
    println!("cargo:rustc-env=VST3_SAMPLE_PLUGIN_DIR={plugin_dir}");

    let Some(sdk) = std::env::var_os("VST3_SDK_DIR").map(PathBuf::from) else {
        println!("cargo:warning=VST3_SDK_DIR unset; conformance test will skip");
        println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=0");
        return;
    };

    let src = sdk.join("public.sdk/samples/vst/hostchecker/source");
    if !src.is_dir() {
        println!("cargo:warning=hostchecker sources not found under VST3_SDK_DIR; test will skip");
        println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=0");
        return;
    }

    build_hostcheck(&sdk, &src);
    println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=1");
}

fn build_hostcheck(sdk: &Path, src: &Path) {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(sdk)
        .include(src)
        .include("tests/support")
        // base/source/fdebug.h refuses to compile unless one of these is set.
        .define("RELEASE", "1")
        .define("NDEBUG", "1")
        // HostChecker is third-party; its warnings are not ours to fix.
        .warnings(false);

    for m in CHECK_MODULES {
        let f = src.join(format!("{m}.cpp"));
        println!("cargo:rerun-if-changed={}", f.display());
        build.file(f);
    }

    println!("cargo:rerun-if-changed=tests/support/hostcheck_shim.cpp");
    build.file("tests/support/hostcheck_shim.cpp");

    build.compile("hostcheck");
}
