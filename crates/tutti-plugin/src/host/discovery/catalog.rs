//! [`PluginCatalog`] — pluggable backing store for discovered plugins.
//!
//! The default implementation (behind the `json` feature) is
//! `JsonCatalog`, which persists records as a JSON file on disk. Users
//! who want SQLite, an in-memory map for tests, or any other store impl
//! this trait and pass it to `Plugins::with_catalog`.

use super::facts::{self, FileFacts};
use super::record::{Blacklist, PluginRecord};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A pluggable store for discovered plugin records.
///
/// Implement these primitives over any store (a JSON file, SQLite, an
/// in-memory map) and pass it to `Plugins::with_catalog`. The derived
/// operations (`is_blacklisted`, `needs_rescan`, `plugins`, `blacklist`,
/// `prune_missing`, …) come from the [`CatalogExt`] blanket extension. Those that
/// depend on the filesystem take a [`FileFacts`] read by the caller and never
/// touch the disk themselves.
pub trait PluginCatalog: Send + Sync {
    /// Returns the record for a filesystem path, if any.
    fn get(&self, path: &Path) -> Option<&PluginRecord>;

    /// Inserts or replaces a record, keyed by its path.
    fn upsert(&mut self, record: PluginRecord);

    /// Removes a record by path (no-op if absent).
    fn remove(&mut self, path: &Path);

    /// Iterates every record, including blacklisted ones.
    fn iter(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_>;

    /// Commits pending changes to durable storage.
    ///
    /// The default is a no-op, right for in-memory stores; file-backed stores
    /// override it to persist.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the store cannot be written.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Derived helpers built on top of [`PluginCatalog`].
///
/// Implemented for every `T: PluginCatalog + ?Sized`, so `Box<dyn
/// PluginCatalog>` and any concrete store both get them.
pub trait CatalogExt: PluginCatalog {
    /// Returns `true` if the plugin at `path` is recorded and blacklisted.
    fn is_blacklisted(&self, path: impl AsRef<Path>) -> bool {
        self.get(path.as_ref())
            .is_some_and(|r| r.blacklist.is_blacklisted())
    }

    /// Returns `true` if `path` is blacklisted **and** its file has not changed
    /// since the blacklist was recorded.
    ///
    /// This is the check the scanner must use, not the raw
    /// [`Self::is_blacklisted`]: blacklisting records the file's mtime, so a
    /// reinstall or a vendor update re-admits the plugin automatically. A
    /// blacklist without that escape hatch is permanent, and false positives
    /// are expected (the dead-man's pedal fires on force-quit, power loss, and
    /// OOM-kill just as readily as on a real plugin crash).
    fn is_blacklisted_and_unchanged(&self, path: impl AsRef<Path>, facts: &FileFacts) -> bool {
        self.get(path.as_ref())
            .is_some_and(|r| facts::is_blacklisted_and_unchanged(r, facts))
    }

    /// Clears the blacklist flag on one record, keeping its other fields.
    ///
    /// Returns `true` if a blacklisted record was found and cleared. The
    /// record is marked for rescan, since a blacklisted record carries only a
    /// placeholder descriptor.
    ///
    /// The inverse of [`Self::blacklist`]. Without it a single pedal misfire
    /// hides a plugin forever, remediable only by hand-editing the DB.
    fn unblacklist(&mut self, path: impl AsRef<Path>) -> bool {
        let Some(existing) = self.get(path.as_ref()) else {
            return false;
        };
        if !existing.blacklist.is_blacklisted() {
            return false;
        }
        let cleared = PluginRecord {
            blacklist: Blacklist::Ok,
            // Force a rescan: the descriptor on a blacklisted stub is a
            // placeholder, so the plugin must be probed again before use.
            modification_time: 0,
            ..existing.clone()
        };
        self.upsert(cleared);
        true
    }

    /// Clears every blacklist entry, returning the paths that were cleared.
    fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        let blacklisted: Vec<PathBuf> = self
            .iter()
            .filter(|r| r.blacklist.is_blacklisted())
            .map(|r| r.path.clone())
            .collect();
        for path in &blacklisted {
            self.unblacklist(path);
        }
        blacklisted
    }

    /// Iterates the blacklisted records, so a UI can show what was hidden and
    /// why instead of the plugin just being absent.
    fn blacklisted(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
        Box::new(self.iter().filter(|r| r.blacklist.is_blacklisted()))
    }

    /// Returns `true` if `path` has no record or its file's modification time
    /// (in `facts`) has changed since the last scan.
    fn needs_rescan(&self, path: impl AsRef<Path>, facts: &FileFacts) -> bool {
        self.get(path.as_ref())
            .is_none_or(|record| facts::needs_rescan(record, facts))
    }

    /// Iterates the records that are not blacklisted.
    fn plugins(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
        Box::new(self.iter().filter(|r| !r.blacklist.is_blacklisted()))
    }

    /// Marks a path as blacklisted with a reason, inserting a stub record if
    /// the path has none yet, and stamps it with the file's time in `facts`.
    fn blacklist(&mut self, path: impl AsRef<Path>, reason: String, facts: &FileFacts) {
        let path = path.as_ref();
        let record = facts::blacklist_record(self.get(path), path, reason, facts);
        self.upsert(record);
    }

    /// Removes every record whose path is not in `present` and returns the
    /// removed paths.
    ///
    /// `present` is the set of record paths that exist on disk, gathered by the
    /// caller.
    fn prune_missing(&mut self, present: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let missing = facts::missing(self.iter(), present);
        for path in &missing {
            self.remove(path);
        }
        missing
    }

    /// Total record count (including blacklisted).
    fn len(&self) -> usize {
        self.iter().count()
    }

    /// `true` when no records are stored.
    fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

impl<T: PluginCatalog + ?Sized> CatalogExt for T {}

/// In-memory [`PluginCatalog`] for tests. Deliberately feature-independent:
/// the scanner and pedal tests exercise scanning and crash recovery, neither
/// of which is about JSON, so they must not require the `json` feature to
/// compile. Doubles as a worked example that the trait is genuinely pluggable.
#[cfg(test)]
#[derive(Default)]
pub(super) struct MemoryCatalog {
    records: std::collections::HashMap<PathBuf, PluginRecord>,
}

#[cfg(test)]
impl PluginCatalog for MemoryCatalog {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::discovery::record::{PluginDescriptor, PluginFormat};

    const T: u64 = 1_700_000_000;

    fn facts(mtime: u64) -> FileFacts {
        FileFacts {
            exists: true,
            mtime: Some(mtime),
            format: Some(PluginFormat::Vst3),
        }
    }

    fn rec(path: &str, mtime: u64, blacklist: Blacklist) -> PluginRecord {
        PluginRecord {
            path: PathBuf::from(path),
            format: PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: mtime,
            blacklist,
        }
    }

    fn bad() -> Blacklist {
        Blacklist::Blacklisted {
            reason: "crashed".into(),
        }
    }

    // Mutation records: the edit that breaks each test.

    /// Mutation: return `false` for an absent record.
    #[test]
    fn a_path_with_no_record_needs_a_rescan() {
        let db = MemoryCatalog::default();
        assert!(db.needs_rescan("/p/unknown.vst3", &facts(T)));
    }

    /// Mutation: pass `FileFacts` through unread (compare to `0`), or invert the
    /// comparison.
    #[test]
    fn needs_rescan_follows_the_facts_time() {
        let mut db = MemoryCatalog::default();
        db.upsert(rec("/p/a.vst3", T, Blacklist::Ok));
        assert!(!db.needs_rescan("/p/a.vst3", &facts(T)));
        assert!(db.needs_rescan("/p/a.vst3", &facts(T + 1)));
    }

    /// Mutation: ignore the facts and report `is_blacklisted()` alone.
    #[test]
    fn a_blacklist_holds_only_while_the_file_is_unchanged() {
        let mut db = MemoryCatalog::default();
        db.upsert(rec("/p/a.vst3", T, bad()));
        assert!(db.is_blacklisted_and_unchanged("/p/a.vst3", &facts(T)));
        assert!(!db.is_blacklisted_and_unchanged("/p/a.vst3", &facts(T + 1)));
        assert!(db.is_blacklisted("/p/a.vst3"), "the flag outlives the file");
        assert!(!db.is_blacklisted_and_unchanged("/p/other.vst3", &facts(T)));
    }

    /// Mutation: stamp `0` or the old record's time in `blacklist`.
    #[test]
    fn blacklist_stamps_the_facts_time_on_a_stub_and_on_an_existing_record() {
        let mut db = MemoryCatalog::default();
        db.blacklist("/p/new.vst3", "boom".into(), &facts(T));
        assert!(db.is_blacklisted_and_unchanged("/p/new.vst3", &facts(T)));

        db.upsert(rec("/p/old.vst3", T, Blacklist::Ok));
        db.blacklist("/p/old.vst3", "boom".into(), &facts(T + 7));
        assert!(db.is_blacklisted_and_unchanged("/p/old.vst3", &facts(T + 7)));
        assert_eq!(db.len(), 2);
    }

    /// Mutation: set `modification_time` to anything but `0` in `unblacklist`
    /// (e.g. `1`), or skip the clearing.
    #[test]
    fn unblacklist_forces_a_rescan_without_reading_the_disk() {
        let mut db = MemoryCatalog::default();
        db.blacklist("/p/a.vst3", "boom".into(), &facts(T));
        assert!(db.unblacklist("/p/a.vst3"));
        assert_eq!(db.get(Path::new("/p/a.vst3")).unwrap().modification_time, 0);
        assert!(db.needs_rescan("/p/a.vst3", &facts(T)));
        assert!(!db.unblacklist("/p/a.vst3"));
        assert!(!db.unblacklist("/p/absent.vst3"));
    }

    /// Mutation: remove the records that are present, or return an empty list.
    #[test]
    fn prune_missing_removes_and_reports_exactly_the_absent_records() {
        let mut db = MemoryCatalog::default();
        db.upsert(rec("/p/here.vst3", T, Blacklist::Ok));
        db.upsert(rec("/p/gone.vst3", T, bad()));
        let present: HashSet<PathBuf> = [PathBuf::from("/p/here.vst3")].into();

        let pruned = db.prune_missing(&present);

        assert_eq!(pruned, vec![PathBuf::from("/p/gone.vst3")]);
        assert_eq!(db.len(), 1);
        assert!(db.get(Path::new("/p/here.vst3")).is_some());
    }
}
