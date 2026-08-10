//! Dead-man's pedal: a sentinel file whose presence signals the scanner
//! crashed while probing a specific plugin. Next startup blacklists that
//! plugin so the scan can make progress.

use super::catalog::{CatalogExt, PluginCatalog};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

pub(super) struct Pedal {
    path: PathBuf,
}

impl Pedal {
    /// Pedal file lives at `path` for the duration of a single probe.
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Record the plugin that is about to be probed.
    pub(super) fn arm(&self, plugin: &Path) -> std::io::Result<()> {
        let mut f = std::fs::File::create(&self.path)?;
        f.write_all(plugin.to_string_lossy().as_bytes())?;
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
        let Ok(crashed_path) = std::fs::read_to_string(&self.path) else {
            let _ = std::fs::remove_file(&self.path);
            return false;
        };
        let crashed_path = crashed_path.trim();
        if crashed_path.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return false;
        }
        let p = PathBuf::from(crashed_path);
        warn!(
            "dead-man's pedal found: blacklisting {:?} (crashed during previous scan)",
            p
        );
        catalog.blacklist(&p, "crashed during previous scan".into());
        // Persist the blacklist immediately so it survives another crash.
        let _ = catalog.flush();
        let _ = std::fs::remove_file(&self.path);
        true
    }
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
}
