//! Filesystem primitives for plugin discovery.

use super::facts::FileFacts;
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

/// Infers the [`PluginFormat`] from a path's extension.
///
/// Matching is **case-insensitive**. macOS (APFS/HFS+ by default) and Windows
/// (NTFS) are case-insensitive volumes, so a vendor shipping `Reverb.VST3` is
/// a real and common case. Matching case-sensitively would make those bundles
/// invisible — and since a `.VST3` bundle *is* a directory, the scanner would
/// recurse *into* it and silently find nothing.
pub fn format_from_path(path: impl AsRef<Path>) -> Option<PluginFormat> {
    let ext = path.as_ref().extension().and_then(|s| s.to_str())?;
    let ext = ext.to_ascii_lowercase();
    FORMAT_BY_EXTENSION
        .iter()
        .find(|(known, _)| *known == ext)
        .map(|(_, format)| *format)
}

impl FileFacts {
    /// Reads the facts for `path`.
    ///
    /// The scan's filesystem edge: every decision after it is a pure function of
    /// the value. For a VST2 or VST3 bundle (a directory) the modification time
    /// is the later of the directory's and its module's, since replacing the
    /// binary inside leaves the directory's own time alone. The module is found
    /// by `tutti_plugin_types::bundle`, the one bundle walk.
    pub fn read(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path);
        let format = format_from_path(path);
        let mut mtime = metadata.as_ref().ok().and_then(modified_secs);
        if metadata.as_ref().is_ok_and(|m| m.is_dir())
            && let Some(module) = module_in_bundle(path, format)
        {
            let inner = std::fs::metadata(module)
                .ok()
                .and_then(|m| modified_secs(&m));
            mtime = mtime.max(inner);
        }
        Self {
            exists: metadata.is_ok(),
            mtime,
            format,
        }
    }
}

fn modified_secs(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// The module inside a bundle directory, preferring the one this target loads.
fn module_in_bundle(bundle: &Path, format: Option<PluginFormat>) -> Option<PathBuf> {
    use tutti_plugin_types::bundle::{ModuleKind, any_module_in_bundle, native_module_in_bundle};
    let kind = match format? {
        PluginFormat::Vst3 => ModuleKind::Vst3,
        PluginFormat::Vst2 => ModuleKind::Vst2,
        _ => return None,
    };
    native_module_in_bundle(bundle, kind).or_else(|| any_module_in_bundle(bundle, kind))
}

/// Recursively discovers plugin files across `dirs`, with the format each
/// extension names.
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
pub fn discover(dirs: impl IntoIterator<Item = impl AsRef<Path>>) -> Vec<(PathBuf, PluginFormat)> {
    dirs.into_iter()
        .flat_map(|dir| discover_plugins(dir.as_ref()))
        .collect()
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
        // A WASM audio node is a host's own format, hosted outside this crate;
        // tutti must not claim `.wasm` as a plugin extension.
        assert_eq!(format_from_path(Path::new("a.wasm")), None);
    }

    /// `.VST3` / `.CLAP` are the same extension on a case-insensitive volume
    /// (macOS APFS, Windows NTFS). Matching case-sensitively would make those
    /// bundles invisible, and because a `.VST3` bundle is a *directory*, the
    /// scanner would then recurse into it and find nothing — silently.
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

    /// VST2 on Windows/Linux ships as a bare `.dll` / `.so`, and
    /// `format_from_path` must agree with `PluginRecord::EXTENSIONS` on both,
    /// or no VST2 plugin is discoverable on those platforms.
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

    /// Replacing a bundle's binary leaves the directory's own time alone; the
    /// facts must still move. Mutation: return the directory's time alone (drop
    /// the `max` with the module's).
    #[cfg(unix)]
    #[test]
    fn a_bundle_whose_binary_changed_reads_as_changed() {
        use std::time::{Duration, UNIX_EPOCH};
        let dir = TempDir::new().unwrap();
        let bundle = dir.path().join("Reverb.vst3");
        let arch = bundle
            .join("Contents")
            .join(tutti_plugin_types::bundle::native_arch_subdirs()[0]);
        std::fs::create_dir_all(&arch).unwrap();
        let module = arch.join(if cfg!(target_os = "linux") {
            "Reverb.so"
        } else {
            "Reverb"
        });
        std::fs::write(&module, b"x").unwrap();

        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        let set = |p: &Path, secs: u64| {
            std::fs::File::open(p)
                .unwrap()
                .set_modified(at(secs))
                .unwrap();
        };
        set(&module, 2_000_000_000);
        set(&bundle, 1_000_000_000);

        let facts = FileFacts::read(&bundle);
        assert_eq!(facts.mtime, Some(2_000_000_000));
        assert_eq!(facts.format, Some(PluginFormat::Vst3));

        // The directory later than the binary still wins.
        set(&bundle, 3_000_000_000);
        assert_eq!(FileFacts::read(&bundle).mtime, Some(3_000_000_000));
    }

    #[test]
    fn file_facts_read_reports_a_file_a_missing_path_and_the_extension() {
        let dir = TempDir::new().unwrap();
        let file = create_fake_plugin(dir.path(), "a.clap");

        let present = FileFacts::read(&file);
        assert!(present.exists);
        assert!(present.mtime.is_some_and(|t| t > 0));
        assert_eq!(present.format, Some(PluginFormat::Clap));

        let absent = FileFacts::read(dir.path().join("gone.vst3"));
        assert!(!absent.exists);
        assert_eq!(absent.mtime, None);
        assert_eq!(absent.format, Some(PluginFormat::Vst3));
    }
}
