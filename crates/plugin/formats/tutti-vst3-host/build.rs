//! Compile Steinberg's HostChecker validation modules into a static lib the
//! `vst3_conformance` test links against, build the in-repo `audio-probe`
//! reference plugin, and locate any external sample plugins.
//!
//! Only runs when the `conformance` feature is on. The VST3 SDK checkout is
//! located via `VST3_SDK_DIR`; if it is unset or missing, nothing is built and
//! the tests skip themselves at runtime.
//!
//! Why the checker rather than hand-written assertions: HostChecker's six
//! check modules encode ~185 spec rules Steinberg accumulated over a decade of
//! host bugs. They depend only on the header-only `pluginterfaces`, so they
//! compile standalone — no VSTGUI, no `base` lib, no plugin bundle.
//!
//! ## The `audio-probe` bundle
//!
//! `audio-probe` is *ours*, not Steinberg's — 693 lines under
//! `tests/support/audio-probe/`. It used to live inside a VST3 SDK checkout at
//! a machine-specific path and be built by CMake, which made every test that
//! loads it unrunnable anywhere but one developer's box. It is built here
//! instead, so `cargo test --features conformance` is the whole story.
//!
//! No CMake: the plugin plus the 40 SDK translation units it needs are compiled
//! straight through `cc` and linked into a `.so` by hand. `cc` only produces
//! static libs, so the link step invokes the configured compiler directly with
//! `-shared` (see [`build_audio_probe`]).

use std::path::{Path, PathBuf};

const CHECK_MODULES: &[&str] = &[
    "hostcheck",
    "eventlistcheck",
    "parameterchangescheck",
    "processcontextcheck",
    "processsetupcheck",
    "eventlogger",
];

/// The probe's own translation units, under `tests/support/audio-probe/source`.
const PROBE_SOURCES: &[&str] = &["probeprocessor.cpp", "probecontroller.cpp", "factory.cpp"];

/// SDK translation units the probe links against, relative to `VST3_SDK_DIR`.
///
/// This is the exact set CMake puts in `libsdk.a` + `libsdk_common.a` +
/// `libbase.a` + `libpluginterfaces.a`, minus the platform-specific members
/// added by [`platform_sources`]. Listed explicitly rather than globbed: a glob
/// would silently pick up the Windows/macOS variants sitting in the same
/// directories, and a missing entry should be a named link error, not a
/// mystery.
const SDK_SOURCES: &[&str] = &[
    // pluginterfaces
    "pluginterfaces/base/conststringtable.cpp",
    "pluginterfaces/base/coreiids.cpp",
    "pluginterfaces/base/funknown.cpp",
    "pluginterfaces/base/ustring.cpp",
    // base
    "base/source/baseiids.cpp",
    "base/source/fbuffer.cpp",
    "base/source/fdebug.cpp",
    "base/source/fdynlib.cpp",
    "base/source/fobject.cpp",
    "base/source/fstreamer.cpp",
    "base/source/fstring.cpp",
    "base/source/timer.cpp",
    "base/source/updatehandler.cpp",
    "base/thread/source/fcondition.cpp",
    "base/thread/source/flock.cpp",
    // public.sdk — common
    "public.sdk/source/common/commoniids.cpp",
    "public.sdk/source/common/commonstringconvert.cpp",
    "public.sdk/source/common/openurl.cpp",
    "public.sdk/source/common/pluginview.cpp",
    "public.sdk/source/common/readfile.cpp",
    // public.sdk — main
    "public.sdk/source/main/moduleinit.cpp",
    "public.sdk/source/main/pluginfactory.cpp",
    // public.sdk — vst
    "public.sdk/source/vst/utility/dataexchange.cpp",
    "public.sdk/source/vst/utility/stringconvert.cpp",
    "public.sdk/source/vst/utility/systemtime.cpp",
    "public.sdk/source/vst/utility/testing.cpp",
    "public.sdk/source/vst/utility/vst2persistence.cpp",
    "public.sdk/source/vst/vstaudioeffect.cpp",
    "public.sdk/source/vst/vstbus.cpp",
    "public.sdk/source/vst/vstcomponent.cpp",
    "public.sdk/source/vst/vstcomponentbase.cpp",
    "public.sdk/source/vst/vsteditcontroller.cpp",
    "public.sdk/source/vst/vstinitiids.cpp",
    "public.sdk/source/vst/vstnoteexpressiontypes.cpp",
    "public.sdk/source/vst/vstparameters.cpp",
    "public.sdk/source/vst/vstpresetfile.cpp",
    "public.sdk/source/vst/vstrepresentation.cpp",
];

/// Per-OS SDK sources: the module entry point and the platform services.
///
/// The entry point is what exports `ModuleEntry`/`ModuleExit` (Linux),
/// `bundleEntry`/`bundleExit` (macOS) or `InitDll`/`ExitDll` (Windows) — the
/// symbols the host's module loader calls before `GetPluginFactory`.
fn platform_sources() -> &'static [&'static str] {
    if cfg!(target_os = "windows") {
        &[
            "public.sdk/source/main/dllmain.cpp",
            "public.sdk/source/common/systemclipboard_win32.cpp",
            "public.sdk/source/common/threadchecker_win32.cpp",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "public.sdk/source/main/macmain.cpp",
            "public.sdk/source/common/systemclipboard_mac.mm",
            "public.sdk/source/common/threadchecker_mac.mm",
        ]
    } else {
        &[
            "public.sdk/source/main/linuxmain.cpp",
            "public.sdk/source/common/systemclipboard_linux.cpp",
            "public.sdk/source/common/threadchecker_linux.cpp",
        ]
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=VST3_SDK_DIR");
    println!("cargo:rerun-if-env-changed=VST3_SAMPLE_PLUGIN_DIR");

    if std::env::var_os("CARGO_FEATURE_CONFORMANCE").is_none() {
        return;
    }

    // Forward the sample-plugin dir to the test, empty if unset.
    let plugin_dir = std::env::var("VST3_SAMPLE_PLUGIN_DIR").unwrap_or_default();
    println!("cargo:rustc-env=VST3_SAMPLE_PLUGIN_DIR={plugin_dir}");

    // Every `rustc-env` the tests read via `env!` must be emitted on *every*
    // path out of this function, including the failure paths. `env!` is
    // resolved at compile time, so an unset one is a build error rather than
    // the skip the test intends — which would make a checkout without the SDK
    // fail to build instead of skipping.
    let Some(sdk) = std::env::var_os("VST3_SDK_DIR").map(PathBuf::from) else {
        println!("cargo:warning=VST3_SDK_DIR unset; conformance test will skip");
        println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=0");
        println!("cargo:rustc-env=VST3_PROBE_DIR=");
        return;
    };

    // Independent of the hostchecker: the probe needs only the SDK's own
    // sources, so it is built even in a checkout whose samples are absent.
    build_audio_probe(&sdk);

    let src = sdk.join("public.sdk/samples/vst/hostchecker/source");
    if !src.is_dir() {
        println!("cargo:warning=hostchecker sources not found under VST3_SDK_DIR; test will skip");
        println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=0");
        return;
    }

    build_hostcheck(&sdk, &src);
    println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=1");
}

/// Build `tests/support/audio-probe` into a loadable `.vst3` bundle under
/// `OUT_DIR`, and export its parent directory as `VST3_PROBE_DIR`.
///
/// The bundle layout is the real one the host's loader walks
/// (`audio-probe.vst3/Contents/<arch-os>/audio-probe.so`), not a bare dylib —
/// the point of building it here is to exercise the same path a shipped plugin
/// takes.
///
/// Failures here are hard errors rather than warnings. A probe that silently
/// fails to build turns every test that loads it into a skip, and those tests
/// still print `ok` — the exact failure mode this whole change exists to remove.
fn build_audio_probe(sdk: &Path) {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let probe_src = PathBuf::from("tests/support/audio-probe/source");

    for f in PROBE_SOURCES {
        println!("cargo:rerun-if-changed={}", probe_src.join(f).display());
    }
    println!("cargo:rerun-if-changed=tests/support/audio-probe/source/probeids.h");
    println!("cargo:rerun-if-changed=tests/support/audio-probe/source/probeprocessor.h");
    println!("cargo:rerun-if-changed=tests/support/audio-probe/source/probecontroller.h");

    // `projectversion.h` is generated by the SDK's CMake from a template, so a
    // plain `cc` build has to supply it. Only the version macros matter; the
    // probe's `version.h` includes it for FULL_VERSION_STR.
    let gen = out_dir.join("probe-gen");
    std::fs::create_dir_all(&gen).expect("create probe-gen dir");
    std::fs::write(
        gen.join("projectversion.h"),
        "#pragma once\n\
         #define MAJOR_VERSION_STR \"3\"\n\
         #define MAJOR_VERSION_INT 3\n\
         #define SUB_VERSION_STR \"8\"\n\
         #define SUB_VERSION_INT 8\n\
         #define RELEASE_NUMBER_STR \"0\"\n\
         #define RELEASE_NUMBER_INT 0\n\
         #define BUILD_NUMBER_STR \"0\"\n\
         #define BUILD_NUMBER_INT 0\n\
         #define FULL_VERSION_STR \"3.8.0.0\"\n\
         #define VERSION_STR \"3.8.0\"\n",
    )
    .expect("write projectversion.h");

    // Compile to objects. `cc` can only emit a static lib, and a static lib is
    // the wrong shape here: the linker would drop every member nothing
    // references from *within the archive*, and for a plugin the entry points
    // are referenced only by the host at dlopen time. So objects are compiled
    // individually and all of them are fed to an explicit `-shared` link below.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(sdk)
        .include(&probe_src)
        .include(&gen)
        .define("RELEASE", "1")
        .define("NDEBUG", "1")
        // Steinberg's own sources; their warnings are not ours to fix.
        .warnings(false)
        .flag_if_supported("-Wno-multichar");

    for f in PROBE_SOURCES {
        build.file(probe_src.join(f));
    }
    for f in SDK_SOURCES.iter().chain(platform_sources()) {
        let path = sdk.join(f);
        assert!(
            path.is_file(),
            "VST3 SDK source missing: {} — is VST3_SDK_DIR pointing at a full checkout?",
            path.display()
        );
        build.file(path);
    }

    let objects = build.compile_intermediates();

    // Link the bundle. `get_compiler()` carries the same toolchain and target
    // flags `cc` just used, so cross-compiles and CC overrides are honoured.
    let bundle = out_dir
        .join("probe-bundle")
        .join("audio-probe.vst3")
        .join("Contents")
        .join(bundle_arch_dir());
    std::fs::create_dir_all(&bundle).expect("create bundle dir");
    let so = bundle.join(format!("audio-probe.{}", dylib_ext()));

    let compiler = build.get_compiler();
    let mut cmd = compiler.to_command();
    cmd.arg("-shared").arg("-fPIC").arg("-o").arg(&so);
    cmd.args(&objects);
    cmd.args(probe_link_args());

    let status = cmd.status().expect("failed to invoke the linker");
    assert!(status.success(), "linking audio-probe failed: {status}");
    assert!(so.is_file(), "linker reported success but {so:?} is absent");

    // The directory *containing* the bundle — the tests take a dir and join the
    // bundle name onto it, matching how an external plugin dir is passed.
    let dir = out_dir.join("probe-bundle");
    println!("cargo:rustc-env=VST3_PROBE_DIR={}", dir.display());
}

/// System libraries the probe's platform sources need at link time.
///
/// The SDK's per-OS sources in [`platform_sources`] are what pull these in:
/// macOS builds `systemclipboard_mac.mm` / `threadchecker_mac.mm` plus the
/// CoreFoundation calls in `fstring`/`timer`/`funknown` and the CoreAudio host
/// clock in `systemtime.cpp`, so the probe needs the Objective-C runtime and
/// four frameworks. On Linux the equivalents are `libstdc++fs`/`pthread`/`dl`.
/// Windows resolves its own through the MSVC defaults and needs nothing here.
fn probe_link_args() -> Vec<String> {
    if cfg!(target_os = "macos") {
        [
            "-framework",
            "CoreFoundation",
            "-framework",
            "Foundation",
            "-framework",
            "AppKit",
            "-framework",
            "CoreAudio",
            "-lobjc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    } else if cfg!(target_os = "windows") {
        Vec::new()
    } else {
        ["-lstdc++fs", "-lpthread", "-ldl"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

/// The per-platform subdirectory inside `Contents/` that the VST3 bundle spec
/// requires, e.g. `x86_64-linux`.
fn bundle_arch_dir() -> String {
    if cfg!(target_os = "macos") {
        return "MacOS".to_string();
    }
    // VST3 spells both x86_64 and aarch64 the way Rust does, so the target arch
    // passes through unchanged.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
    if cfg!(target_os = "windows") {
        format!("{arch}-win")
    } else {
        format!("{arch}-linux")
    }
}

fn dylib_ext() -> &'static str {
    if cfg!(target_os = "windows") {
        "vst3"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
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
