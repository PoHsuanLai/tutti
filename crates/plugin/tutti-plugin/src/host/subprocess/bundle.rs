//! Cross-platform plugin bundle resolution.
//!
//! VST3, CLAP, and AU are distributed as bundles (directories with a
//! `Contents/` subtree); VST2 is a plain shared library. This module maps a
//! bundle path to the actual binary inside it so the caller can `dlopen` it.

use crate::error::{BridgeError, Result};
use std::path::{Path, PathBuf};

/// Architecture subdirectories under `Contents/` to probe, in preference
/// order, for the *build target* this binary was compiled for.
///
/// The VST3 SDK derives this name from the running architecture, not just the
/// OS: `module_linux.cpp` builds `uname().machine + "-linux"`, and
/// `module_win32.cpp` enumerates six Windows variants. Keying on
/// `target_os` alone (as this module used to) means zero VST3/CLAP bundles
/// resolve on ARM Linux or Windows-on-ARM.
///
/// This is deliberately a function rather than a `#[cfg]` chain at the call
/// site so the tests can consume the *same* list — a test helper that
/// hardcodes its own copy passes on ARM while production fails.
pub(crate) fn arch_subdirs() -> &'static [&'static str] {
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

/// Explicit inner-binary extension used on this platform, if any. macOS
/// bundle binaries are extensionless; Linux uses `.so` and Windows `.vst3`.
fn arch_binary_ext() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        None
    }
    #[cfg(target_os = "linux")]
    {
        Some("so")
    }
    #[cfg(target_os = "windows")]
    {
        Some("vst3")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Resolve a plugin bundle directory to its inner library binary.
///
/// If `path` is already a file (VST2) or doesn't exist as a directory, it is
/// returned unchanged — the caller will surface any load failure with its own
/// richer context. For bundle directories, probes platform-specific
/// subdirectories under `Contents/`.
pub fn resolve_bundle(path: &Path) -> Result<PathBuf> {
    if path.is_file() || !path.is_dir() {
        return Ok(path.to_path_buf());
    }

    let subdirs = arch_subdirs();
    let ext = arch_binary_ext();

    subdirs
        .iter()
        .find_map(|arch| probe_subdir(path, arch, ext))
        .ok_or_else(|| BridgeError::BundleResolutionFailed {
            path: path.to_path_buf(),
            // Self-diagnosing: says *where* we looked, so a missing-arch
            // bundle reads as "wrong architecture" and not "corrupt".
            arch_subdirs: subdirs.join(", "),
        })
}

fn probe_subdir(bundle: &Path, arch_dir: &str, ext: Option<&str>) -> Option<PathBuf> {
    let dir = bundle.join("Contents").join(arch_dir);
    let stem = bundle.file_stem()?;

    // Standard macOS/VST3/AU/CLAP layout: Contents/MacOS/<BundleStem>
    let candidate = dir.join(stem);
    if candidate.is_file() {
        return Some(candidate);
    }

    // Linux/Windows convention: stem with explicit extension
    if let Some(ext) = ext {
        let with_ext = dir.join(format!("{}.{}", stem.to_str()?, ext));
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }

    // Some bundles keep the outer extension on the inner binary
    // (e.g. SpectraLayers.vst3/Contents/MacOS/SpectraLayers.vst3)
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

    /// The subdirectory `resolve_bundle` looks in *first* on this target.
    ///
    /// This deliberately reads from production's `arch_subdirs()` rather than
    /// hardcoding a copy: the previous helper hardcoded `"x86_64-linux"` /
    /// `"x86_64-win"`, the exact strings production got wrong, so the tests
    /// passed on ARM while every real bundle failed to resolve.
    fn arch_subdir() -> &'static str {
        arch_subdirs()[0]
    }

    fn make_bundle(tmp: &TempDir, name: &str) -> PathBuf {
        let bundle = tmp.path().join(name);
        fs::create_dir_all(bundle.join("Contents").join(arch_subdir())).unwrap();
        bundle
    }

    #[test]
    fn passes_through_plain_files() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("plain.so");
        fs::write(&file, b"").unwrap();
        assert_eq!(resolve_bundle(&file).unwrap(), file);
    }

    #[test]
    fn passes_through_nonexistent_paths() {
        let missing = PathBuf::from("/definitely/does/not/exist.vst3");
        // Not a file and not a directory — caller will diagnose.
        assert_eq!(resolve_bundle(&missing).unwrap(), missing);
    }

    #[test]
    fn resolves_stem_binary() {
        let tmp = TempDir::new().unwrap();
        let bundle = make_bundle(&tmp, "Surge XT.vst3");
        let inner = bundle.join("Contents").join(arch_subdir()).join("Surge XT");
        fs::write(&inner, b"").unwrap();
        assert_eq!(resolve_bundle(&bundle).unwrap(), inner);
    }

    #[test]
    fn resolves_full_bundle_name_binary() {
        // e.g. SpectraLayers.vst3/Contents/MacOS/SpectraLayers.vst3
        let tmp = TempDir::new().unwrap();
        let bundle = make_bundle(&tmp, "SpectraLayers.vst3");
        let inner = bundle
            .join("Contents")
            .join(arch_subdir())
            .join("SpectraLayers.vst3");
        fs::write(&inner, b"").unwrap();
        assert_eq!(resolve_bundle(&bundle).unwrap(), inner);
    }

    #[test]
    fn errors_when_no_binary_matches() {
        let tmp = TempDir::new().unwrap();
        let bundle = make_bundle(&tmp, "Empty.vst3");
        // Drop an unrelated file so the directory is non-empty — the removed
        // fallback would have picked this up; we no longer want that.
        let stray = bundle
            .join("Contents")
            .join(arch_subdir())
            .join("readme.txt");
        fs::write(&stray, b"").unwrap();

        match resolve_bundle(&bundle) {
            Err(BridgeError::BundleResolutionFailed { path, arch_subdirs }) => {
                assert_eq!(path, bundle);
                // Self-diagnosing: the error names where we looked.
                assert!(
                    arch_subdirs.contains(super::arch_subdirs()[0]),
                    "error should name the probed arch subdir, got {arch_subdirs:?}"
                );
            }
            other => panic!("expected BundleResolutionFailed, got {other:?}"),
        }
    }

    /// Regression for DISC-H5: the arch subdir must track `target_arch`, not
    /// just `target_os`. On ARM Linux / Windows-on-ARM the old OS-only
    /// `#[cfg]` produced `x86_64-*`, so zero bundles resolved.
    #[test]
    fn arch_subdir_tracks_target_architecture() {
        let dirs = super::arch_subdirs();
        assert!(!dirs.is_empty());

        #[cfg(target_os = "macos")]
        assert_eq!(dirs, &["MacOS"]);

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        assert_eq!(dirs[0], "aarch64-linux");

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(dirs[0], "x86_64-linux");

        #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
        assert_eq!(dirs[0], "arm64-win");

        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        assert_eq!(dirs[0], "x86_64-win");

        // On every non-mac target the subdir carries the architecture, and on
        // a 64-bit ARM target it must never be an x86 name.
        #[cfg(all(not(target_os = "macos"), target_arch = "aarch64"))]
        assert!(
            !dirs[0].starts_with("x86"),
            "ARM64 target must not probe an x86 subdir first, got {:?}",
            dirs[0]
        );
    }

    /// A bundle laid out for a *different* architecture must not resolve, and
    /// the error must say which subdirs were probed.
    #[test]
    fn wrong_architecture_bundle_reports_probed_subdirs() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("Foreign.vst3");
        // Deliberately a subdir this target never probes.
        let foreign = bundle.join("Contents").join("sparc64-solaris");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("Foreign"), b"").unwrap();

        match resolve_bundle(&bundle) {
            Err(BridgeError::BundleResolutionFailed { arch_subdirs, .. }) => {
                assert!(!arch_subdirs.is_empty());
                assert!(!arch_subdirs.contains("sparc64-solaris"));
            }
            other => panic!("expected BundleResolutionFailed, got {other:?}"),
        }
    }
}
