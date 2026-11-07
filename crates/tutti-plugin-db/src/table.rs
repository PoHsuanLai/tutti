//! The file table: the database's source of truth.
//!
//! A row is keyed by file path and holds what the scan learned about that file.
//! A crash blacklists a file before any plugin id is known, so the file, not a
//! plugin, is the unit of record. Plugins are read out of the rows.
//!
//! The table belongs to one thread, the control thread that holds the
//! [`PluginDb`]. It never crosses a thread: a scan works on a clone and sends
//! [`Delta`]s back, and [`PluginDb::apply`] is the only code that changes a row.

use crate::facts::{self, FileFacts};
use crate::format::{PluginDescriptor, PluginFormat, PluginRole};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// What the database knows about one plugin file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRow {
    /// Which format's loader opens the file.
    pub format: PluginFormat,
    /// Seconds since the Unix epoch of the file's last modification when it was
    /// probed, or `None` if the file had no readable time. A row is out of date
    /// when the file's time differs from this one.
    pub mtime: Option<u64>,
    /// What the probe found.
    pub state: FileState,
}

/// What a probe made of a file.
///
/// A sum type, so a blacklisted file cannot also list plugins and a reason
/// cannot outlive the state that justifies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileState {
    /// The plugins the file holds. One per file today; a shell file may hold
    /// several.
    Probed(Vec<PluginDescriptor>),
    /// The file brought a scan down and will not be opened.
    Blacklisted {
        /// Why, so a host can report it and offer to load the file anyway.
        reason: String,
    },
}

impl FileRow {
    /// The row for a file that probed into `plugins`, stamped with `facts`' time.
    #[must_use]
    pub fn probed(format: PluginFormat, facts: &FileFacts, plugins: Vec<PluginDescriptor>) -> Self {
        Self {
            format,
            mtime: facts.mtime,
            state: FileState::Probed(plugins),
        }
    }

    /// The row for a blacklisted file, stamped with `facts`' time so that a
    /// reinstall or an update re-admits it. The format is the one the extension
    /// names, or VST3 when it names none.
    #[must_use]
    pub fn blacklisted(reason: String, facts: &FileFacts) -> Self {
        Self {
            format: facts.format.unwrap_or(PluginFormat::Vst3),
            mtime: facts.mtime,
            state: FileState::Blacklisted { reason },
        }
    }

    /// The plugins the file holds; none when it is blacklisted.
    #[must_use]
    pub fn plugins(&self) -> &[PluginDescriptor] {
        match &self.state {
            FileState::Probed(plugins) => plugins,
            FileState::Blacklisted { .. } => &[],
        }
    }

    /// Why the file is blacklisted, or `None` if it is not.
    #[must_use]
    pub fn blacklist_reason(&self) -> Option<&str> {
        match &self.state {
            FileState::Probed(_) => None,
            FileState::Blacklisted { reason } => Some(reason),
        }
    }
}

/// One change to the table: replace a file's row, or drop it.
///
/// A delta carries a whole row, so a reader sees a file's old row or its new one
/// and never a mixture.
#[derive(Debug, Clone, PartialEq)]
#[must_use = "a delta changes nothing until PluginDb::apply takes it"]
pub struct Delta {
    path: PathBuf,
    change: Change,
}

#[derive(Debug, Clone, PartialEq)]
enum Change {
    Put(FileRow),
    Remove,
}

impl Delta {
    /// Replace the row for `path`, or add one.
    pub fn put(path: impl Into<PathBuf>, row: FileRow) -> Self {
        Self {
            path: path.into(),
            change: Change::Put(row),
        }
    }

    /// Drop the row for `path`.
    pub fn remove(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            change: Change::Remove,
        }
    }

    /// The file the delta is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The row the delta puts, or `None` for a removal.
    #[must_use]
    pub fn row(&self) -> Option<&FileRow> {
        match &self.change {
            Change::Put(row) => Some(row),
            Change::Remove => None,
        }
    }
}

/// One plugin, read out of a file's row.
#[derive(Debug, Clone, Copy)]
pub struct PluginEntry<'a> {
    /// The file that holds the plugin.
    pub path: &'a Path,
    /// Which format's loader opens it.
    pub format: PluginFormat,
    /// What the probe reported.
    pub descriptor: &'a PluginDescriptor,
}

impl PluginEntry<'_> {
    /// What the plugin is (instrument, effect, ...), normalized across formats.
    #[must_use]
    pub fn role(&self) -> PluginRole {
        self.descriptor.class.role()
    }

    /// Whether the plugin is a note-driven sound source.
    ///
    /// A [`Generator`](PluginRole::Generator) is deliberately not an
    /// instrument: it makes sound without being played.
    #[must_use]
    pub fn is_instrument(&self) -> bool {
        self.role() == PluginRole::Instrument
    }
}

/// A blacklisted file and why it is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlacklistedFile<'a> {
    /// The file.
    pub path: &'a Path,
    /// The recorded reason.
    pub reason: &'a str,
}

/// The plugin database: a table of files, and the plugins read out of it.
///
/// Plain data with no I/O; `save` and `load` (feature `json`) are functions over
/// it. Every change goes through [`apply`](Self::apply).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginDb {
    files: BTreeMap<PathBuf, FileRow>,
}

impl PluginDb {
    /// An empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The only code that changes a row.
    pub fn apply(&mut self, delta: Delta) {
        match delta.change {
            Change::Put(row) => {
                self.files.insert(delta.path, row);
            }
            Change::Remove => {
                self.files.remove(&delta.path);
            }
        }
    }

    /// The row for `path`, if any.
    #[must_use]
    pub fn row(&self, path: &Path) -> Option<&FileRow> {
        self.files.get(path)
    }

    /// Every row, blacklisted ones included, in path order.
    pub fn rows(&self) -> impl Iterator<Item = (&Path, &FileRow)> {
        self.files.iter().map(|(path, row)| (path.as_path(), row))
    }

    /// The number of files with a row.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether no file has a row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The plugins of every file that is not blacklisted.
    pub fn plugins(&self) -> impl Iterator<Item = PluginEntry<'_>> {
        self.rows().flat_map(|(path, row)| {
            row.plugins().iter().map(move |descriptor| PluginEntry {
                path,
                format: row.format,
                descriptor,
            })
        })
    }

    /// The blacklisted files, so a UI can show what was hidden and why.
    pub fn blacklisted(&self) -> impl Iterator<Item = BlacklistedFile<'_>> {
        self.rows().filter_map(|(path, row)| {
            row.blacklist_reason()
                .map(|reason| BlacklistedFile { path, reason })
        })
    }

    /// Whether `path` has a row and it is blacklisted.
    #[must_use]
    pub fn is_blacklisted(&self, path: &Path) -> bool {
        self.row(path)
            .is_some_and(|row| row.blacklist_reason().is_some())
    }

    /// Whether `path` is blacklisted and its file has not changed since.
    ///
    /// The check a scan and an open must use, not the raw
    /// [`is_blacklisted`](Self::is_blacklisted): blacklisting records the file's
    /// time, so a reinstall or an update re-admits the plugin. A permanent
    /// blacklist would be wrong, because the pedal fires on a force-quit, a power
    /// loss and an out-of-memory kill as readily as on a plugin crash.
    #[must_use]
    pub fn is_blacklisted_and_unchanged(&self, path: &Path, facts: &FileFacts) -> bool {
        self.row(path)
            .is_some_and(|row| facts::is_blacklisted_and_unchanged(row, facts))
    }

    /// Whether `path` has no row or its file's time has changed since it was
    /// probed.
    #[must_use]
    pub fn needs_rescan(&self, path: &Path, facts: &FileFacts) -> bool {
        self.row(path)
            .is_none_or(|row| facts::needs_rescan(row, facts))
    }

    /// Blacklists `path` with a reason, stamped with the file's time in `facts`.
    pub fn blacklist(&mut self, path: &Path, reason: String, facts: &FileFacts) {
        self.apply(Delta::put(path, FileRow::blacklisted(reason, facts)));
    }

    /// Lifts the blacklist on `path` by dropping its row, so the next scan
    /// probes the file again. A blacklisted row holds no plugins to keep.
    ///
    /// Returns whether a blacklisted row was found. The inverse of
    /// [`blacklist`](Self::blacklist); without it one pedal misfire hides a
    /// plugin until the file is edited by hand.
    pub fn unblacklist(&mut self, path: &Path) -> bool {
        let found = self.is_blacklisted(path);
        if found {
            self.apply(Delta::remove(path));
        }
        found
    }

    /// Lifts every blacklist and returns the paths that were blacklisted.
    pub fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self.blacklisted().map(|b| b.path.to_path_buf()).collect();
        for path in &paths {
            self.apply(Delta::remove(path));
        }
        paths
    }

    /// Drops every row whose path is not in `present` and returns the dropped
    /// paths. `present` is the set of row paths that exist on disk, gathered by
    /// the caller.
    pub fn prune_missing(&mut self, present: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let missing = facts::missing(self.rows().map(|(path, _)| path), present);
        for path in &missing {
            self.apply(Delta::remove(path));
        }
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PluginClass;

    const T: u64 = 1_700_000_000;

    pub(crate) fn facts(mtime: u64) -> FileFacts {
        FileFacts {
            exists: true,
            mtime: Some(mtime),
            format: Some(PluginFormat::Vst3),
        }
    }

    fn row(mtime: u64) -> FileRow {
        FileRow::probed(
            PluginFormat::Vst3,
            &facts(mtime),
            vec![PluginDescriptor::new("id", "name", PluginClass::Unknown)],
        )
    }

    fn db_with(path: &str, row: FileRow) -> PluginDb {
        let mut db = PluginDb::new();
        db.apply(Delta::put(path, row));
        db
    }

    // Mutation records: the edit that breaks each test.

    /// Mutation: ignore `Remove`, or make `Put` keep the row already there
    /// (`entry().or_insert`).
    #[test]
    fn apply_puts_replaces_and_removes_a_row() {
        let mut db = db_with("/p/a.vst3", row(T));
        db.apply(Delta::put("/p/a.vst3", row(T + 1)));
        assert_eq!(db.len(), 1);
        assert_eq!(db.row(Path::new("/p/a.vst3")).unwrap().mtime, Some(T + 1));

        db.apply(Delta::remove("/p/a.vst3"));
        assert!(db.is_empty());
        db.apply(Delta::remove("/p/absent.vst3"));
        assert!(db.is_empty());
    }

    /// Mutation: return `false` for an absent row.
    #[test]
    fn a_path_with_no_row_needs_a_rescan() {
        assert!(PluginDb::new().needs_rescan(Path::new("/p/unknown.vst3"), &facts(T)));
    }

    /// Mutation: pass the facts through unread, or invert the comparison.
    #[test]
    fn needs_rescan_follows_the_facts_time() {
        let db = db_with("/p/a.vst3", row(T));
        assert!(!db.needs_rescan(Path::new("/p/a.vst3"), &facts(T)));
        assert!(db.needs_rescan(Path::new("/p/a.vst3"), &facts(T + 1)));
    }

    /// Mutation: ignore the facts and report the flag alone.
    #[test]
    fn a_blacklist_holds_only_while_the_file_is_unchanged() {
        let mut db = PluginDb::new();
        db.blacklist(Path::new("/p/a.vst3"), "crashed".into(), &facts(T));
        let a = Path::new("/p/a.vst3");
        assert!(db.is_blacklisted_and_unchanged(a, &facts(T)));
        assert!(!db.is_blacklisted_and_unchanged(a, &facts(T + 1)));
        assert!(db.is_blacklisted(a), "the flag outlives the file");
        assert!(!db.is_blacklisted_and_unchanged(Path::new("/p/other.vst3"), &facts(T)));
    }

    /// Mutation: stamp no time, or keep the old row's time, in `blacklist`.
    #[test]
    fn blacklist_stamps_the_facts_time_on_a_new_and_on_an_existing_row() {
        let mut db = PluginDb::new();
        db.blacklist(Path::new("/p/new.vst3"), "boom".into(), &facts(T));
        assert!(db.is_blacklisted_and_unchanged(Path::new("/p/new.vst3"), &facts(T)));

        db.apply(Delta::put("/p/old.vst3", row(T)));
        db.blacklist(Path::new("/p/old.vst3"), "boom".into(), &facts(T + 7));
        assert!(db.is_blacklisted_and_unchanged(Path::new("/p/old.vst3"), &facts(T + 7)));
        assert_eq!(db.len(), 2);
    }

    /// A blacklisted file lists no plugin and is listed as hidden. Mutation:
    /// make `PluginDb::blacklisted` list every row.
    #[test]
    fn a_blacklisted_file_lists_no_plugins_and_a_probed_one_lists_its_own() {
        let mut db = db_with("/p/good.vst3", row(T));
        db.apply(Delta::put("/p/bad.vst3", row(T)));
        db.blacklist(Path::new("/p/bad.vst3"), "boom".into(), &facts(T));

        let listed: Vec<_> = db.plugins().map(|p| p.path).collect();
        assert_eq!(listed, [Path::new("/p/good.vst3")]);
        let hidden: Vec<_> = db.blacklisted().collect();
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].reason, "boom");
    }

    /// A file may hold several plugins and each is listed under the file's path.
    /// Mutation: list only the first descriptor.
    #[test]
    fn a_file_with_several_plugins_lists_each() {
        let ids = ["a", "b"].map(|n| PluginDescriptor::new(n, n, PluginClass::Unknown));
        let db = db_with(
            "/p/shell.vst3",
            FileRow::probed(PluginFormat::Vst3, &facts(T), ids.to_vec()),
        );
        let names: Vec<_> = db.plugins().map(|p| p.descriptor.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// Lifting a blacklist drops the row, so the next scan probes the file
    /// whatever its time. Mutation: keep the row (only clear the flag), or
    /// report `true` for a healthy or absent row.
    #[test]
    fn unblacklist_drops_the_row_so_the_file_is_probed_again() {
        let mut db = PluginDb::new();
        db.blacklist(Path::new("/p/a.vst3"), "boom".into(), &facts(T));
        assert!(db.unblacklist(Path::new("/p/a.vst3")));
        assert!(db.row(Path::new("/p/a.vst3")).is_none());
        assert!(db.needs_rescan(Path::new("/p/a.vst3"), &facts(T)));
        assert!(!db.unblacklist(Path::new("/p/a.vst3")));

        db.apply(Delta::put("/p/healthy.vst3", row(T)));
        assert!(!db.unblacklist(Path::new("/p/healthy.vst3")));
        assert_eq!(db.len(), 1, "a healthy row is left alone");
    }

    /// Mutation: clear nothing, or clear healthy rows too.
    #[test]
    fn clear_blacklist_lifts_exactly_the_blacklisted_rows() {
        let mut db = db_with("/p/ok.vst3", row(T));
        db.blacklist(Path::new("/p/a.vst3"), "x".into(), &facts(T));
        db.blacklist(Path::new("/p/b.vst3"), "y".into(), &facts(T));

        let mut cleared = db.clear_blacklist();
        cleared.sort();
        assert_eq!(
            cleared,
            [PathBuf::from("/p/a.vst3"), PathBuf::from("/p/b.vst3")]
        );
        assert_eq!(db.len(), 1);
        assert_eq!(db.blacklisted().count(), 0);
    }

    /// Mutation: remove the rows that are present, or return an empty list.
    #[test]
    fn prune_missing_removes_and_reports_exactly_the_absent_rows() {
        let mut db = db_with("/p/here.vst3", row(T));
        db.blacklist(Path::new("/p/gone.vst3"), "x".into(), &facts(T));
        let present: HashSet<PathBuf> = [PathBuf::from("/p/here.vst3")].into();

        let pruned = db.prune_missing(&present);

        assert_eq!(pruned, vec![PathBuf::from("/p/gone.vst3")]);
        assert_eq!(db.len(), 1);
        assert!(db.row(Path::new("/p/here.vst3")).is_some());
    }
}
