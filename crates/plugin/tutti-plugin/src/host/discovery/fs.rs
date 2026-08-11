//! Filesystem primitives for plugin discovery.

use super::record::PluginFormat;
use std::path::{Path, PathBuf};
use tracing::debug;

/// The one extension → format table: the single source of truth for which
/// file extensions name a plugin. [`super::record::PluginRecord::EXTENSIONS`]
/// is derived from this, so the two lists cannot drift apart. Drift is a
/// whole-platform outage: an extension advertised but rejected here makes
/// **no** VST2 plugin discoverable on Windows or Linux, where VST2 ships as a
/// bare shared library.
///
/// Entries are lowercase; candidates are lowercased before comparison.
pub(super) const FORMAT_BY_EXTENSION: &[(&str, PluginFormat)] = &[
    ("vst3", PluginFormat::Vst3),
    ("vst", PluginFormat::Vst2),
    ("dll", PluginFormat::Vst2),
    ("so", PluginFormat::Vst2),
    ("clap", PluginFormat::Clap),
    ("component", PluginFormat::AudioUnit),
];

/// Infer [`PluginFormat`] from a file extension.
///
/// Matching is **case-insensitive**. macOS (APFS/HFS+ by default) and Windows
/// (NTFS) are case-insensitive volumes, so a vendor shipping `Reverb.VST3` is
/// a real and common case. Matching case-sensitively made those bundles
/// invisible — and since a `.VST3` bundle *is* a directory, the scanner then
/// recursed *into* it and silently found nothing.
pub fn format_from_path(path: &Path) -> Option<PluginFormat> {
    let ext = path.extension().and_then(|s| s.to_str())?;
    let ext = ext.to_ascii_lowercase();
    FORMAT_BY_EXTENSION
        .iter()
        .find(|(known, _)| *known == ext)
        .map(|(_, format)| *format)
}

/// Get a file's modification time as seconds since the Unix epoch.
pub fn file_modification_time(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Recursively discover plugin files across `dirs`.
///
/// The pure half of a scan: walks the filesystem and returns what it found,
/// touching no catalog and spawning no subprocess. Pair with
/// [`PluginRecord::probe`](super::record::PluginRecord::probe) to build records
/// yourself when you own persistence and don't want a
/// [`PluginCatalog`](super::catalog::PluginCatalog) at all.
///
/// What you forfeit going this way is what the catalog buys: incremental
/// rescan (probing is a subprocess spawn with a multi-second timeout, so a
/// large plugin folder is minutes of work that mtime comparison otherwise
/// skips) and crash recovery (the dead-man's pedal needs state that outlives
/// the probe that killed the process). [`PluginScanner`](super::PluginScanner)
/// is the batteries-included path.
///
/// Unreadable directories are skipped, not reported — discovery over a list of
/// standard install locations must not fail because one of them is absent.
pub fn discover(dirs: &[PathBuf]) -> Vec<(PathBuf, PluginFormat)> {
    dirs.iter().flat_map(|dir| discover_plugins(dir)).collect()
}

/// Recursively discover plugin files in `dir`.
///
/// VST3 bundles are directories with a `.vst3` extension, so
/// `format_from_path` is checked before recursing.
pub(super) fn discover_plugins(dir: &Path) -> Vec<(PathBuf, PluginFormat)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        debug!("cannot read directory {:?}", dir);
        return Vec::new();
    };

    entries
        .flatten()
        .map(|e| e.path())
        .flat_map(|path| match format_from_path(&path) {
            Some(format) => vec![(path, format)],
            None if path.is_dir() => discover_plugins(&path),
            None => Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path
    }

    #[test]
    fn format_from_path_detection() {
        assert_eq!(
            format_from_path(Path::new("a.vst3")),
            Some(PluginFormat::Vst3)
        );
        assert_eq!(
            format_from_path(Path::new("a.vst")),
            Some(PluginFormat::Vst2)
        );
        assert_eq!(
            format_from_path(Path::new("a.clap")),
            Some(PluginFormat::Clap)
        );
        assert_eq!(
            format_from_path(Path::new("a.component")),
            Some(PluginFormat::AudioUnit)
        );
        assert_eq!(format_from_path(Path::new("a.txt")), None);
        // `dawai:audio-plugin` is the app's own format, hosted outside this
        // crate; tutti must not claim `.wasm` as a plugin extension.
        assert_eq!(format_from_path(Path::new("a.wasm")), None);
    }

    /// Regression for `.VST3` / `.CLAP` are the same extension on a
    /// case-insensitive volume (macOS APFS, Windows NTFS). Matching
    /// case-sensitively made those bundles invisible, and because a `.VST3`
    /// bundle is a *directory*, the scanner then recursed into it and found
    /// nothing — silently, with no warning.
    #[test]
    fn format_from_path_is_case_insensitive() {
        assert_eq!(
            format_from_path(Path::new("Reverb.VST3")),
            Some(PluginFormat::Vst3)
        );
        assert_eq!(
            format_from_path(Path::new("Comp.CLAP")),
            Some(PluginFormat::Clap)
        );
        assert_eq!(
            format_from_path(Path::new("Synth.Component")),
            Some(PluginFormat::AudioUnit)
        );
        assert_eq!(
            format_from_path(Path::new("Legacy.Vst")),
            Some(PluginFormat::Vst2)
        );
        assert_eq!(
            format_from_path(Path::new("Legacy.DLL")),
            Some(PluginFormat::Vst2)
        );
    }

    /// Regression for VST2 on Windows/Linux ships as a bare
    /// `.dll` / `.so`. `PluginRecord::EXTENSIONS` advertised both while
    /// `format_from_path` rejected both, so no VST2 plugin was discoverable
    /// on those platforms at all.
    #[test]
    fn vst2_bare_libraries_are_recognised() {
        assert_eq!(
            format_from_path(Path::new("Synth1.dll")),
            Some(PluginFormat::Vst2)
        );
        assert_eq!(
            format_from_path(Path::new("Synth1.so")),
            Some(PluginFormat::Vst2)
        );
    }

    /// The advertised list and the accepted list must be the same list.
    #[test]
    fn advertised_extensions_are_all_accepted() {
        use super::super::record::PluginRecord;
        for ext in PluginRecord::EXTENSIONS {
            assert!(
                format_from_path(Path::new(&format!("plugin.{ext}"))).is_some(),
                "PluginRecord::EXTENSIONS advertises {ext:?} but format_from_path rejects it"
            );
        }
        assert_eq!(
            PluginRecord::EXTENSIONS.len(),
            FORMAT_BY_EXTENSION.len(),
            "the advertised list must be exactly the accepted table"
        );
    }

    /// A `.VST3` bundle directory must be reported as a plugin, not recursed
    /// Into. This is the silent-invisibility half of
    #[test]
    fn discover_treats_uppercase_bundle_dir_as_plugin() {
        let dir = TempDir::new().unwrap();
        let bundle = dir.path().join("Reverb.VST3");
        std::fs::create_dir_all(bundle.join("Contents").join("MacOS")).unwrap();
        std::fs::write(bundle.join("Contents").join("MacOS").join("Reverb"), b"x").unwrap();

        let found = discover_plugins(dir.path());
        assert_eq!(found.len(), 1, "expected the bundle itself, got {found:?}");
        assert_eq!(found[0].0, bundle);
        assert_eq!(found[0].1, PluginFormat::Vst3);
    }

    #[test]
    fn discover_finds_plugins() {
        let dir = TempDir::new().unwrap();
        create_fake_plugin(dir.path(), "reverb.vst3");
        create_fake_plugin(dir.path(), "delay.clap");
        create_fake_plugin(dir.path(), "readme.txt");

        let found = discover_plugins(dir.path());
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn discover_recurses_into_subdirs() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        create_fake_plugin(&sub, "deep.vst3");

        let found = discover_plugins(dir.path());
        assert_eq!(found.len(), 1);
    }
}
