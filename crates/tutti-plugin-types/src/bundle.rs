//! Locating the loadable module inside a plugin bundle directory (`.vst3`, `.vst`,
//! `.clap`, `.component`).
//!
//! A bundle is a *directory*: the module sits at `Contents/<arch>/<name>`,
//! where `<arch>` names the platform and CPU and the extension differs per
//! platform and per format. Getting the walk wrong fails silently — `None` is
//! indistinguishable from "this is not a bundle" — so every host and test that
//! needs the module should use these functions rather than its own walk. They
//! also skip the import libraries (`.lib`) a Windows linker drops beside the
//! module.
//!
//! # Two traversals
//!
//! [`native_module_in_bundle`] probes only the directories *this* target can
//! load from. That is what a host wants: loading a foreign-arch binary fails at
//! `dlopen`/`LoadLibrary` anyway, and reporting "no module" is the honest answer.
//!
//! [`any_module_in_bundle`] probes every arch directory any platform uses. That
//! is what a *corpus scan* wants: a test fixture built by a cross-build, or a
//! bundle assembled for another host, should be found rather than silently
//! skipped.

use std::path::{Path, PathBuf};

/// Which plugin format's bundle is being walked.
///
/// The formats agree on the directory layout and disagree on the module's
/// extension — VST3 names it `.vst3` on Windows where VST2 names it `.dll` —
/// so the extension cannot be derived from the platform alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    /// `.vst` bundles: `.dll` on Windows, `.so` on Linux, extensionless on macOS.
    Vst2,
    /// `.vst3` bundles: `.vst3` on Windows, `.so` on Linux, extensionless on macOS.
    Vst3,
    /// `.clap` bundles: `.clap` on Windows, `.so` on Linux, extensionless on macOS.
    Clap,
    /// `.component` bundles, macOS only: the executable under `Contents/MacOS`,
    /// named by the bundle's `Info.plist` and usually its stem.
    Au,
}

/// Every `Contents/<arch>` subdirectory any platform may keep a module in.
///
/// Ordered macOS, Linux, Windows, and within Windows in the SDK's own
/// preference order. Used by [`any_module_in_bundle`]; a host wanting only what
/// it can actually load wants [`native_arch_subdirs`] instead.
pub const ALL_ARCH_SUBDIRS: &[&str] = &[
    "MacOS",
    "x86_64-linux",
    "aarch64-linux",
    "armv7l-linux",
    "armv8l-linux",
    "arm-linux",
    "i686-linux",
    "i386-linux",
    "x86_64-win",
    "arm64-win",
    "arm64ec-win",
    "arm64x-win",
    "arm-win",
    "x86-win",
];

/// Returns the `Contents/<arch>` subdirectories *this* target can load from,
/// most preferred first.
///
/// On ARM64 Windows an `arm64ec`/`arm64x` module is also loadable and x64 runs
/// under emulation, so those follow as fallbacks in the SDK's order. Everywhere
/// else the list is one entry: a binary for another CPU is not a fallback, it
/// is a load error with extra steps.
pub fn native_arch_subdirs() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        // A macOS bundle binary is fat/universal; there is no per-arch dir.
        &["MacOS"]
    }

    // Linux: `<machine>-linux`, where `<machine>` is the `uname -m` string.
    // Rust's `target_arch` maps onto those names.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["x86_64-linux"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["aarch64-linux"]
    }
    #[cfg(all(target_os = "linux", target_arch = "arm"))]
    {
        // `uname -m` reports the specific ARM variant; probe the common ones.
        &["armv7l-linux", "armv8l-linux", "arm-linux"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86"))]
    {
        &["i686-linux", "i386-linux"]
    }
    #[cfg(all(
        target_os = "linux",
        not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86"
        ))
    ))]
    {
        &["x86_64-linux"]
    }

    // Windows: the SDK's six variants, narrowed to what this target can load.
    // On ARM64 Windows an arm64ec/arm64x binary is also loadable, and x64 runs
    // under emulation, so probe those as fallbacks in the SDK's order.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        &["x86_64-win"]
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        &["arm64-win", "arm64ec-win", "arm64x-win", "x86_64-win"]
    }
    #[cfg(all(target_os = "windows", target_arch = "arm"))]
    {
        &["arm-win", "x86-win"]
    }
    #[cfg(all(target_os = "windows", target_arch = "x86"))]
    {
        &["x86-win"]
    }
    #[cfg(all(
        target_os = "windows",
        not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "x86"
        ))
    ))]
    {
        &["x86_64-win"]
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        &["MacOS"]
    }
}

/// Module extensions that may appear inside `arch_dir`, in probe order.
///
/// Keyed on the *directory* rather than on `cfg!`, because
/// [`any_module_in_bundle`] walks foreign-arch directories too and a macOS
/// bundle's module is extensionless whatever host is reading it. An empty
/// string means "no extension", which is macOS's convention and not an error.
fn exts_for(arch_dir: &str, kind: ModuleKind) -> &'static [&'static str] {
    if arch_dir == "MacOS" {
        return &["dylib"];
    }
    if arch_dir.ends_with("-linux") {
        return &["so"];
    }
    match kind {
        ModuleKind::Vst2 => &["dll"],
        ModuleKind::Vst3 => &["vst3", "dll"],
        ModuleKind::Clap => &["clap", "dll"],
        ModuleKind::Au => &[],
    }
}

/// Returns whether `path` is a linker by-product (`.lib`, `.exp`, `.pdb`,
/// `.ilk`) rather than a module.
///
/// On Windows an MSVC link drops `<stem>.lib`, `.exp`, `.pdb` and `.ilk` next to
/// the DLL, all inside the bundle. A bundle is a directory a host scans, so
/// every file in it is a candidate — and `read_dir` order is arbitrary, so
/// "take the first file" is a coin flip that reports a healthy plugin as
/// corrupt. This is what makes the enumeration in [`any_module_in_bundle`] safe.
#[must_use]
pub fn is_link_byproduct(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("lib" | "exp" | "pdb" | "ilk")
    )
}

/// Returns the module inside `bundle` that this target can load, or `None`.
///
/// A path that is already a file, or is not a directory, yields `None` — it is
/// not a bundle, and the caller knows better than this function what to do
/// about that. Hosts typically pass such a path to the loader unchanged.
pub fn native_module_in_bundle(bundle: &Path, kind: ModuleKind) -> Option<PathBuf> {
    probe_dirs(bundle, native_arch_subdirs(), kind, false)
}

/// The file a host loads for `path`: its native module when `path` is a bundle
/// directory, and `path` itself otherwise (a plain library, or a path that does
/// not exist, which the loader then reports with its own context).
///
/// # Errors
///
/// [`PluginError::Load`](crate::PluginError::Load) at
/// [`LoadStage::Opening`](crate::LoadStage::Opening) when `path` is a directory
/// holding no module this target can load; the message lists where it looked.
pub fn native_module_path(path: &Path, kind: ModuleKind) -> crate::PluginResult<PathBuf> {
    if path.is_file() || !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    native_module_in_bundle(path, kind).ok_or_else(|| crate::PluginError::Load {
        stage: crate::LoadStage::Opening,
        reason: format!(
            "{} holds no module for this target (looked in {})",
            path.display(),
            native_arch_subdirs().join(", ")
        ),
    })
}

/// Returns the module inside `bundle` for *any* platform, or `None`.
///
/// Probes every directory in [`ALL_ARCH_SUBDIRS`] by name, then falls back to
/// enumerating each one and taking the first file that is not a
/// [`link by-product`](is_link_byproduct). The fallback is what finds a module
/// whose name does not follow the bundle's stem — which a hand-assembled test
/// fixture need not.
pub fn any_module_in_bundle(bundle: &Path, kind: ModuleKind) -> Option<PathBuf> {
    probe_dirs(bundle, ALL_ARCH_SUBDIRS, kind, true)
}

fn probe_dirs(
    bundle: &Path,
    subdirs: &[&str],
    kind: ModuleKind,
    enumerate: bool,
) -> Option<PathBuf> {
    if bundle.is_file() || !bundle.is_dir() {
        return None;
    }
    // Named lookup first, across every directory, before any enumeration. A
    // correctly named module in the *last* directory is a better answer than
    // whatever `read_dir` happens to return from the first.
    for sub in subdirs {
        if let Some(found) = probe_named(bundle, sub, kind) {
            return Some(found);
        }
    }
    if !enumerate {
        return None;
    }
    for sub in subdirs {
        let dir = bundle.join("Contents").join(sub);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && !is_link_byproduct(p))
            .collect();
        // `read_dir` order is arbitrary and the caller may compare paths across
        // runs, so settle on one deterministically.
        found.sort();
        if let Some(first) = found.into_iter().next() {
            return Some(first);
        }
    }
    None
}

/// The conventionally-named module in one arch directory.
///
/// Three spellings, in order: the bare stem (macOS), the stem plus this
/// directory's extension (Linux/Windows), and the bundle's *full* name — which
/// some shipping bundles use, e.g.
/// `SpectraLayers.vst3/Contents/MacOS/SpectraLayers.vst3`.
fn probe_named(bundle: &Path, arch_dir: &str, kind: ModuleKind) -> Option<PathBuf> {
    let dir = bundle.join("Contents").join(arch_dir);
    let stem = bundle.file_stem()?.to_str()?;

    // The bare stem first, on **every** platform. macOS bundle binaries are
    // extensionless by convention, but a Linux or Windows bundle is free to
    // ship one too — `Surge XT.vst3/Contents/x86_64-linux/Surge XT` is real,
    // and probing only `<stem>.so` there resolves nothing.
    let bare = dir.join(stem);
    if bare.is_file() {
        return Some(bare);
    }
    for ext in exts_for(arch_dir, kind) {
        let candidate = dir.join(format!("{stem}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(name) = bundle.file_name() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build `<name>` under `tmp` with one file at `Contents/<arch>/<file>`.
    fn bundle_with(tmp: &TempDir, name: &str, arch: &str, files: &[&str]) -> PathBuf {
        let bundle = tmp.path().join(name);
        let dir = bundle.join("Contents").join(arch);
        fs::create_dir_all(&dir).expect("create bundle");
        for f in files {
            fs::write(dir.join(f), b"x").expect("write module");
        }
        bundle
    }

    #[test]
    fn every_native_subdir_is_one_the_all_list_knows() {
        for sub in native_arch_subdirs() {
            assert!(
                ALL_ARCH_SUBDIRS.contains(sub),
                "{sub} is probed natively but missing from ALL_ARCH_SUBDIRS, so \
                 a corpus scan would skip a bundle this very host can load"
            );
        }
    }

    #[test]
    fn aarch64_linux_and_the_arm64_windows_spellings_are_all_covered() {
        // Directories that are easy to miss. Named one by one rather than by
        // counting, so adding a directory does not silently satisfy this.
        for required in [
            "aarch64-linux",
            "arm64-win",
            "arm64ec-win",
            "arm64x-win",
            "MacOS",
            "x86_64-linux",
            "x86_64-win",
        ] {
            assert!(
                ALL_ARCH_SUBDIRS.contains(&required),
                "{required} is missing from ALL_ARCH_SUBDIRS"
            );
        }
    }

    #[test]
    fn a_plain_file_is_not_a_bundle() {
        let tmp = TempDir::new().expect("tmp");
        let file = tmp.path().join("plain.vst3");
        fs::write(&file, b"x").expect("write");
        assert_eq!(any_module_in_bundle(&file, ModuleKind::Vst3), None);
        assert_eq!(native_module_in_bundle(&file, ModuleKind::Vst3), None);
    }

    #[test]
    fn a_missing_path_is_not_a_bundle() {
        let tmp = TempDir::new().expect("tmp");
        let nope = tmp.path().join("absent.vst3");
        assert_eq!(any_module_in_bundle(&nope, ModuleKind::Vst3), None);
    }

    #[test]
    fn a_foreign_arch_bundle_is_found_by_any_and_not_by_native() {
        let tmp = TempDir::new().expect("tmp");
        // A directory no host in this test run is: if the native probe ever
        // matches it, `native_arch_subdirs` has grown a wrong entry.
        let foreign = if cfg!(target_os = "windows") {
            "aarch64-linux"
        } else {
            "arm64ec-win"
        };
        let ext = if foreign.ends_with("-linux") {
            "so"
        } else {
            "vst3"
        };
        let b = bundle_with(&tmp, "probe.vst3", foreign, &[&format!("probe.{ext}")]);

        assert!(
            any_module_in_bundle(&b, ModuleKind::Vst3).is_some(),
            "a cross-built bundle must still be found by a corpus scan"
        );
        assert_eq!(
            native_module_in_bundle(&b, ModuleKind::Vst3),
            None,
            "a module this target cannot load must not be offered to the loader"
        );
    }

    #[test]
    fn the_import_library_is_never_taken_for_the_module() {
        let tmp = TempDir::new().expect("tmp");
        // The exact Windows link output, and deliberately NOT named after the
        // bundle stem — so the named lookup cannot rescue this and the
        // enumeration has to make the right call on its own.
        let b = bundle_with(
            &tmp,
            "audio-probe.vst3",
            "x86_64-win",
            &[
                "other.lib",
                "other.exp",
                "other.pdb",
                "other.ilk",
                "other.vst3",
            ],
        );
        let got = any_module_in_bundle(&b, ModuleKind::Vst3).expect("a module must be found");
        assert_eq!(
            got.extension().and_then(|e| e.to_str()),
            Some("vst3"),
            "the loader was handed {got:?}, a linker by-product rather than the \
             module — which is reported as a corrupt plugin"
        );
    }

    #[test]
    fn a_bundle_of_only_link_byproducts_yields_nothing() {
        let tmp = TempDir::new().expect("tmp");
        let b = bundle_with(&tmp, "x.vst3", "x86_64-win", &["x.lib", "x.exp"]);
        assert_eq!(
            any_module_in_bundle(&b, ModuleKind::Vst3),
            None,
            "no module is `None`, never the nearest by-product"
        );
    }

    #[test]
    fn vst2_and_vst3_disagree_about_the_windows_extension() {
        let tmp = TempDir::new().expect("tmp");
        let b2 = bundle_with(&tmp, "a.vst", "x86_64-win", &["a.dll"]);
        let b3 = bundle_with(&tmp, "b.vst3", "x86_64-win", &["b.vst3"]);
        assert!(any_module_in_bundle(&b2, ModuleKind::Vst2).is_some());
        assert!(any_module_in_bundle(&b3, ModuleKind::Vst3).is_some());
    }

    #[test]
    fn an_extensionless_binary_resolves_outside_macos_too() {
        // `Surge XT.vst3/Contents/x86_64-linux/Surge XT`. Probing only
        // `<stem>.so` in a `-linux` directory misses this, and the bundle reads
        // as "wrong architecture" when it is simply named the other way.
        let tmp = TempDir::new().expect("tmp");
        for arch in ["x86_64-linux", "x86_64-win"] {
            let b = bundle_with(&tmp, &format!("Surge {arch}.vst3"), arch, &[]);
            let inner = b.join("Contents").join(arch).join(format!("Surge {arch}"));
            fs::write(&inner, b"x").expect("write");
            assert_eq!(
                any_module_in_bundle(&b, ModuleKind::Vst3).as_deref(),
                Some(inner.as_path()),
                "an extensionless module in {arch} was not found"
            );
        }
    }

    #[test]
    fn the_outer_extension_on_the_inner_binary_is_accepted() {
        // SpectraLayers.vst3/Contents/MacOS/SpectraLayers.vst3 — the module is
        // named with the bundle's full name, not its stem.
        let tmp = TempDir::new().expect("tmp");
        let b = bundle_with(&tmp, "Spectra.vst3", "MacOS", &["Spectra.vst3"]);
        assert!(any_module_in_bundle(&b, ModuleKind::Vst3).is_some());
    }

    #[test]
    fn a_named_module_beats_an_enumerated_one_in_an_earlier_directory() {
        let tmp = TempDir::new().expect("tmp");
        let b = bundle_with(&tmp, "p.vst3", "MacOS", &["stray-file"]);
        let win = b.join("Contents").join("x86_64-win");
        fs::create_dir_all(&win).expect("mk");
        fs::write(win.join("p.vst3"), b"x").expect("w");

        let got = any_module_in_bundle(&b, ModuleKind::Vst3).expect("found");
        assert_eq!(
            got.file_name().and_then(|n| n.to_str()),
            Some("p.vst3"),
            "the correctly-named module lost to whatever happened to sit in an \
             earlier arch directory"
        );
    }

    /// A Windows CLAP module is `<stem>.clap` in the arch directory (the bundle
    /// is spelled `.CLAP` so the full-name spelling cannot find it). Mutation:
    /// drop `"clap"` from `ModuleKind::Clap`'s extensions in `exts_for`.
    #[test]
    fn a_clap_module_is_named_with_the_clap_extension_on_windows() {
        let tmp = TempDir::new().unwrap();
        let b = bundle_with(&tmp, "Synth.CLAP", "x86_64-win", &["Synth.clap"]);
        assert_eq!(
            probe_named(&b, "x86_64-win", ModuleKind::Clap),
            Some(b.join("Contents/x86_64-win/Synth.clap"))
        );
    }

    /// A macOS CLAP or AU bundle's executable is the bare stem under
    /// `Contents/MacOS`. Mutation: make `probe_named` skip the bare stem.
    #[test]
    fn a_macos_clap_or_au_bundle_resolves_its_bare_stem_executable() {
        let tmp = TempDir::new().unwrap();
        for (name, kind) in [
            ("Synth.clap", ModuleKind::Clap),
            ("Verb.component", ModuleKind::Au),
        ] {
            let b = bundle_with(&tmp, name, "MacOS", &[name.split('.').next().unwrap()]);
            let exe = b
                .join("Contents/MacOS")
                .join(name.split('.').next().unwrap());
            assert_eq!(probe_named(&b, "MacOS", kind), Some(exe.clone()));
            assert_eq!(any_module_in_bundle(&b, kind), Some(exe));
        }
    }

    /// An AU executable need not carry the bundle's stem (its `Info.plist` names
    /// it); the enumeration finds it. Mutation: set `enumerate` to `false` in
    /// `any_module_in_bundle`.
    #[test]
    fn an_au_executable_with_another_name_is_found_by_enumeration() {
        let tmp = TempDir::new().unwrap();
        let b = bundle_with(&tmp, "Verb.component", "MacOS", &["VerbEngine"]);
        assert_eq!(
            any_module_in_bundle(&b, ModuleKind::Au),
            Some(b.join("Contents/MacOS/VerbEngine"))
        );
    }
}
