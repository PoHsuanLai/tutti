//! JSON-backed [`PluginCatalog`] implementation.
//!
//! Stores [`PluginRecord`] entries in memory keyed by filesystem path and
//! persists them as a JSON list on [`PluginCatalog::flush`]. This is the
//! default catalog used by [`crate::catalog::Plugins`].

use super::catalog::PluginCatalog;
use super::record::PluginRecord;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Catalog backed by a JSON file on disk.
pub struct JsonCatalog {
    records: HashMap<PathBuf, PluginRecord>,
    db_path: PathBuf,
}

impl JsonCatalog {
    /// Load from `db_path`, or start empty if the file is missing or
    /// corrupt.
    pub fn load(db_path: impl Into<PathBuf>) -> Self {
        let db_path = db_path.into();
        let records = match std::fs::read_to_string(&db_path) {
            Ok(json) => match serde_json::from_str::<Vec<PluginRecord>>(&json) {
                Ok(list) => {
                    debug!("loaded {} plugin records from {:?}", list.len(), db_path);
                    list.into_iter().map(|r| (r.path.clone(), r)).collect()
                }
                Err(e) => {
                    warn!("plugin database corrupt, starting fresh: {e}");
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

    /// Empty catalog that writes to `db_path` on `flush`.
    pub fn empty(db_path: impl Into<PathBuf>) -> Self {
        Self {
            records: HashMap::new(),
            db_path: db_path.into(),
        }
    }

    /// Path where JSON is persisted.
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

    fn flush(&mut self) -> std::io::Result<()> {
        let records: Vec<&PluginRecord> = self.records.values().collect();
        let json = serde_json::to_string_pretty(&records).map_err(std::io::Error::other)?;

        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&self.db_path, json)?;
        debug!(
            "saved {} plugin records to {:?}",
            records.len(),
            self.db_path
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::catalog::CatalogExt;
    use super::super::record::{Blacklist, PluginFormat};
    use super::*;
    use super::super::record::{PluginClass, PluginDescriptor};
    use tempfile::TempDir;

    fn test_record(name: &str) -> PluginRecord {
        PluginRecord {
            path: PathBuf::from(format!("/plugins/{name}.vst3")),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::new(name, name, PluginClass::Unknown),
            modification_time: 1700000000,
            blacklist: Blacklist::Ok,
            extension_id: None,
            manifest_index: None,
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
            extension_id: None,
            manifest_index: None,
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
            extension_id: None,
            manifest_index: None,
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
            extension_id: None,
            manifest_index: None,
        });
        db.upsert(PluginRecord {
            path: PathBuf::from("/nonexistent/gone.vst3"),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: 0,
            blacklist: Blacklist::Ok,
            extension_id: None,
            manifest_index: None,
        });

        assert_eq!(db.len(), 2);
        db.prune_missing();
        assert_eq!(db.len(), 1);
        assert!(db.plugins().any(|r| r.path == existing));
    }

    #[test]
    fn remove_for_extension_drops_only_owned() {
        use crate::discovery::catalog::CatalogExt;

        let dir = TempDir::new().unwrap();
        let mut db = JsonCatalog::empty(dir.path().join("db.json"));

        // One standalone, two owned by ext-A, one owned by ext-B.
        let mut standalone = test_record("standalone");
        standalone.extension_id = None;
        let mut a1 = test_record("a1");
        a1.extension_id = Some("ext-a".into());
        let mut a2 = test_record("a2");
        a2.extension_id = Some("ext-a".into());
        let mut b1 = test_record("b1");
        b1.extension_id = Some("ext-b".into());

        db.upsert(standalone);
        db.upsert(a1);
        db.upsert(a2);
        db.upsert(b1);
        assert_eq!(db.len(), 4);

        let removed = db.remove_for_extension("ext-a");
        assert_eq!(removed.len(), 2);
        assert_eq!(db.len(), 2);
        assert!(db.iter().any(|r| r.descriptor.name == "standalone"));
        assert!(db.iter().any(|r| r.descriptor.name == "b1"));
    }

    #[test]
    fn load_corrupt_json_returns_empty() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db.json");
        std::fs::write(&db_path, "not valid json{{{").unwrap();

        let db = JsonCatalog::load(&db_path);
        assert!(db.is_empty());
    }
}
