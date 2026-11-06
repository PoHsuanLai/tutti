//! Dead-man's pedal: a sentinel file whose presence signals the scanner
//! crashed while probing a specific plugin. Next startup blacklists that
//! plugin so the scan can make progress.

use super::catalog::{CatalogExt, PluginCatalog};
use super::facts::FileFacts;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug)]
pub(super) struct Pedal {
    path: PathBuf,
}

impl Pedal {
    /// Pedal file lives at `path` for the duration of a single probe.
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Record the plugin that is about to be probed.
    pub(super) fn arm(&self, plugin: impl AsRef<Path>) -> std::io::Result<()> {
        let mut f = std::fs::File::create(&self.path)?;
        f.write_all(plugin.as_ref().to_string_lossy().as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    /// Remove the pedal on clean completion.
    pub(super) fn disarm(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// If a pedal file survives from a crashed previous scan, blacklist
    /// the recorded plugin and remove the pedal. Returns `true` if
    /// recovery happened.
    pub(super) fn recover(&self, catalog: &mut dyn PluginCatalog) -> bool {
        if !self.path.exists() {
            return false;
        }
        let crashed = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| parse_pedal(&text));
        let recovered = crashed.is_some();
        if let Some(p) = crashed {
            warn!(
                "dead-man's pedal found: blacklisting {:?} (crashed during previous scan)",
                p
            );
            catalog.blacklist(
                &p,
                "crashed during previous scan".into(),
                &FileFacts::read(&p),
            );
            // Persist the blacklist immediately so it survives another crash.
            let _ = catalog.flush();
        }
        let _ = std::fs::remove_file(&self.path);
        recovered
    }
}

/// Reads the plugin a pedal file names.
///
/// Returns `None` for text that names no plugin (empty or only whitespace).
pub(super) fn parse_pedal(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::super::catalog::MemoryCatalog;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn arm_writes_and_disarm_removes() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let plugin = Path::new("/plugins/test.vst3");

        pedal.arm(plugin).unwrap();
        assert!(pedal.path.exists());
        let contents = std::fs::read_to_string(&pedal.path).unwrap();
        assert_eq!(contents, "/plugins/test.vst3");

        pedal.disarm();
        assert!(!pedal.path.exists());
    }

    #[test]
    fn recover_blacklists_crashed_plugin() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));

        // Simulate a previous crash.
        std::fs::write(&pedal.path, "/plugins/crashy.vst3").unwrap();

        let mut db = MemoryCatalog::default();
        assert!(pedal.recover(&mut db));
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(!pedal.path.exists());
    }

    #[test]
    fn recover_returns_false_when_no_pedal() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let mut db = MemoryCatalog::default();

        assert!(!pedal.recover(&mut db));
    }

    // Mutation records: the edit that breaks each in-memory test.

    /// Mutation: drop the `trim` (the trailing newline becomes part of the path).
    #[test]
    fn parse_pedal_names_the_plugin_without_surrounding_whitespace() {
        assert_eq!(
            parse_pedal("/plugins/crashy.vst3"),
            Some(PathBuf::from("/plugins/crashy.vst3"))
        );
        assert_eq!(
            parse_pedal("  /plugins/crashy.vst3\n"),
            Some(PathBuf::from("/plugins/crashy.vst3"))
        );
    }

    /// Mutation: return `Some` for the empty path (a blank pedal would then
    /// blacklist the path `""`).
    #[test]
    fn parse_pedal_names_nothing_for_blank_text() {
        assert_eq!(parse_pedal(""), None);
        assert_eq!(parse_pedal(" \n\t "), None);
    }

    /// A path with interior spaces is one path. Mutation: split on whitespace.
    #[test]
    fn parse_pedal_keeps_interior_spaces() {
        assert_eq!(
            parse_pedal("/Library/My Plugins/A B.vst3"),
            Some(PathBuf::from("/Library/My Plugins/A B.vst3"))
        );
    }

    /// An unreadable or blank pedal is removed without blacklisting anything.
    /// Mutation: leave the file in place on the `None` path (every later start
    /// would find it again).
    #[test]
    fn recover_removes_a_blank_pedal_and_blacklists_nothing() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "  \n").unwrap();

        let mut db = MemoryCatalog::default();
        assert!(!pedal.recover(&mut db));
        assert!(db.is_empty());
        assert!(!pedal.path.exists());
    }
}
