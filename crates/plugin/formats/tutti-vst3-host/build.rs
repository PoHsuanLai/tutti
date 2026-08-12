//! Compile Steinberg's HostChecker validation modules into a static lib the
//! `vst3_conformance` test links against, build the in-repo `audio-probe`
//! reference plugin, and locate any external sample plugins.
//!
//! Only runs when the `conformance` feature is on — which this crate turns on
//! for its own tests via a dev-dependency on itself, so `cargo test` needs no
//! flags. The SDK comes from the in-repo submodules; `VST3_SDK_DIR` overrides
//! for testing against another revision. See [`resolve_sdk`].
//!
//! Why the checker rather than hand-written assertions: HostChecker's six
//! check modules encode ~185 spec rules Steinberg accumulated over a decade of
//! host bugs. They depend only on the header-only `pluginterfaces`, so they
//! compile standalone — no VSTGUI, no `base` lib, no plugin bundle.
//!
//! ## The `audio-probe` bundle
//!
//! `audio-probe` is *ours*, not Steinberg's — ~1400 lines under
//! `tests/support/audio-probe/`. It used to live inside a VST3 SDK checkout at
//! a machine-specific path and be built by CMake, which made every test that
//! loads it unrunnable anywhere but one developer's box. It is built here
//! instead, so `cargo test` is the whole story.
//!
//! No CMake: the plugin plus the ~43 SDK translation units it needs are
//! compiled straight through `cc` and linked into a `.so` by hand. `cc` only
//! produces static libs, so the link step invokes the configured compiler
//! directly with `-shared` (see [`build_audio_probe`]).

use std::path::{Path, PathBuf};

/// The in-repo SDK, relative to this crate's manifest dir. Three git submodules
/// (`pluginterfaces`, `base`, `public.sdk`) pinned at `v3.8.0_build_66` — see
/// that directory's README for why the SDK superproject is not used directly.
const VENDORED_SDK: &str = "../../vendor/vst3-sdk";

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

    let sdk = resolve_sdk();

    // Independent of the hostchecker: the probe needs only the SDK's own
    // sources, so it is built even in a checkout whose samples are absent.
    build_audio_probe(&sdk);

    let src = sdk.join("public.sdk/samples/vst/hostchecker/source");
    if !src.is_dir() {
        println!(
            "cargo:warning=hostchecker sources not found under {}; conformance test will skip",
            src.display()
        );
        println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=0");
        return;
    }

    build_hostcheck(&sdk, &src);
    println!("cargo:rustc-env=VST3_HOSTCHECK_AVAILABLE=1");
}

/// The VST3 SDK to build against: the in-repo submodules by default,
/// `VST3_SDK_DIR` when someone wants a different SDK version.
///
/// ## Why this no longer degrades into a skip
///
/// It used to. `VST3_SDK_DIR` unset meant an empty `VST3_PROBE_DIR`, and the
/// suites that read it via `env!` then *panicked* — so `--features conformance`
/// on a machine without an SDK checkout was a hard failure dressed up as a
/// skip, and the tests that did skip cleanly were skipping for a reason nobody
/// could act on. The SDK is a submodule now, so it is present in any correctly
/// cloned tree and its absence has exactly one cause and one fix.
fn resolve_sdk() -> PathBuf {
    if let Some(dir) = std::env::var_os("VST3_SDK_DIR") {
        let dir = PathBuf::from(dir);
        assert!(
            dir.join("pluginterfaces/base/funknown.h").is_file(),
            "VST3_SDK_DIR={} does not look like a VST3 SDK checkout \
             (no pluginterfaces/ inside). Unset it to use the in-repo \
             submodule at {VENDORED_SDK}.",
            dir.display(),
        );
        return dir;
    }

    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(VENDORED_SDK);

    // An uninitialised submodule is an *empty directory*, not a missing one —
    // git creates the mount point either way — so this checks for a file
    // inside rather than for the directory. `is_dir()` here passes on a bare
    // clone and defers the failure to the per-source assert in
    // `build_audio_probe`, which reports a confusing "SDK source missing" for
    // one arbitrary `.cpp` instead of naming the actual cause.
    //
    // Naming the exact command matters more than it looks: this is the repo's
    // only submodule, so a contributor hitting it has no muscle memory for it.
    assert!(
        vendored.join("pluginterfaces/base/funknown.h").is_file(),
        "the vendored VST3 SDK at {} is empty — the submodules are not checked out.\n\
         Run:  git submodule update --init --recursive\n\
         (or set VST3_SDK_DIR to an external SDK checkout.)",
        vendored.display(),
    );
    vendored
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
            "VST3 SDK source missing: {} — the in-repo submodule is incomplete. \
             Run: git submodule update --init --recursive",
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

    // Same path, for *dependents*. `rustc-env` applies only to the crate whose
    // build script emitted it, so a dependent's tests cannot see the line
    // above; the `links` key in Cargo.toml turns this one into
    // `DEP_TUTTI_VST3_PROBE_DIR` in their build scripts. That is what lets
    // `tutti-plugin-server` find this bundle instead of rebuilding a second
    // copy of it — the probe is 90 lines of C++ compilation and there should be
    // exactly one. Cargo builds that name from the `links` value
    // (`tutti_vst3_probe`) plus this key, so the key is deliberately just `dir`.
    println!("cargo:dir={}", dir.display());
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
