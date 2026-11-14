//! The [`Catalog`]: scans plugin directories, keeps what it found and says what
//! is blacklisted. It does not open a plugin: the session loads one, and a load asks the catalog
//! whether it may ([`Catalog::check`],
//! `PluginLoad::with_catalog`).
//!
//! The caller supplies the persistence and which directories to scan: this
//! crate has no opinion on OS conventions, app names or file formats. The
//! persistence is a closure from `&PluginDb` to a typed `Result`; with the
//! `serde` feature a `PluginDb` is `Serialize` and `Deserialize`, so the app
//! picks the format and where the bytes live.
//!
//! ```no_run
//! # fn ex() -> Result<(), tutti_plugin_db::ScanBusy> {
//! use tutti_plugin::catalog::{Catalog, CatalogConfig, PersistError};
//! use tutti_plugin_db::PluginDb;
//! use std::path::PathBuf;
//!
//! let mut catalog = Catalog::new(
//!     PluginDb::new(), // or the one the app deserialized
//!     CatalogConfig::new(
//!         PathBuf::from("/my/app/state"),
//!         vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")],
//!     ),
//!     |db| {
//!         // Write `db` wherever the app keeps it.
//!         let _ = db;
//!         Ok::<(), PersistError>(())
//!     },
//! );
//! let scanned = catalog.scan()?;
//! println!("{} new", scanned.new);
//! # Ok(())
//! # }
//! ```
//!
//! # Threads
//!
//! A [`Catalog`] is owned by one thread, the one that calls its methods. A
//! rescan ([`spawn_scan`](Catalog::spawn_scan)) works on a snapshot on its
//! own thread and sends deltas back; [`poll_scan`](Catalog::poll_scan)
//! applies them on the owner. The catalog is readable throughout, and nothing
//! here runs on or reaches the audio thread.

use crate::host::db_probe::{measure_record, probe_plugin, probe_plugin_measured};
use crate::host::plugin_ref::PluginRef;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use tutti_plugin_db::{
    BlacklistedFile, Delta, FileFacts, PluginDb, PluginEntry, Scan, ScanBusy, ScanPoll, ScanResult,
    Scanner,
};
use tutti_plugin_ipc::CatalogConfig;
use tutti_plugin_types::PluginDescriptor;
use tutti_plugin_types::{BridgeError, BridgeResult};

/// Why the persistence a catalog was given could not store the database.
///
/// The persistence closure builds one from whatever failed (an I/O error, a
/// serializer's error); the catalog hands it back from [`Catalog::save`] and
/// [`Catalog::take_persist_error`].
#[derive(Debug, thiserror::Error)]
#[error("the plugin database could not be persisted: {source}")]
pub struct PersistError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl PersistError {
    /// A failure of the persistence, from its underlying error.
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

/// How a catalog stores its database: the host's own closure.
type Persist = Arc<dyn Fn(&PluginDb) -> Result<(), PersistError> + Send + Sync>;

/// The failure of the last save a catalog started itself, until it is taken.
type Unreported = Arc<Mutex<Option<PersistError>>>;

/// A catalog of scanned plugins that can list and blacklist them, and says
/// whether a load may open one ([`check`](Self::check)).
///
/// Wraps a [`PluginDb`] and the persistence the host supplies
/// ([`Catalog::new`]). Every change is in memory until [`save`](Self::save) is
/// called, or a scan or a crash recovery saves it.
pub struct Catalog {
    db: PluginDb,
    scanner: Scanner,
    config: CatalogConfig,
    persist: Persist,
    unreported: Unreported,
    /// The write a finished rescan started on its own thread.
    pending_save: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.finish_pending_save();
    }
}

/// Runs `persist` for a save the catalog starts itself. A failure is logged and
/// kept in `unreported` for [`Catalog::take_persist_error`]; a success clears
/// an older failure, since the newer table is what is stored.
fn persist_unattended(
    persist: &Persist,
    unreported: &Unreported,
    db: &PluginDb,
) -> Result<(), String> {
    let outcome = persist(db);
    let mut slot = unreported.lock().unwrap_or_else(PoisonError::into_inner);
    match outcome {
        Ok(()) => {
            *slot = None;
            Ok(())
        }
        Err(e) => {
            tracing::warn!("{e}");
            let message = e.to_string();
            *slot = Some(e);
            Err(message)
        }
    }
}

/// The key a path has in the database: what a scan keys the file by, so an
/// alias and the real path are one row, also when the file is gone (its parent
/// is resolved and its name kept).
fn key(path: &Path) -> PathBuf {
    tutti_plugin_db::canonical_path(path)
}

impl Catalog {
    /// Creates a catalog over `db`, which `persist` stores.
    ///
    /// `persist` receives the table and stores it wherever the host keeps it. It
    /// runs on the owning thread for [`save`](Self::save) and for a crash
    /// recovery, and on a short-lived thread for the save after a rescan, which
    /// gets a copy of the table. A crash recovery removes its sentinel only
    /// after `persist` succeeds.
    #[must_use]
    pub fn new(
        db: PluginDb,
        config: CatalogConfig,
        persist: impl Fn(&PluginDb) -> Result<(), PersistError> + Send + Sync + 'static,
    ) -> Self {
        let scanner = Scanner::new(config.pedal_path(), probe_plugin);
        Self {
            db,
            scanner,
            config,
            persist: Arc::new(persist),
            unreported: Unreported::default(),
            pending_save: None,
        }
    }

    /// Makes this catalog's scans **measure** the plugins they probe: each new or changed file
    /// is also rendered, in the probe's own
    /// process, and its descriptors carry [`measured`](PluginDescriptor::measured).
    /// Off by default: measuring renders each plugin at two configurations, which
    /// costs seconds a plain probe does not, and a plugin that crashes or hangs
    /// while rendering is blacklisted like one that crashes in its load. A scan
    /// skips a file whose time has not changed, so it never measures one it has
    /// already probed: [`measure`](Self::measure) does.
    #[must_use]
    pub fn with_measuring(mut self) -> Self {
        self.scanner = Scanner::new(self.config.pedal_path(), probe_plugin_measured);
        self
    }

    /// The database, for reading rows this catalog has no method for.
    ///
    /// Its rows are keyed by [`canonical_path`](tutti_plugin_db::canonical_path),
    /// which is what a scan and every method of this catalog key by. The table
    /// itself matches paths exactly, so look a file up by its canonical path
    /// (an alias finds nothing); the catalog's own `is_blacklisted`, `blacklist`
    /// and the rest canonicalize for you.
    pub fn db(&self) -> &PluginDb {
        &self.db
    }

    /// The save the scanner and the pedal's recovery run: `persist`, with its
    /// failure kept for [`take_persist_error`](Self::take_persist_error).
    fn autosave(&self) -> impl FnOnce(&PluginDb) -> std::io::Result<()> + use<> {
        let (persist, unreported) = (Arc::clone(&self.persist), Arc::clone(&self.unreported));
        move |db| persist_unattended(&persist, &unreported, db).map_err(std::io::Error::other)
    }

    /// Starts a scan of the plugin directories on a background thread.
    ///
    /// Recovers a crashed previous scan first, then hands the scanner a
    /// snapshot. This catalog stays whole and usable while the scan runs; call
    /// [`poll_scan`](Self::poll_scan) on the owning thread (a frame tick, a
    /// UI loop) to apply what the scan found. See [`rescan`](Self::scan) for
    /// the blocking form.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a rescan, or a recovery, is already using this
    /// catalog's pedal: its worker has not exited yet.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the scanner thread.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tutti_plugin::catalog::Catalog;
    /// # use tutti_plugin_db::ScanPoll;
    /// # fn ex(mut plugins: Catalog) -> Result<(), tutti_plugin_db::ScanBusy> {
    /// let scan = plugins.spawn_scan()?;
    /// loop {
    ///     while let Ok(progress) = scan.progress_rx.try_recv() {
    ///         println!("{}/{}", progress.current, progress.total);
    ///     }
    ///     match plugins.poll_scan(&scan) {
    ///         ScanPoll::Running => std::thread::yield_now(),
    ///         ScanPoll::Done(result) => break println!("{} new", result.new),
    ///         ScanPoll::Died => break,
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn spawn_scan(&mut self) -> std::result::Result<Scan, ScanBusy> {
        self.finish_pending_save();
        let save = self.autosave();
        self.scanner
            .spawn(&mut self.db, &self.config.scan_dirs, save)
    }

    /// Applies what a rescan has found so far and reports where it stands.
    /// Never blocks.
    ///
    /// A finished scan is saved when this returns [`ScanPoll::Done`]: the table
    /// is copied here and written on a short-lived thread, so the fsync is not
    /// on the owner's thread. A failed save is logged and kept for
    /// [`take_persist_error`](Self::take_persist_error). The write is finished
    /// before the next scan, `save` or drop. Drop the
    /// [`Scan`] once it reports `Done` or `Died`.
    pub fn poll_scan(&mut self, scan: &Scan) -> ScanPoll {
        let poll = scan.poll(&mut self.db);
        if matches!(poll, ScanPoll::Done(_)) {
            self.save_in_background();
        }
        poll
    }

    /// Scans the plugin directories on the calling thread and returns the
    /// summary.
    ///
    /// Blocks while each new or changed plugin is probed in a subprocess. The
    /// in-memory catalog is updated, and saved, before this returns; a failed
    /// save is kept for [`take_persist_error`](Self::take_persist_error).
    ///
    /// # Errors
    ///
    /// [`ScanBusy`], as for [`spawn_scan`](Self::spawn_scan).
    pub fn scan(&mut self) -> std::result::Result<ScanResult, ScanBusy> {
        self.finish_pending_save();
        let save = self.autosave();
        let result = self
            .scanner
            .scan(&mut self.db, &self.config.scan_dirs, save)?;
        // A failure stays for `take_persist_error`; the scan's result is the table.
        let _ = persist_unattended(&self.persist, &self.unreported, &self.db);
        Ok(result)
    }

    /// Writes a copy of the table on its own thread. The clone is the only cost
    /// on the owner's thread. A write still in flight is finished first, so two
    /// writes never race and the newer table is the one stored.
    fn save_in_background(&mut self) {
        self.finish_pending_save();
        let copy = Arc::new(self.db.clone());
        let for_thread = (
            Arc::clone(&copy),
            Arc::clone(&self.persist),
            Arc::clone(&self.unreported),
        );
        let spawned = std::thread::Builder::new()
            .name("plugin-db-save".into())
            .spawn(move || {
                let (copy, persist, unreported) = for_thread;
                let _ = persist_unattended(&persist, &unreported, &copy);
            });
        match spawned {
            Ok(handle) => self.pending_save = Some(handle),
            Err(e) => {
                tracing::warn!(
                    "could not start a thread for the database save, saving inline: {e}"
                );
                let _ = persist_unattended(&self.persist, &self.unreported, &copy);
            }
        }
    }

    fn finish_pending_save(&mut self) {
        if let Some(handle) = self.pending_save.take()
            && handle.join().is_err()
        {
            let panicked = PersistError::new("the persistence panicked");
            tracing::warn!("{panicked}");
            *self
                .unreported
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(panicked);
        }
    }

    /// The failure of the last save this catalog started itself (after
    /// [`scan`](Self::scan), after a rescan's [`poll_scan`](Self::poll_scan), or
    /// in a [`recover_crash`](Self::recover_crash)), or `None` if it succeeded or
    /// the failure was already taken. A later successful save clears an older
    /// failure: the table it stored is newer. Waits for a rescan's write still in
    /// flight.
    pub fn take_persist_error(&mut self) -> Option<PersistError> {
        self.finish_pending_save();
        self.unreported
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Iterates the refs and descriptors of all non-blacklisted plugins.
    pub fn iter(&self) -> impl Iterator<Item = (PluginRef, &PluginDescriptor)> {
        self.db
            .plugins()
            .map(|p| (PluginRef::from(&p), p.descriptor))
    }

    /// Iterates all non-blacklisted plugins with their file and format.
    ///
    /// Use this when you need `format` or `role`, for example to group plugins
    /// by format in a browser.
    pub fn entries(&self) -> impl Iterator<Item = PluginEntry<'_>> {
        self.db.plugins()
    }

    /// Returns the ref of the first non-blacklisted plugin with this display
    /// name.
    pub fn find(&self, name: &str) -> Option<PluginRef> {
        self.db
            .plugins()
            .find(|p| p.descriptor.name == name)
            .map(|p| PluginRef::from(&p))
    }

    /// Returns the descriptor of a non-blacklisted plugin: the one in the
    /// ref's file with the ref's id, or the file's first when the ref has
    /// none.
    pub fn info(&self, plugin: &PluginRef) -> Option<&PluginDescriptor> {
        // Rows are keyed by canonical path; a ref keeps the path it was given
        // (a link's extension is what names its format).
        let file = key(plugin.path());
        self.db
            .plugins()
            .find(|p| p.path == file && plugin.id().is_none_or(|id| p.descriptor.id == id))
            .map(|p| p.descriptor)
    }

    /// Iterates the blacklisted files, including the recorded reason.
    ///
    /// [`iter`](Self::iter) and [`entries`](Self::entries) exclude these; use
    /// this to show hidden plugins and offer to [`unblacklist`](Self::unblacklist)
    /// them.
    pub fn blacklisted(&self) -> impl Iterator<Item = BlacklistedFile<'_>> {
        self.db.blacklisted()
    }

    /// Returns `true` if [`check`](Self::check) would refuse the plugin at
    /// `path`.
    ///
    /// A blacklisted plugin whose file has changed since it was blacklisted
    /// (a reinstall or update) is not refused, so this returns `false` for it.
    /// The check is mtime-aware ([`PluginDb::is_blacklisted_and_unchanged`]).
    #[must_use]
    pub fn is_blacklisted(&self, path: impl AsRef<Path>) -> bool {
        let path = key(path.as_ref());
        self.db
            .is_blacklisted_and_unchanged(&path, &FileFacts::read(&path))
    }

    /// Whether a load may open `plugin`: the catalog's one refusal.
    ///
    /// The path need not have been scanned. The check is mtime-aware, so a
    /// reinstall or update re-admits a plugin without clearing anything. The
    /// catalog does not open a plugin; the session's load asks this
    /// (`PluginLoad::with_catalog`), so a blacklisted plugin is refused when it
    /// is loaded, before any server starts.
    ///
    /// # Errors
    ///
    /// [`BridgeError::Blacklisted`] with the recorded reason when the plugin's
    /// file is blacklisted and unchanged; a host can offer to load it anyway
    /// (a load that does not name the catalog) or to
    /// [`unblacklist`](Self::unblacklist) it.
    pub fn check(&self, plugin: &PluginRef) -> BridgeResult<()> {
        let path = plugin.path();
        if !self.is_blacklisted(path) {
            return Ok(());
        }
        let reason = self
            .db
            .row(&key(path))
            .and_then(|row| row.blacklist_reason())
            .unwrap_or("no reason recorded")
            .to_string();
        Err(BridgeError::Blacklisted {
            path: path.to_path_buf(),
            reason,
        })
    }

    /// Blacklists a plugin by path, hiding it from `iter`, `entries` and `find`
    /// and making [`check`](Self::check) refuse it.
    pub fn blacklist(&mut self, path: impl AsRef<Path>, reason: impl Into<String>) {
        let path = key(path.as_ref());
        self.db
            .blacklist(&path, reason.into(), &FileFacts::read(&path));
    }

    /// Clears one blacklist entry so the plugin is probed again on the next
    /// scan.
    ///
    /// The file's row is dropped, so the plugin is **absent** from
    /// [`iter`](Self::iter), [`entries`](Self::entries) and [`find`](Self::find)
    /// until that scan has probed it again.
    ///
    /// Returns `true` if a blacklisted file was found and cleared. Offer this
    /// to users: crash recovery also blacklists after a force-quit, power loss
    /// or out-of-memory kill, so false positives are expected.
    ///
    /// The file's entry in the crash sentinel is dropped too (one a failed or
    /// absent save kept), so the next recovery does not blacklist it again. While
    /// a rescan is running the sentinel is that scan's, and is left alone: the
    /// entry is then dropped by the next `unblacklist`.
    pub fn unblacklist(&mut self, path: impl AsRef<Path>) -> bool {
        let path = key(path.as_ref());
        self.forget_in_pedal(&path);
        self.db.unblacklist(&path)
    }

    fn forget_in_pedal(&self, file: &Path) {
        if self.scanner.forget(file).is_err() {
            tracing::warn!("a rescan is running; not touching its crash sentinel for {file:?}");
        }
    }

    /// Clears every blacklist entry and returns the paths cleared.
    pub fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        self.db.clear_blacklist()
    }

    /// Removes one file, blacklisted or not.
    ///
    /// Unlike [`Self::unblacklist`] this forgets the plugin entirely, so the
    /// next scan treats it as new.
    pub fn remove(&mut self, path: impl AsRef<Path>) {
        let path = key(path.as_ref());
        self.forget_in_pedal(&path);
        self.db.apply(Delta::remove(path));
    }

    /// Measures the file `plugin` names and records the result: the tail and
    /// latency each of its plugins reports, against what it renders, in each descriptor's
    /// [`measured`](PluginDescriptor::measured).
    /// The row is replaced, so the file is probed again too.
    ///
    /// **Blocking**: the probe renders each plugin at two configurations, up to
    /// [`DEFAULT_MEASURE_TIMEOUT`](crate::catalog::DEFAULT_MEASURE_TIMEOUT). A
    /// frame-driven host runs [`measure_record`] on a worker and passes the
    /// result to [`apply`](Self::apply).
    ///
    /// # Errors
    ///
    /// Any error of [`measure_record`]: the probe cannot be found or spawned, the
    /// plugin would not load, or it crashed or hung while rendering.
    pub fn measure(&mut self, plugin: &PluginRef) -> BridgeResult<()> {
        let delta = measure_record(plugin.path())?;
        self.apply(delta);
        Ok(())
    }

    /// Applies a delta to the catalog, such as the one `probe_record`
    /// returns.
    ///
    /// Run `probe_record` on any thread (it needs no catalog) and pass the
    /// result here. Call [`save`](Self::save) to persist.
    pub fn apply(&mut self, delta: Delta) {
        self.db.apply(delta);
    }

    /// Writes the in-memory catalog through the catalog's persistence.
    ///
    /// # Errors
    ///
    /// The [`PersistError`] the persistence returned; what it stored before is
    /// then as the persistence leaves it.
    pub fn save(&mut self) -> Result<(), PersistError> {
        self.finish_pending_save();
        let outcome = (self.persist)(&self.db);
        if outcome.is_ok() {
            *self
                .unreported
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
        outcome
    }

    /// Blacklists the plugin a previous scan was probing when it died.
    ///
    /// A scan writes a sentinel file before each probe and removes it after, so
    /// a sentinel left over means the scanning process went down: a plugin
    /// crash, but equally a force-quit, power loss or out-of-memory kill. False
    /// positives are therefore expected; offer
    /// [`unblacklist`](Self::unblacklist) to the user.
    ///
    /// Every scan already calls this. Call it directly to report "a plugin
    /// brought your last session down" at startup without a full rescan.
    /// Idempotent, and a no-op when no sentinel is present. Returns the files
    /// that were blacklisted.
    ///
    /// The sentinel is removed only after the blacklist is saved. If the
    /// persistence fails, the blacklist lives in memory, the sentinel stays armed
    /// (the next start blacklists the plugin again rather than forgetting it) and
    /// the failure is kept for [`take_persist_error`](Self::take_persist_error).
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a rescan is in flight: its armed sentinel names a live
    /// probe, not a crash.
    pub fn recover_crash(&mut self) -> std::result::Result<Vec<PathBuf>, ScanBusy> {
        self.finish_pending_save();
        let save = self.autosave();
        self.scanner.recover_crash(&mut self.db, save)
    }

    /// Removes rows whose plugin file does not exist and returns their paths.
    ///
    /// A scan only adds what it finds, so an uninstalled plugin keeps its
    /// row until this is called. Scans do not prune on their own because a
    /// temporarily unavailable directory (an unmounted volume, a network share)
    /// would lose all its rows and need a full re-probe.
    pub fn prune_missing(&mut self) -> Vec<PathBuf> {
        let present: HashSet<PathBuf> = self
            .db
            .rows()
            .filter(|(path, _)| FileFacts::read(path).exists)
            .map(|(path, _)| path.to_path_buf())
            .collect();
        self.db.prune_missing(&present)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::plugin::Plugin;
    use tutti_plugin_db::{FileRow, PluginFormat, ScanPoll};
    use tutti_types::SampleRate;

    /// A blacklisted row whose `mtime` matches what the file system reports,
    /// i.e. the file has not changed since it was recorded.
    fn blacklisted_row(path: &Path, reason: &str) -> FileRow {
        FileRow::blacklisted(reason.to_string(), &FileFacts::read(path))
    }

    fn catalog_with(path: &Path, row: FileRow) -> Catalog {
        let mut db = PluginDb::new();
        // Rows are keyed by canonical path, as a scan keys them.
        db.apply(Delta::put(key(path), row));
        Catalog::new(
            db,
            CatalogConfig::new("/nonexistent", crate::catalog::NO_SCAN_DIRS),
            |_| Ok(()),
        )
    }

    /// **A catalog names each plugin of a bundle, and `info` finds the one the
    /// ref names**, not the file's first: two plugins in one file are two refs
    /// with their own ids, and a ref with no id is the first.
    #[test]
    fn a_bundle_of_two_plugins_is_two_refs_and_info_finds_each() {
        use tutti_plugin_db::{FileFacts, PluginFormat};
        use tutti_plugin_types::{PluginClass, PluginDescriptor};
        let path = std::env::temp_dir().join("tutti-two-plugins.clap");
        std::fs::write(&path, b"x").unwrap();
        let describe = |id: &str| PluginDescriptor::new(id, id, PluginClass::Unknown);
        let row = FileRow::probed(
            PluginFormat::Clap,
            &FileFacts::read(&path),
            vec![describe("one"), describe("two")],
        );
        let plugins = catalog_with(&path, row);

        let refs: Vec<_> = plugins.iter().map(|(r, _)| r).collect();
        let ids: Vec<_> = refs.iter().map(|r| r.id().unwrap()).collect();
        assert_eq!(ids, ["one", "two"]);
        assert_eq!(plugins.info(&refs[1]).map(|d| d.id.as_str()), Some("two"));
        assert_eq!(plugins.find("two"), Some(refs[1].clone()));
        let by_file = PluginRef::file(&path).unwrap();
        assert_eq!(plugins.info(&by_file).map(|d| d.id.as_str()), Some("one"));
        // A ref through a link names the scanned file (macOS's temp dir is
        // one: `/var` links to `/private/var`).
        #[cfg(unix)]
        {
            let link = std::env::temp_dir().join("tutti-two-plugins-link.clap");
            std::fs::remove_file(&link).ok();
            std::os::unix::fs::symlink(&path, &link).unwrap();
            let by_link = PluginRef::file(&link).unwrap();
            assert_eq!(plugins.info(&by_link).map(|d| d.id.as_str()), Some("one"));
            std::fs::remove_file(&link).ok();
        }

        std::fs::remove_file(&path).ok();
    }

    /// The check refuses a plugin the scanner recorded as a crasher,
    /// and says which one and why.
    ///
    /// The reason is the whole point of carrying it: a host has to be able to
    /// name the plugin and offer to load it anyway.
    #[test]
    fn a_blacklisted_plugin_is_refused_with_its_recorded_reason() {
        let dir = std::env::temp_dir().join("tutti-bl-refused");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Crasher.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        let plugins = catalog_with(&path, blacklisted_row(&path, "SIGSEGV during probe"));

        match plugins.check(&PluginRef::file(&path).unwrap()) {
            Err(BridgeError::Blacklisted { path: p, reason }) => {
                assert_eq!(p, path);
                assert_eq!(reason, "SIGSEGV during probe");
            }
            other => panic!("expected a Blacklisted refusal, got {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    /// A blacklisted plugin whose file has since changed is admitted again.
    ///
    /// Blacklisting records the file's mtime precisely so a reinstall or a
    /// vendor update lifts it without the host clearing anything. Checking the
    /// raw flag instead would hide the plugin permanently, and false positives
    /// are expected — the scanner's pedal fires on a force-quit or an OOM kill
    /// as readily as on a real crash.
    #[test]
    fn a_blacklisted_plugin_is_readmitted_once_its_file_changes() {
        let dir = std::env::temp_dir().join("tutti-bl-readmit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Updated.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        // Record the blacklist against a *different* mtime than the file has,
        // which is what a reinstall produces.
        let mut row = blacklisted_row(&path, "crashed once");
        row.mtime = row.mtime.map(|t| t.saturating_sub(1_000));
        let plugins = catalog_with(&path, row);

        plugins
            .check(&PluginRef::file(&path).unwrap())
            .expect("a changed file must not be refused as blacklisted");

        std::fs::remove_file(&path).ok();
    }

    /// The query and the check agree about a plugin whose file has changed.
    ///
    /// Were the query to read the raw flag while `check` used the mtime-aware
    /// check, an updated plugin would report blacklisted and open anyway. A
    /// browser greying entries out on the query then hides a plugin it could
    /// load — permanently, because the reinstall meant to lift the blacklist is
    /// exactly what makes the two diverge.
    ///
    /// Asserting *agreement* rather than a fixed answer is what makes this
    /// survive a future change to the staleness rule: whatever `check` decides,
    /// the query has to say the same thing.
    #[test]
    fn the_query_and_the_check_agree_after_the_file_changes() {
        let dir = std::env::temp_dir().join("tutti-bl-stale");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Updated.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        // Unchanged first: both halves must still refuse.
        let fresh = catalog_with(&path, blacklisted_row(&path, "SIGSEGV during probe"));
        assert!(
            fresh.is_blacklisted(&path),
            "unchanged and blacklisted: the query must say so"
        );
        assert!(
            matches!(
                fresh.check(&PluginRef::file(&path).unwrap()),
                Err(BridgeError::Blacklisted { .. })
            ),
            "unchanged and blacklisted: the check must refuse"
        );

        // Now the vendor ships a fix. Backdating the *record* rather than
        // rewriting the file is what the sibling test does, and for a reason:
        // the modification time is second-resolution, so a rewrite inside
        // the same second leaves the times equal and the test proves nothing.
        let mut stale = blacklisted_row(&path, "SIGSEGV during probe");
        stale.mtime = stale.mtime.map(|t| t.saturating_sub(1_000));
        let plugins = catalog_with(&path, stale);

        assert!(
            !plugins.is_blacklisted(&path),
            "a changed file lifts the blacklist — this is the half that was wrong"
        );
        plugins
            .check(&PluginRef::file(&path).unwrap())
            .expect("the check must admit it too, so the two answers agree");

        std::fs::remove_file(&path).ok();
    }

    /// The plain door has no catalog, so it has no blacklist to consult.
    ///
    /// This is the opt-in boundary: `Plugin::open` is a file API and stays one.
    #[test]
    fn the_unguarded_door_does_not_consult_a_blacklist() {
        let dir = std::env::temp_dir().join("tutti-bl-unguarded");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Crasher.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        let plugins = catalog_with(&path, blacklisted_row(&path, "SIGSEGV during probe"));
        assert!(plugins.is_blacklisted(&path), "fixture should be recorded");

        let err = Plugin::open(None, &PluginRef::file(&path).unwrap(), SampleRate::SR_48K)
            .expect_err("the fixture is not a loadable plugin");
        assert!(
            !matches!(err, BridgeError::Blacklisted { .. }),
            "Plugin::open has no catalog and cannot refuse on one, got {err:?}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// The real edge: `prune_missing` stats each row's path and removes only
    /// the absent one.
    #[test]
    fn prune_missing_removes_only_rows_whose_file_is_gone() {
        let dir = tempfile::TempDir::new().unwrap();
        let here = dir.path().join("here.vst3");
        std::fs::write(&here, b"x").unwrap();
        let gone = dir.path().join("gone.vst3");

        let mut db = PluginDb::new();
        for path in [&here, &gone] {
            db.apply(Delta::put(
                path,
                FileRow::probed(
                    PluginFormat::Vst3,
                    &FileFacts::read(path),
                    vec![PluginDescriptor::default()],
                ),
            ));
        }
        let mut plugins = Catalog::new(
            db,
            CatalogConfig::new("/nonexistent", crate::catalog::NO_SCAN_DIRS),
            |_| Ok(()),
        );

        assert_eq!(plugins.prune_missing(), vec![gone]);
        assert_eq!(plugins.iter().count(), 1);
    }

    /// A catalog's persistence gets the blacklist before the pedal
    /// goes, and the pedal goes after.
    #[test]
    fn the_persistence_receives_the_recovered_blacklist_before_the_pedal_goes() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path(), crate::catalog::NO_SCAN_DIRS);
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();
        let pedal = config.pedal_path();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let key_path = key(&crashy);

        let mut plugins = Catalog::new(PluginDb::new(), config, move |db| {
            log.lock()
                .unwrap()
                .push((db.is_blacklisted(&key_path), pedal.exists()));
            Ok(())
        });
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));

        assert_eq!(*seen.lock().unwrap(), vec![(true, true)]);
        assert!(!plugins.config.pedal_path().exists());
    }

    /// A persistence that always fails cannot have saved the blacklist, so the
    /// pedal stays armed, the blacklist is in memory only, and the failure
    /// reaches the caller as the persistence's own error.
    #[test]
    fn a_failing_persistence_keeps_the_pedal_armed_and_reports_its_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path(), crate::catalog::NO_SCAN_DIRS);
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();

        let mut plugins = Catalog::new(PluginDb::new(), config.clone(), |_| {
            Err(PersistError::new("disk full"))
        });
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));
        let recovery = plugins
            .take_persist_error()
            .expect("the recovery's save failed");
        assert!(recovery.to_string().contains("disk full"), "{recovery}");
        assert_eq!(plugins.take_persist_error().map(|_| ()), None, "taken once");
        // A rescan recovers first; it must not delete what it could not save.
        plugins.scan().unwrap();
        let scan = plugins
            .take_persist_error()
            .expect("the scan's save failed");
        assert!(scan.to_string().contains("disk full"), "{scan}");

        assert!(plugins.db().is_blacklisted(&key(&crashy)));
        assert!(config.pedal_path().exists());
        let saved = plugins.save().unwrap_err();
        assert!(saved.to_string().contains("disk full"), "{saved}");
        assert!(std::error::Error::source(&saved).is_some());
    }

    /// The save after a rescan is off the owner's thread: `poll_scan` reports
    /// `Done` while the persistence is still running, and the next `save` waits for
    /// it, and dropping the catalog waits for it.
    #[test]
    fn the_save_after_a_rescan_does_not_block_the_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path(), crate::catalog::NO_SCAN_DIRS);
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(8);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::clone(&writes);
        let mut plugins = Catalog::new(PluginDb::new(), config, move |_| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
            done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        let began = std::time::Instant::now();
        let scan = plugins.spawn_scan().unwrap();
        let end = loop {
            match plugins.poll_scan(&scan) {
                ScanPoll::Running => std::thread::yield_now(),
                other => break other,
            }
        };
        assert!(matches!(end, ScanPoll::Done(_)), "{end:?}");
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the save started");
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);

        release_tx.send(()).unwrap();
        drop(scan);
        drop(plugins); // joins the save
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A plugin the user unblacklists, or removes, is not put back by the next
    /// recovery from an entry a failed save kept in the sentinel.
    #[test]
    fn unblacklist_and_remove_drop_the_retained_sentinel_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path(), crate::catalog::NO_SCAN_DIRS);
        let (a, b) = (dir.path().join("A.vst3"), dir.path().join("B.vst3"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let entries = format!("{}\0{}", a.display(), b.display());
        std::fs::write(config.pedal_path(), entries).unwrap();
        let mut plugins = Catalog::new(PluginDb::new(), config.clone(), |_| {
            Err(PersistError::new("no disk"))
        });
        assert_eq!(plugins.recover_crash().unwrap().len(), 2);
        assert!(config.pedal_path().exists(), "kept: the save failed");

        assert!(plugins.unblacklist(&a));
        plugins.scan().unwrap();
        assert!(!plugins.is_blacklisted(&a), "the user's unblacklist held");
        assert!(plugins.is_blacklisted(&b), "the other entry is still kept");

        plugins.remove(&b);
        plugins.scan().unwrap();
        assert!(plugins.db().row(&key(&b)).is_none(), "the removal held");
        assert!(!config.pedal_path().exists());
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// An alias of a blacklisted plugin is refused like the plugin itself: the
        /// catalog keys by canonical path.
        #[test]
        fn a_blacklist_set_through_an_alias_holds_for_the_real_path() {
            let dir = tempfile::TempDir::new().unwrap();
            let real = dir.path().join("Real.vst3");
            std::fs::write(&real, b"x").unwrap();
            let alias = dir.path().join("Alias.vst3");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            let mut plugins = Catalog::new(
                PluginDb::new(),
                CatalogConfig::new(dir.path(), crate::catalog::NO_SCAN_DIRS),
                |_| Ok(()),
            );

            plugins.blacklist(&alias, "boom");

            assert!(plugins.is_blacklisted(&real));
            assert!(plugins.is_blacklisted(&alias));
            assert_eq!(plugins.db().len(), 1);
        }

        /// `remove` and `unblacklist` through an alias reach the row of a file that
        /// is gone: the key resolves the parent and keeps the name.
        #[test]
        fn remove_through_an_alias_reaches_a_deleted_file() {
            let dir = tempfile::TempDir::new().unwrap();
            let real_dir = dir.path().join("real");
            std::fs::create_dir(&real_dir).unwrap();
            let file = real_dir.join("Gone.vst3");
            std::fs::write(&file, b"x").unwrap();
            let alias_dir = dir.path().join("alias");
            std::os::unix::fs::symlink(&real_dir, &alias_dir).unwrap();
            let mut plugins = Catalog::new(
                PluginDb::new(),
                CatalogConfig::new(dir.path(), crate::catalog::NO_SCAN_DIRS),
                |_| Ok(()),
            );
            plugins.blacklist(&file, "boom");
            plugins.blacklist(real_dir.join("Also.vst3"), "boom");
            std::fs::remove_file(&file).unwrap();

            plugins.remove(alias_dir.join("Gone.vst3"));
            assert!(plugins.unblacklist(alias_dir.join("Also.vst3")));

            assert_eq!(plugins.db().len(), 0);
        }

        /// The whole catalog works through a root that is a link, as macOS's temp
        /// dir is: a sentinel entry, a blacklist, a lookup and an unblacklist all
        /// name the file by the link path and meet at the canonical key. Linux has
        /// no such temp dir, so the link is made here.
        #[test]
        fn a_catalog_under_a_symlinked_root_agrees_with_itself() {
            let dir = tempfile::TempDir::new().unwrap();
            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            std::fs::write(real.join("A.vst3"), b"x").unwrap();
            std::fs::write(real.join("B.vst3"), b"x").unwrap();
            let root = dir.path().join("root");
            std::os::unix::fs::symlink(&real, &root).unwrap();
            let config = CatalogConfig::new(&root, crate::catalog::NO_SCAN_DIRS);
            std::fs::write(config.pedal_path(), root.join("A.vst3").to_str().unwrap()).unwrap();
            let mut plugins = Catalog::new(PluginDb::new(), config, |_| Ok(()));

            let crashed = plugins.recover_crash().unwrap();
            plugins.blacklist(root.join("B.vst3"), "boom");

            let real_a = real.canonicalize().unwrap().join("A.vst3");
            assert_eq!(crashed, vec![real_a.clone()]);
            for name in ["A.vst3", "B.vst3"] {
                assert!(plugins.is_blacklisted(root.join(name)), "{name} by link");
                assert!(plugins.is_blacklisted(real.join(name)), "{name} by path");
            }
            assert!(plugins.unblacklist(root.join("A.vst3")));
            assert!(!plugins.is_blacklisted(real_a));
            assert_eq!(plugins.db().len(), 1);
        }
    }
}
