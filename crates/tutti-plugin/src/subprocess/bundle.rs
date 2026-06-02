//! Cross-platform plugin bundle resolution.
//!
//! VST3, CLAP, and AU are distributed as bundles (directories with a
//! `Contents/` subtree); VST2 is a plain shared library. This module maps a
//! bundle path to the actual binary inside it so the caller can `dlopen` it.

use crate::error::{BridgeError, Result};
use std::path::{Path, PathBuf};

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

    #[cfg(target_os = "macos")]
    let resolved = probe_subdir(path, "MacOS", None);

    #[cfg(target_os = "linux")]
    let resolved = probe_subdir(path, "x86_64-linux", Some("so"));

    #[cfg(target_os = "windows")]
    let resolved = probe_subdir(path, "x86_64-win", Some("vst3"));

    resolved.ok_or_else(|| BridgeError::BundleResolutionFailed {
        path: path.to_path_buf(),
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

    /// On the current OS, return the subdirectory `probe_subdir` looks in.
    fn arch_subdir() -> &'static str {
        #[cfg(target_os = "macos")]
        {
            "MacOS"
        }
        #[cfg(target_os = "linux")]
        {
            "x86_64-linux"
        }
        #[cfg(target_os = "windows")]
        {
            "x86_64-win"
        }
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
            Err(BridgeError::BundleResolutionFailed { path }) => assert_eq!(path, bundle),
            other => panic!("expected BundleResolutionFailed, got {other:?}"),
        }
    }
}
