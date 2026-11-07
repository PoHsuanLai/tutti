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

/// Parses the JSON a [`JsonCatalog`] writes into its records, keyed by path.
///
/// # Errors
///
/// Returns the parse error for text that is not a JSON list of records.
pub fn parse_records(text: &str) -> Result<HashMap<PathBuf, PluginRecord>, serde_json::Error> {
    let list: Vec<PluginRecord> = serde_json::from_str(text)?;
    Ok(list.into_iter().map(|r| (r.path.clone(), r)).collect())
}

/// A [`PluginCatalog`] that keeps records in memory and persists them as a JSON
/// file.
///
/// Records are keyed by plugin path. [`PluginCatalog::flush`] writes the whole
/// list atomically (a sibling temp file, fsynced, then renamed over the real
/// path), so a crash mid-write leaves the previous file intact. This is the
/// store behind `tutti_plugin::catalog::Plugins::with_json_catalog`.
#[derive(Debug)]
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
            Ok(json) => match parse_records(&json) {
                Ok(records) => {
                    debug!("loaded {} plugin records from {:?}", records.len(), db_path);
                    records
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

        if let Some(parent) = self.db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
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
    use super::super::facts::FileFacts;
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
    fn blacklisting() {
        let dir = TempDir::new().unwrap();
        let mut db = JsonCatalog::empty(dir.path().join("db.json"));
        let path = Path::new("/plugins/crashy.vst3");

        assert!(!db.is_blacklisted(path));
        db.blacklist(path, "crashed during scan".into(), &FileFacts::read(path));
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
            db.blacklist(&path, "segfault".into(), &FileFacts::read(&path));
            db.flush().unwrap();
        }

        let db = JsonCatalog::load(&db_path);
        assert!(db.is_blacklisted(&path));
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

    // Mutation records: the property each in-memory parse test pins and the edit
    // that breaks it.

    /// Mutation: key the map by anything but `record.path` (the lookup fails).
    #[test]
    fn parse_records_keys_by_path() {
        let json = serde_json::to_string(&[test_record("a"), test_record("b")]).unwrap();
        let records = parse_records(&json).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[&PathBuf::from("/plugins/a.vst3")].descriptor.name,
            "a"
        );
    }

    /// Mutation: swallow the parse error (`unwrap_or_default`), which would hide
    /// a corrupt file from the quarantine in `load`.
    #[test]
    fn parse_records_refuses_text_that_is_not_a_record_list() {
        assert!(parse_records("not valid json{{{").is_err());
        assert!(parse_records("{}").is_err());
        assert!(parse_records("").is_err());
    }

    /// Mutation: collect into a map without the path key, or drop duplicates the
    /// wrong way round (the later record must win, as `upsert` does).
    #[test]
    fn parse_records_keeps_the_later_of_two_records_with_one_path() {
        let first = test_record("a");
        let mut second = test_record("a");
        second.modification_time = 5;
        let json = serde_json::to_string(&[first, second]).unwrap();
        let records = parse_records(&json).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records.values().next().unwrap().modification_time, 5);
    }

    /// A record written before the blacklist field existed still parses, as
    /// healthy. Mutation: drop `#[serde(default)]` on `PluginRecord::blacklist`.
    #[test]
    fn parse_records_reads_a_record_with_no_blacklist_field_as_healthy() {
        let mut value = serde_json::to_value([test_record("a")]).unwrap();
        value[0].as_object_mut().unwrap().remove("blacklist");
        let records = parse_records(&value.to_string()).unwrap();
        assert!(!records.values().next().unwrap().blacklist.is_blacklisted());
    }
}
