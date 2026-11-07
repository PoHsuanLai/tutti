//! Builds the C++ comparison plugin: the SDK's translation units plus this
//! plugin's own, linked into `shared-vst3-cpp.vst3`. The SDK recipe is the one
//! `tutti-vst3-host/build.rs` uses for its reference plugins (copied, not shared).

use std::path::{Path, PathBuf};

const VENDORED_SDK: &str = "../../vendor/vst3-sdk";

/// Whether the crate is being built *for* `os`.
///
/// Not `cfg!(target_os = ..)`: inside a build script that names the OS the
/// script itself runs on, the host. Every per-OS choice here (SDK sources, link
/// flags, bundle layout, module extension) is about the plugin being built for
/// the target, so a cross-build that asked `cfg!` compiled the host's platform
/// sources into a target-OS plugin.
fn target_os_is(os: &str) -> bool {
    std::env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS for build scripts")
        == os
}

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
    if target_os_is("windows") {
        &[
            "public.sdk/source/main/dllmain.cpp",
            "public.sdk/source/common/systemclipboard_win32.cpp",
            "public.sdk/source/common/threadchecker_win32.cpp",
        ]
    } else if target_os_is("macos") {
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

/// Compile the SDK's own translation units and return the objects.
///
/// Separate from any one plugin's build so more than one bundle can link them.
/// This is the expensive half — ~43 files — and compiling it per plugin would
/// double the build for nothing.
fn compile_sdk_objects(sdk: &Path, generated_include: &Path) -> Vec<PathBuf> {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(sdk)
        .include(generated_include)
        .define("RELEASE", "1")
        .define("NDEBUG", "1")
        // Steinberg's own sources; their warnings are not ours to fix.
        .warnings(false)
        .flag_if_supported("-Wno-multichar");

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
    build.compile_intermediates()
}

/// Link `objects` into a real `.vst3` bundle named `plugin` under `dir`.
///
/// The bundle layout is the one the host's loader walks
/// (`<plugin>.vst3/Contents/<arch-os>/<plugin>.so`), not a bare dylib — the
/// point of building here is to exercise the same path a shipped plugin takes.
///
/// `cc` can only emit a static lib, and that is the wrong shape: the linker
/// drops archive members nothing references, and a plugin's entry points are
/// referenced only by the host at load time. So this invokes the compiler
/// directly. Failures are hard errors — a plugin that silently fails to build
/// turns every test that loads it into a skip.
///
/// # Two flag dialects
///
/// The GCC/Clang spelling (`-shared -fPIC -o`) is not portable to MSVC, whose
/// `cl` rejects `-shared` and `-fPIC` outright and treats `-o` as deprecated —
/// which is how this build script failed on Windows for as long as nobody ran
/// it there. Everything else in this file already branched on the target;
/// only the link step did not.
///
/// The MSVC spelling is `/LD` (emit a DLL), `/Fe:` (name the output), and
/// `/link` to pass the rest through to `link.exe`. There is no `-fPIC`
/// counterpart and none is needed: Windows code is position-independent by
/// construction, and the export side is handled in the sources by the SDK's
/// `SMTG_EXPORT_SYMBOL` (`__declspec(dllexport)`) rather than by a flag.
fn link_bundle<'a>(
    build: &cc::Build,
    objects: impl Iterator<Item = &'a PathBuf>,
    dir: &Path,
    plugin: &str,
) {
    let contents = dir
        .join(format!("{plugin}.vst3"))
        .join("Contents")
        .join(bundle_arch_dir());
    std::fs::create_dir_all(&contents).expect("create bundle dir");
    let so = contents.join(format!("{plugin}.{}", dylib_ext()));

    // `get_compiler()` carries the same toolchain and target flags `cc` used,
    // so cross-compiles and CC overrides are honoured.
    let compiler = build.get_compiler();
    let objects: Vec<&PathBuf> = objects.collect();
    let mut cmd = compiler.to_command();
    if compiler.is_like_msvc() {
        // No `.ilk`: an incremental-link file would sit in the bundle's arch
        // directory beside the module.
        cmd.arg("/nologo").arg("/LD");
        cmd.args(&objects);
        cmd.arg(format!("/Fe:{}", so.display()));
        cmd.arg("/link").arg("/INCREMENTAL:NO");
        // Keep the import library and its export file out of the bundle. MSVC
        // writes both beside the DLL by default, and a `.vst3` bundle is a
        // directory a host *scans* — anything in the arch dir is a candidate
        // module. They are build artifacts, so they belong in OUT_DIR.
        let implib =
            PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join(format!("{plugin}.lib"));
        cmd.arg(format!("/IMPLIB:{}", implib.display()));
        cmd.args(plugin_link_args());
    } else {
        cmd.arg("-shared").arg("-fPIC").arg("-o").arg(&so);
        cmd.args(&objects);
        cmd.args(plugin_link_args());
    }

    let status = cmd.status().expect("failed to invoke the linker");
    assert!(status.success(), "linking {plugin} failed: {status}");
    assert!(so.is_file(), "linker reported success but {so:?} is absent");
}

/// System libraries the plugin's platform sources need at link time.
///
/// The SDK's per-OS sources in [`platform_sources`] are what pull these in:
/// macOS builds `systemclipboard_mac.mm` / `threadchecker_mac.mm` plus the
/// CoreFoundation calls in `fstring`/`timer`/`funknown` and the CoreAudio host
/// clock in `systemtime.cpp`, so the plugin needs the Objective-C runtime and
/// four frameworks. On Linux the equivalents are `libstdc++fs`/`pthread`/`dl`.
/// Windows resolves its own through the MSVC defaults and needs nothing here.
fn plugin_link_args() -> Vec<String> {
    if target_os_is("macos") {
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
    } else if target_os_is("windows") {
        // `dllmain.cpp` calls `CoInitialize`, and `systemclipboard_win32.cpp`
        // the clipboard API, so the SDK's platform sources pull these in. They
        // are not optional on the `/link` line: with `/LD` the compiler driver
        // passes only the default libs, and these are not among them.
        ["ole32.lib", "user32.lib", "shell32.lib"]
            .iter()
            .map(|s| s.to_string())
            .collect()
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
    if target_os_is("macos") {
        return "MacOS".to_string();
    }
    // VST3 spells both x86_64 and aarch64 the way Rust does, so the target arch
    // passes through unchanged.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
    if target_os_is("windows") {
        format!("{arch}-win")
    } else {
        format!("{arch}-linux")
    }
}

fn dylib_ext() -> &'static str {
    if target_os_is("windows") {
        "vst3"
    } else if target_os_is("macos") {
        "dylib"
    } else {
        "so"
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=VST3_SDK_DIR");
    println!("cargo:rerun-if-changed=source");
    let sdk = match std::env::var_os("VST3_SDK_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(VENDORED_SDK),
    };
    assert!(
        sdk.join("pluginterfaces/base/funknown.h").is_file(),
        "no VST3 SDK at {}",
        sdk.display()
    );
    for f in SDK_SOURCES.iter().chain(platform_sources()) {
        println!("cargo:rerun-if-changed={}", sdk.join(f).display());
    }
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let gen_dir = out_dir.join("gen");
    std::fs::create_dir_all(&gen_dir).expect("gen dir");
    std::fs::write(
        gen_dir.join("projectversion.h"),
        "#pragma once\n#define MAJOR_VERSION_STR \"3\"\n#define MAJOR_VERSION_INT 3\n#define SUB_VERSION_STR \"8\"\n\
         #define SUB_VERSION_INT 8\n#define RELEASE_NUMBER_STR \"0\"\n#define RELEASE_NUMBER_INT 0\n\
         #define BUILD_NUMBER_STR \"0\"\n#define BUILD_NUMBER_INT 0\n#define FULL_VERSION_STR \"3.8.0.0\"\n#define VERSION_STR \"3.8.0\"\n",
    )
    .expect("projectversion.h");
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(&sdk)
        .include("source")
        .include(&gen_dir)
        .define("RELEASE", "1")
        .define("NDEBUG", "1")
        .warnings(false)
        .flag_if_supported("-Wno-multichar")
        .file("source/shell.cpp");
    let sdk_objects = compile_sdk_objects(&sdk, &gen_dir);
    let objects = build.compile_intermediates();
    let dir = out_dir.join("bundle");
    link_bundle(
        &build,
        objects.iter().chain(&sdk_objects),
        &dir,
        "shared-vst3-cpp",
    );
    println!("cargo:dir={}", dir.display());
}
