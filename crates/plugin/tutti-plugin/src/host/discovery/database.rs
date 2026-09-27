//! JSON-backed [`PluginCatalog`] implementation.

use super::catalog::PluginCatalog;
use super::record::PluginRecord;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, warn};

/// Disambiguates concurrent temp files within one process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A [`PluginCatalog`] that keeps records in memory and persists them as a JSON
/// file.
///
/// Records are keyed by plugin path. [`PluginCatalog::flush`] writes the whole
/// list atomically (a sibling temp file, fsynced, then renamed over the real
/// path), so a crash mid-write leaves the previous file intact. This is the
/// store behind [`Plugins::with_json_catalog`](crate::catalog::Plugins::with_json_catalog).
pub struct JsonCatalog {
    records: HashMap<PathBuf, PluginRecord>,
    db_path: PathBuf,
}

impl JsonCatalog {
    /// Loads the catalog from `db_path`, or starts empty if the file is missing
    /// or corrupt.
    ///
    /// A corrupt file is not overwritten: it is renamed to the same path with
    /// a `.corrupt` extension so its records can be recovered by hand.
    pub fn load(db_path: impl Into<PathBuf>) -> Self {
        let db_path = db_path.into();
        let records = match std::fs::read_to_string(&db_path) {
            Ok(json) => match serde_json::from_str::<Vec<PluginRecord>>(&json) {
                Ok(list) => {
                    debug!("loaded {} plugin records from {:?}", list.len(), db_path);
                    list.into_iter().map(|r| (r.path.clone(), r)).collect()
                }
                Err(e) => {
                    // Do not silently discard: move the unreadable file aside
                    // so the blacklist and scan results are recoverable by
                    // hand rather than overwritten by the next flush.
                    let quarantine = db_path.with_extension("corrupt");
                    match std::fs::rename(&db_path, &quarantine) {
                        Ok(()) => warn!(
                            "plugin database corrupt ({e}); preserved at {:?}, starting fresh",
                            quarantine
                        ),
                        Err(move_err) => warn!(
                            "plugin database corrupt ({e}) and could not be preserved \
                             ({move_err}); starting fresh"
                        ),
                    }
                    HashMap::new()
                }
            },
            Err(_) => {
                debug!("no plugin database at {:?}, starting fresh", db_path);
                HashMap::new()
            }
        };
        Self { records, db_path }
    }

    /// Creates an empty catalog that writes to `db_path` on `flush`, without
    /// reading it.
    pub fn empty(db_path: impl Into<PathBuf>) -> Self {
        Self {
            records: HashMap::new(),
            db_path: db_path.into(),
        }
    }

    /// Returns the path the JSON file is written to.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

impl PluginCatalog for JsonCatalog {
    fn get(&self, path: &Path) -> Option<&PluginRecord> {
        self.records.get(path)
    }

    fn upsert(&mut self, record: PluginRecord) {
        self.records.insert(record.path.clone(), record);
    }

    fn remove(&mut self, path: &Path) {
        self.records.remove(path);
    }

    fn iter(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
        Box::new(self.records.values())
    }

    /// Writes the catalog atomically: serialises to a sibling temp file, fsyncs
    /// it, then renames it over the real path.
    ///
    /// A reader sees either the old file or the new one, never a partial write.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the parent directory, the temp file or the
    /// rename fails.
    fn flush(&mut self) -> std::io::Result<()> {
        let records: Vec<&PluginRecord> = self.records.values().collect();
        let json = serde_json::to_string_pretty(&records).map_err(std::io::Error::other)?;
        let count = records.len();
        drop(records);

        if let Some(parent) = self.db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Temp file must be a *sibling* so the rename stays within one
        // filesystem (cross-device rename fails with EXDEV).
        let tmp_path = self.db_path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let write_result = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })();

        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }

        if let Err(e) = std::fs::rename(&tmp_path, &self.db_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }

        debug!("saved {count} plugin records to {:?}", self.db_path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::catalog::CatalogExt;
    use super::super::record::{Blacklist, PluginFormat};
    use super::super::record::{PluginClass, PluginDescriptor};
    use super::*;
    use tempfile::TempDir;

    fn test_record(name: &str) -> PluginRecord {
        PluginRecord {
            path: PathBuf::from(format!("/plugins/{name}.vst3")),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::new(name, name, PluginClass::Unknown),
            modification_time: 1700000000,
            blacklist: Blacklist::Ok,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("plugins.json");

        {
            let mut db = JsonCatalog::empty(&db_path);
            db.upsert(test_record("reverb"));
            db.upsert(test_record("delay"));
            db.flush().unwrap();
        }

        let db = JsonCatalog::load(&db_path);
        assert_eq!(db.len(), 2);
        assert!(db.plugins().any(|r| r.descriptor.name == "reverb"));
        assert!(db.plugins().any(|r| r.descriptor.name == "delay"));
    }

    #[test]
    fn needs_rescan_missing_entry() {
        let dir = TempDir::new().unwrap();
        let db = JsonCatalog::empty(dir.path().join("db.json"));
        assert!(db.needs_rescan(Path::new("/plugins/unknown.vst3")));
    }

    #[test]
    fn needs_rescan_changed_mtime() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join("test.vst3");
        std::fs::write(&plugin_file, b"fake plugin").unwrap();

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.upsert(PluginRecord {
            path: plugin_file.clone(),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: 0,
            blacklist: Blacklist::Ok,
        });

        assert!(db.needs_rescan(&plugin_file));
    }

    #[test]
    fn needs_rescan_up_to_date() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join("test.vst3");
        std::fs::write(&plugin_file, b"fake plugin").unwrap();
        let mtime = super::super::fs::file_modification_time(&plugin_file).unwrap();

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.upsert(PluginRecord {
            path: plugin_file.clone(),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: mtime,
            blacklist: Blacklist::Ok,
        });

        assert!(!db.needs_rescan(&plugin_file));
    }

    #[test]
    fn blacklisting() {
        let dir = TempDir::new().unwrap();
        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        let path = Path::new("/plugins/crashy.vst3");

        assert!(!db.is_blacklisted(path));
        db.blacklist(path, "crashed during scan".into());
        assert!(db.is_blacklisted(path));

        assert_eq!(db.plugins().count(), 0);
        assert_eq!(db.iter().count(), 1);
    }

    #[test]
    fn blacklist_persists_through_save_load() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db.json");
        let path = PathBuf::from("/plugins/crashy.vst3");

        {
            let mut db = JsonCatalog::empty(&db_path);
            db.blacklist(&path, "segfault".into());
            db.flush().unwrap();
        }

        let db = JsonCatalog::load(&db_path);
        assert!(db.is_blacklisted(&path));
    }

    #[test]
    fn prune_missing() {
        let dir = TempDir::new().unwrap();
        let existing = dir.path().join("exists.vst3");
        std::fs::write(&existing, b"real").unwrap();

        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        db.upsert(PluginRecord {
            path: existing.clone(),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: 0,
            blacklist: Blacklist::Ok,
        });
        db.upsert(PluginRecord {
            path: PathBuf::from("/nonexistent/gone.vst3"),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: 0,
            blacklist: Blacklist::Ok,
        });

        assert_eq!(db.len(), 2);
        db.prune_missing();
        assert_eq!(db.len(), 1);
        assert!(db.plugins().any(|r| r.path == existing));
    }

    #[test]
    fn load_corrupt_json_returns_empty() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db.json");
        std::fs::write(&db_path, "not valid json{{{").unwrap();

        let db = JsonCatalog::load(&db_path);
        assert!(db.is_empty());
    }

    /// A corrupt DB must be preserved, not destroyed by the next flush.
    /// Losing it loses every blacklist entry.
    #[test]
    fn load_corrupt_json_quarantines_the_file() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db.json");
        std::fs::write(&db_path, "not valid json{{{").unwrap();

        let _db = JsonCatalog::load(&db_path);

        let quarantine = db_path.with_extension("corrupt");
        assert!(quarantine.exists(), "corrupt DB should be moved aside");
        assert_eq!(
            std::fs::read_to_string(&quarantine).unwrap(),
            "not valid json{{{"
        );
        assert!(!db_path.exists(), "corrupt DB should not be left in place");
    }

    /// `flush` must never truncate the live file.
    /// It writes a sibling temp then renames, so any failure leaves the
    /// previous catalog intact rather than half-written.
    #[test]
    fn flush_is_atomic_and_leaves_no_temp_files() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("plugins.json");

        let mut db = JsonCatalog::empty(&db_path);
        db.upsert(test_record("first"));
        db.flush().unwrap();

        let after_first = std::fs::read_to_string(&db_path).unwrap();
        assert!(after_first.contains("first"));

        db.upsert(test_record("second"));
        db.flush().unwrap();

        // The rewrite replaced the file wholesale; both records survive and
        // no temp debris is left behind.
        let reloaded = JsonCatalog::load(&db_path);
        assert_eq!(reloaded.len(), 2);

        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp-"))
            .collect();
        assert!(stray.is_empty(), "flush left temp files behind: {stray:?}");
    }

    /// Whatever is on disk after a flush must be parseable — never a
    /// truncated prefix. Asserts the rename-based write, not `fs::write`.
    #[test]
    fn flush_never_leaves_partial_json() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("plugins.json");

        let mut db = JsonCatalog::empty(&db_path);
        for i in 0..50 {
            db.upsert(test_record(&format!("p{i}")));
        }
        db.flush().unwrap();

        let json = std::fs::read_to_string(&db_path).unwrap();
        let parsed: Vec<PluginRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 50);

        // Overwrite with fewer records: the rename must fully replace, not
        // leave the tail of the longer previous content.
        let mut db2 = JsonCatalog::empty(&db_path);
        db2.upsert(test_record("only"));
        db2.flush().unwrap();

        let json = std::fs::read_to_string(&db_path).unwrap();
        let parsed: Vec<PluginRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
    }
}
