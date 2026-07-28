//! [`PluginCatalog`] — pluggable backing store for discovered plugins.
//!
//! The default implementation (behind the `json` feature) is
//! `JsonCatalog`, which persists records as a JSON file on disk. Users
//! who want SQLite, an in-memory map for tests, or any other store impl
//! this trait and pass it to `Plugins::with_catalog`.

use super::fs::{file_modification_time, format_from_path};
use super::record::{Blacklist, PluginFormat, PluginRecord};
use std::path::{Path, PathBuf};

/// Minimal primitives a catalog must provide. Derived operations
/// (`is_blacklisted`, `needs_rescan`, `plugins`, `blacklist`,
/// `prune_missing`) live on the [`CatalogExt`] blanket extension.
pub trait PluginCatalog: Send + Sync {
    /// Look up a record by filesystem path.
    fn get(&self, path: &Path) -> Option<&PluginRecord>;

    /// Insert or replace a record.
    fn upsert(&mut self, record: PluginRecord);

    /// Remove a record by path (no-op if absent).
    fn remove(&mut self, path: &Path);

    /// Iterate every record, including blacklisted ones.
    fn iter(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_>;

    /// Commit pending changes to durable storage. Default: no-op — right
    /// for in-memory impls. File-backed impls override to persist.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Derived helpers built on top of [`PluginCatalog`]. Automatically
/// implemented for every `T: PluginCatalog + ?Sized`, so `Box<dyn
/// PluginCatalog>` and any concrete impl both get them free.
pub trait CatalogExt: PluginCatalog {
    /// `true` if the plugin at `path` is recorded and blacklisted.
    fn is_blacklisted(&self, path: &Path) -> bool {
        self.get(path).is_some_and(|r| r.blacklist.is_blacklisted())
    }

    /// `true` if `path` is blacklisted **and** its on-disk bytes have not
    /// changed since the blacklist was recorded.
    ///
    /// This is the check the scanner must use, not the raw
    /// [`Self::is_blacklisted`]: blacklisting records the file's mtime, so a
    /// reinstall or a vendor update re-admits the plugin automatically. A
    /// blacklist without that escape hatch is permanent, and false positives
    /// are expected (the dead-man's pedal fires on force-quit, power loss, and
    /// OOM-kill just as readily as on a real plugin crash).
    fn is_blacklisted_and_unchanged(&self, path: &Path) -> bool {
        self.get(path).is_some_and(|r| {
            r.blacklist.is_blacklisted()
                && file_modification_time(path).unwrap_or(0) == r.modification_time
        })
    }

    /// Clear the blacklist flag on one record, keeping its other fields.
    /// Returns `true` if a blacklisted record was found and cleared.
    ///
    /// The inverse of [`Self::blacklist`]. Without it a single pedal misfire
    /// hides a plugin forever, remediable only by hand-editing the DB.
    fn unblacklist(&mut self, path: &Path) -> bool {
        let Some(existing) = self.get(path) else {
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

    /// Clear every blacklist entry. Returns the paths that were cleared.
    /// The bulk escape hatch for "my plugins vanished after a crash".
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

    /// Iterate the blacklisted records, so a UI can show what was hidden and
    /// why instead of the plugin just being absent.
    fn blacklisted(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
        Box::new(self.iter().filter(|r| r.blacklist.is_blacklisted()))
    }

    /// `true` if `path` is absent or its on-disk mtime has changed since
    /// the last scan.
    fn needs_rescan(&self, path: &Path) -> bool {
        self.get(path).is_none_or(|record| {
            let current_mtime = file_modification_time(path).unwrap_or(0);
            current_mtime != record.modification_time
        })
    }

    /// Iterate non-blacklisted records.
    fn plugins(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
        Box::new(self.iter().filter(|r| !r.blacklist.is_blacklisted()))
    }

    /// Mark a path as blacklisted with a reason. Inserts a stub record if
    /// the path isn't present yet.
    fn blacklist(&mut self, path: &Path, reason: String) {
        let record = match self.get(path) {
            Some(existing) => PluginRecord {
                blacklist: Blacklist::Blacklisted { reason },
                ..existing.clone()
            },
            None => PluginRecord {
                path: path.to_path_buf(),
                format: format_from_path(path).unwrap_or(PluginFormat::Vst3),
                descriptor: super::record::PluginDescriptor::default(),
                modification_time: file_modification_time(path).unwrap_or(0),
                blacklist: Blacklist::Blacklisted { reason },
                extension_id: None,
                manifest_index: None,
            },
        };
        self.upsert(record);
    }

    /// Drop every record whose path no longer exists on disk.
    fn prune_missing(&mut self) {
        let missing: Vec<PathBuf> = self
            .iter()
            .filter(|r| !r.path.exists())
            .map(|r| r.path.clone())
            .collect();
        for path in missing {
            self.remove(&path);
        }
    }

    /// Total record count (including blacklisted).
    fn len(&self) -> usize {
        self.iter().count()
    }

    /// `true` when no records are stored.
    fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Drop every record bundled by the named extension. Used at
    /// extension deactivation: standalone scanner-discovered records
    /// (`extension_id == None`) are unaffected. Returns the paths
    /// removed so the caller can also drop running instances.
    fn remove_for_extension(&mut self, ext_id: &str) -> Vec<PathBuf> {
        let owned: Vec<PathBuf> = self
            .iter()
            .filter(|r| r.extension_id.as_deref() == Some(ext_id))
            .map(|r| r.path.clone())
            .collect();
        for path in &owned {
            self.remove(path);
        }
        owned
    }
}

impl<T: PluginCatalog + ?Sized> CatalogExt for T {}
