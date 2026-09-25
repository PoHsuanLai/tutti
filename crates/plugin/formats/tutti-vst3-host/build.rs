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

    // The SDK's translation units are compiled by their own `cc::Build` so a
    // second plugin can reuse the objects. Keeping them separate is not just
    // tidiness: every VST3 plugin supplies its *own* `GetPluginFactory`, so
    // handing another plugin this build's full object list — probe factory
    // included — is a duplicate-symbol link error.
    let sdk_objects = compile_sdk_objects(sdk, &gen);

    let objects = build.compile_intermediates();
    link_bundle(
        &build,
        objects.iter().chain(&sdk_objects),
        &out_dir.join("probe-bundle"),
        "audio-probe",
    );

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

    build_sdk_samples(sdk, &gen, &dir, &sdk_objects);
}

/// Compile the SDK's own translation units and return the objects.
///
/// Separate from any one plugin's build so more than one bundle can link them.
/// This is the expensive half — ~43 files — and compiling it per plugin would
/// double the build for nothing.
fn compile_sdk_objects(sdk: &Path, gen: &Path) -> Vec<PathBuf> {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(sdk)
        .include(gen)
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
        cmd.arg("/nologo").arg("/LD");
        cmd.args(&objects);
        cmd.arg(format!("/Fe:{}", so.display()));
        cmd.arg("/link");
        // Keep the import library and its export file out of the bundle. MSVC
        // writes both beside the DLL by default, and a `.vst3` bundle is a
        // directory a host *scans* — anything in the arch dir is a candidate
        // module. They are build artifacts, so they belong in OUT_DIR.
        let implib =
            PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join(format!("{plugin}.lib"));
        cmd.arg(format!("/IMPLIB:{}", implib.display()));
        cmd.args(probe_link_args());
    } else {
        cmd.arg("-shared").arg("-fPIC").arg("-o").arg(&so);
        cmd.args(&objects);
        cmd.args(probe_link_args());
    }

    let status = cmd.status().expect("failed to invoke the linker");
    assert!(status.success(), "linking {plugin} failed: {status}");
    assert!(so.is_file(), "linker reported success but {so:?} is absent");
}

/// Build the SDK sample plugins the conformance suite drives, beside the probe.
///
/// # Which samples, and why not all of them
///
/// The suite names four plugins. Two are built here, `audio-probe` is built
/// above, and two are not buildable from the pinned submodules at all:
///
/// - **`multiple_programchanges`** — declares **16 program lists** whose ids are
///   `kProgramStartId + i`, so a list id is provably not a position in
///   `program_lists()`. A host that dropped `list_id` and kept `index` would
///   collapse all 16 onto one another, and three tests in
///   `tutti-plugin-server` exist to catch exactly that. `audio-probe`
///   publishes no program lists, so it cannot stand in.
/// - **`remap_paramid`** — publishes an `IRemapParamID` mapping, the only way to
///   exercise the host's param-id migration path. Needs no VSTGUI: its
///   controller is a plain `EditControllerEx1`.
///
/// **`hostchecker` and `note_expression_synth` are NOT built, and cannot be.**
/// Both ship a controller that *inherits from* `VSTGUI::VST3EditorDelegate`
/// (`hostcheckercontroller.h:110`, `note_expression_synth_ui.h:36`), and each
/// sample's `factory.cpp` registers that controller — so the UI translation
/// unit is not optional, it is on the only path to `GetPluginFactory`. VSTGUI is
/// a separate Steinberg repository and is **not among this repo's three pinned
/// submodules** (`base`, `pluginterfaces`, `public.sdk`), so there is nothing to
/// compile it against. Vendoring a fourth submodule for a UI library the host
/// never calls into is a large amount of build for no host coverage.
///
/// The tests that need those two are `#[ignore]`d with that reason rather than
/// silently skipped — see `vst3_conformance.rs`. Note that the *hostchecker
/// validation modules* are unaffected and still compile: [`build_hostcheck`]
/// takes the six check `.cpp`s directly, and they depend only on the
/// header-only `pluginterfaces` and never touch the controller.
fn build_sdk_samples(sdk: &Path, gen: &Path, dir: &Path, sdk_objects: &[PathBuf]) {
    build_sdk_sample(
        sdk,
        gen,
        dir,
        sdk_objects,
        "multiple_programchanges",
        &["plug.cpp", "plugcontroller.cpp", "plugentry.cpp"],
        "multiple-program-changes",
    );
    build_sdk_sample(
        sdk,
        gen,
        dir,
        sdk_objects,
        "remap_paramid",
        &[
            "remapparamidprocessor.cpp",
            "remapparamidcontroller.cpp",
            "remapparamidentry.cpp",
        ],
        "remap-paramid",
    );
}

/// Compile one SDK sample under `public.sdk/samples/vst/<sample>` and link it
/// into a `.vst3` bundle named `bundle` under `dir`.
///
/// `sdk_objects` are the shared SDK translation units, compiled once by
/// [`compile_sdk_objects`]. Only *this* sample's objects are chained onto them:
/// every VST3 plugin supplies its own `GetPluginFactory`, so handing one sample
/// another's object list is a duplicate-symbol link error.
fn build_sdk_sample(
    sdk: &Path,
    gen: &Path,
    dir: &Path,
    sdk_objects: &[PathBuf],
    sample: &str,
    sources: &[&str],
    bundle: &str,
) {
    let src = sdk.join(format!("public.sdk/samples/vst/{sample}/source"));
    assert!(
        src.is_dir(),
        "the `{sample}` sample is missing from {} — the SDK submodule is \
         incomplete. Run: git submodule update --init --recursive",
        src.display()
    );

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(sdk)
        .include(&src)
        .include(gen)
        .define("RELEASE", "1")
        .define("NDEBUG", "1")
        .warnings(false)
        .flag_if_supported("-Wno-multichar");

    for f in sources {
        let path = src.join(f);
        assert!(path.is_file(), "sample source missing: {}", path.display());
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(path);
    }

    let objects = build.compile_intermediates();
    link_bundle(&build, objects.iter().chain(sdk_objects), dir, bundle);
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
