//! Filesystem primitives for plugin discovery.

use super::record::PluginFormat;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Infer [`PluginFormat`] from a file extension.
pub fn format_from_path(path: &Path) -> Option<PluginFormat> {
    path.extension()
        .and_then(|s| s.to_str())
        .and_then(|ext| match ext {
            "vst3" => Some(PluginFormat::Vst3),
            "vst" => Some(PluginFormat::Vst2),
            "clap" => Some(PluginFormat::Clap),
            "component" => Some(PluginFormat::AudioUnit),
            "wasm" => Some(PluginFormat::Wasm),
            _ => None,
        })
}

/// Get a file's modification time as seconds since the Unix epoch.
pub fn file_modification_time(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
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
        assert_eq!(
            format_from_path(Path::new("a.wasm")),
            Some(PluginFormat::Wasm)
        );
        assert_eq!(format_from_path(Path::new("a.txt")), None);
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
