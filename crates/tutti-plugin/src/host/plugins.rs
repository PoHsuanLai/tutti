//! The [`Plugins`] catalog: scans plugin directories and opens what it found.
//!
//! The caller supplies where the DB lives and which directories to scan —
//! this crate has no opinion on OS conventions or app names.
//!
//! ```no_run
//! # #[cfg(feature = "json")] {
//! use tutti_plugin::catalog::{CatalogConfig, Plugins};
//! use std::path::PathBuf;
//!
//! let plugins = Plugins::load(CatalogConfig::new(
//!     PathBuf::from("/my/app/plugin-db.json"),
//!     vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")],
//! ))
//! .with_fresh_scan();
//! # }
//! ```
//!
//! The database is a plain [`PluginDb`]; a host that
//! keeps its rows elsewhere (a CRDT, an application database) builds one from
//! them and persists it itself, without the `json` feature:
//!
//! ```no_run
//! use std::path::PathBuf;
//! use tutti_plugin::catalog::{CatalogConfig, Plugins};
//! use tutti_plugin_db::PluginDb;
//!
//! let mut db = PluginDb::new();
//! // ... db.apply(delta) for each row the host stored ...
//! let plugins = Plugins::with_db(
//!     db,
//!     CatalogConfig::new(PathBuf::from("/unused"), vec![PathBuf::from("/usr/lib/vst3")]),
//! );
//! ```
//!
//! # Threads
//!
//! A [`Plugins`] is owned by one thread, the one that calls its methods. A
//! rescan ([`spawn_rescan`](Plugins::spawn_rescan)) works on a snapshot on its
//! own thread and sends deltas back; [`poll_rescan`](Plugins::poll_rescan)
//! applies them on the owner. The catalog is readable throughout, and nothing
//! here runs on or reaches the audio thread.
//!
//! Audio knobs are separate and default sensibly; set them with
//! [`Plugins::with_audio_config`] when the defaults don't fit.

use crate::host::db_probe::{probe_plugin, probe_record};
use crate::host::plugin::Plugin;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tutti_plugin_db::{
    BlacklistedFile, Delta, FileFacts, PluginDb, PluginEntry, Scan, ScanBusy, ScanPoll, ScanResult,
    Scanner,
};
use tutti_plugin_ipc::{AudioConfig, CatalogConfig};
use tutti_plugin_ipc::{BridgeError, Result};
use tutti_plugin_types::MainThread;
use tutti_plugin_types::PluginDescriptor;
use tutti_types::SampleRate;

/// Identifies a plugin in a [`Plugins`] catalog by its file path.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PluginId(PathBuf);

impl PluginId {
    /// Creates an id from a plugin file path.
    ///
    /// The id only resolves through [`Plugins::info`] when a record with this
    /// exact path is in the catalog.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Returns the plugin file path this id wraps.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl From<PathBuf> for PluginId {
    fn from(path: PathBuf) -> Self {
        Self(path)
    }
}

/// How a catalog writes its database out: the host's own, or the JSON file.
type Saver = Arc<dyn Fn(&PluginDb) -> std::io::Result<()> + Send + Sync>;

/// A catalog of scanned plugins that can list, blacklist and open them.
///
/// Wraps a [`PluginDb`]; [`Plugins::with_db`] takes your own, and
/// `Plugins::load` reads the versioned JSON file when the `json` feature is
/// enabled. Every change is in memory until [`save`](Self::save) is called.
///
/// With the `json` feature the store is the JSON file at `config.db_path`.
/// Without it there is no store until the host gives one
/// ([`with_saver`](Self::with_saver)); until then `save` fails with
/// [`Unsupported`](std::io::ErrorKind::Unsupported), and so does the save a
/// crash recovery needs, which keeps the pedal armed (see
/// [`recover_crash`](Self::recover_crash)).
pub struct Plugins {
    db: PluginDb,
    scanner: Scanner,
    config: CatalogConfig,
    audio: AudioConfig,
    saver: Option<Saver>,
    /// The write a finished rescan started on its own thread.
    pending_save: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugins")
            .field("config", &self.config)
            .field("audio", &self.audio)
            .finish_non_exhaustive()
    }
}

impl Drop for Plugins {
    fn drop(&mut self) {
        self.finish_pending_save();
    }
}

/// The saver `with_db` installs: the JSON file at the config's path.
#[cfg(feature = "json")]
fn file_saver(config: &CatalogConfig) -> Option<Saver> {
    let path = config.db_path.clone();
    Some(Arc::new(move |db| tutti_plugin_db::save(db, &path)))
}

#[cfg(not(feature = "json"))]
fn file_saver(_config: &CatalogConfig) -> Option<Saver> {
    None
}

fn write_through(saver: Option<&Saver>, db: &PluginDb) -> std::io::Result<()> {
    match saver {
        Some(save) => save(db),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this catalog has no store: enable `json` or call `Plugins::with_saver`",
        )),
    }
}

/// The key a path has in the database: what a scan keys the file by, so an
/// alias and the real path are one row, also when the file is gone (its parent
/// is resolved and its name kept).
fn key(path: &Path) -> PathBuf {
    tutti_plugin_db::canonical_path(path)
}

impl Plugins {
    /// Creates a catalog over a database you built.
    ///
    /// Audio settings default ([`AudioConfig::default`]); override with
    /// [`Plugins::with_audio_config`].
    #[must_use]
    pub fn with_db(db: PluginDb, config: CatalogConfig) -> Self {
        let scanner = Scanner::new(config.pedal_path(), probe_plugin);
        let saver = file_saver(&config);
        Self {
            db,
            scanner,
            config,
            audio: AudioConfig::default(),
            saver,
            pending_save: None,
        }
    }

    /// Sets how this catalog persists its database, replacing the JSON file
    /// (or the absence of one). It runs on the owning thread for `save` and for
    /// a crash recovery, and on a short-lived thread for the save after a
    /// rescan, which gets a copy of the table.
    #[must_use]
    pub fn with_saver(
        mut self,
        saver: impl Fn(&PluginDb) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.saver = Some(Arc::new(saver));
        self
    }

    /// Creates an empty catalog without reading `config.db_path`.
    ///
    /// With the `json` feature, `save` writes to `config.db_path`.
    #[must_use]
    pub fn empty(config: CatalogConfig) -> Self {
        Self::with_db(PluginDb::new(), config)
    }

    /// Creates a catalog loaded from the JSON file at `config.db_path`.
    ///
    /// See [`tutti_plugin_db::load`] for how a missing, corrupt or
    /// other-version file is handled. Requires the `json` feature.
    #[cfg(feature = "json")]
    #[must_use]
    pub fn load(config: CatalogConfig) -> Self {
        let db = tutti_plugin_db::load(&config.db_path);
        Self::with_db(db, config)
    }

    /// Sets the audio settings applied to every plugin this catalog opens.
    #[must_use]
    pub fn with_audio_config(mut self, audio: AudioConfig) -> Self {
        self.audio = audio;
        self
    }

    /// Returns the audio settings this catalog applies when opening a plugin.
    ///
    /// Pass them to [`Plugin::open_with`] to open a plugin off-thread with the
    /// same settings: clone the value, hand it to a worker with the path, and
    /// no borrow of the catalog has to outlive the launch, which can take from
    /// half a second to several seconds.
    ///
    /// [`Plugin::open_with`]: crate::catalog::Plugin::open_with
    pub fn audio_config(&self) -> &AudioConfig {
        &self.audio
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

    /// Runs a blocking scan and returns `self`, for use in a builder chain.
    ///
    /// Discards the [`ScanResult`]; use [`Plugins::rescan`] if you need it.
    #[must_use]
    pub fn with_fresh_scan(mut self) -> Self {
        let _ = self.rescan();
        self
    }

    /// Starts a scan of the plugin directories on a background thread.
    ///
    /// Recovers a crashed previous scan first, then hands the scanner a
    /// snapshot. This catalog stays whole and usable while the scan runs; call
    /// [`poll_rescan`](Self::poll_rescan) on the owning thread (a frame tick, a
    /// UI loop) to apply what the scan found. See [`rescan`](Self::rescan) for
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
    /// # use tutti_plugin::catalog::Plugins;
    /// # use tutti_plugin_db::ScanPoll;
    /// # fn ex(mut plugins: Plugins) -> Result<(), tutti_plugin_db::ScanBusy> {
    /// let scan = plugins.spawn_rescan()?;
    /// loop {
    ///     while let Ok(progress) = scan.progress_rx.try_recv() {
    ///         println!("{}/{}", progress.current, progress.total);
    ///     }
    ///     match plugins.poll_rescan(&scan) {
    ///         ScanPoll::Running => std::thread::yield_now(),
    ///         ScanPoll::Done(result) => break println!("{} new", result.new),
    ///         ScanPoll::Died => break,
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn spawn_rescan(&mut self) -> std::result::Result<Scan, ScanBusy> {
        self.finish_pending_save();
        let saver = self.saver.as_ref();
        self.scanner
            .spawn(&mut self.db, &self.config.scan_dirs, |db| {
                write_through(saver, db)
            })
    }

    /// Applies what a rescan has found so far and reports where it stands.
    /// Never blocks.
    ///
    /// A finished scan is saved when this returns [`ScanPoll::Done`]: the table
    /// is copied here and written on a short-lived thread, so the fsync is not
    /// on the owner's thread. A failed save is logged, not returned. The write
    /// is finished before the next scan, `save`, `reload` or drop. Drop the
    /// [`Scan`] once it reports `Done` or `Died`.
    pub fn poll_rescan(&mut self, scan: &Scan) -> ScanPoll {
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
    /// in-memory catalog is updated, and saved, before this returns.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`], as for [`spawn_rescan`](Self::spawn_rescan).
    pub fn rescan(&mut self) -> std::result::Result<ScanResult, ScanBusy> {
        self.finish_pending_save();
        let saver = self.saver.as_ref();
        let result = self
            .scanner
            .scan(&mut self.db, &self.config.scan_dirs, |db| {
                write_through(saver, db)
            })?;
        if let Err(e) = write_through(self.saver.as_ref(), &self.db) {
            tracing::warn!("failed to save the plugin database after a scan: {e}");
        }
        Ok(result)
    }

    /// Writes a copy of the table on its own thread. The clone is the only cost
    /// on the owner's thread. A write still in flight is finished first, so two
    /// writes never race and the newer table is the one on disk.
    fn save_in_background(&mut self) {
        self.finish_pending_save();
        let Some(saver) = self.saver.clone() else {
            tracing::warn!("a rescan finished but this catalog has no store to save to");
            return;
        };
        let copy = Arc::new(self.db.clone());
        let for_thread = (Arc::clone(&copy), Arc::clone(&saver));
        let spawned = std::thread::Builder::new()
            .name("plugin-db-save".into())
            .spawn(move || {
                let (copy, saver) = for_thread;
                if let Err(e) = saver(&copy) {
                    tracing::warn!("failed to save the plugin database after a scan: {e}");
                }
            });
        match spawned {
            Ok(handle) => self.pending_save = Some(handle),
            Err(e) => {
                tracing::warn!(
                    "could not start a thread for the database save, saving inline: {e}"
                );
                if let Err(e) = saver(&copy) {
                    tracing::warn!("failed to save the plugin database after a scan: {e}");
                }
            }
        }
    }

    fn finish_pending_save(&mut self) {
        if let Some(handle) = self.pending_save.take()
            && handle.join().is_err()
        {
            tracing::warn!("the plugin database save panicked; the file may be out of date");
        }
    }

    /// Replaces the in-memory catalog with the contents of the JSON database
    /// file.
    ///
    /// Useful when another process may have rewritten the file. Unsaved
    /// in-memory changes are lost. A rescan in flight still applies its deltas
    /// to the reloaded catalog. Requires the `json` feature.
    #[cfg(feature = "json")]
    pub fn reload(&mut self) {
        self.finish_pending_save();
        self.db = tutti_plugin_db::load(&self.config.db_path);
    }

    /// Iterates the ids and descriptors of all non-blacklisted plugins.
    pub fn iter(&self) -> impl Iterator<Item = (PluginId, &PluginDescriptor)> {
        self.db
            .plugins()
            .map(|p| (PluginId(p.path.to_path_buf()), p.descriptor))
    }

    /// Iterates all non-blacklisted plugins with their file and format.
    ///
    /// Use this when you need `format` or `role`, for example to group plugins
    /// by format in a browser.
    pub fn entries(&self) -> impl Iterator<Item = PluginEntry<'_>> {
        self.db.plugins()
    }

    /// Returns the id of the first non-blacklisted plugin with this display
    /// name.
    pub fn find(&self, name: &str) -> Option<PluginId> {
        self.db
            .plugins()
            .find(|p| p.descriptor.name == name)
            .map(|p| PluginId(p.path.to_path_buf()))
    }

    /// Returns the descriptor of a non-blacklisted plugin by id.
    pub fn info(&self, id: &PluginId) -> Option<&PluginDescriptor> {
        self.db
            .plugins()
            .find(|p| p.path == id.0)
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

    /// Returns `true` if [`open`](Self::open) would refuse the plugin at
    /// `path`.
    ///
    /// A blacklisted plugin whose file has changed since it was blacklisted
    /// (a reinstall or update) is not refused, so this returns `false` for it.
    /// Must stay in step with `open`: both use the mtime-aware check
    /// ([`PluginDb::is_blacklisted_and_unchanged`]).
    #[must_use]
    pub fn is_blacklisted(&self, path: impl AsRef<Path>) -> bool {
        let path = key(path.as_ref());
        self.db
            .is_blacklisted_and_unchanged(&path, &FileFacts::read(&path))
    }

    /// Opens a plugin with this catalog's audio settings, refusing one that is
    /// blacklisted.
    ///
    /// [`Plugin::open`] opens any path without consulting a catalog; this adds
    /// the blacklist check. The path need not have been scanned. The check is
    /// mtime-aware, so a reinstall or update re-admits the plugin without
    /// clearing anything. Blocks while the plugin-server subprocess launches.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeError::Blacklisted`] with the recorded reason if the
    /// plugin is blacklisted and unchanged; a host can offer to load it anyway
    /// through [`Plugin::open`]. Otherwise returns any error of
    /// [`Plugin::open_with`].
    ///
    /// [`Plugin::open`]: crate::catalog::Plugin::open
    pub fn open(
        &self,
        main: Option<&MainThread>,
        path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Plugin> {
        let path = path.as_ref();
        if self.is_blacklisted(path) {
            let reason = self
                .db
                .row(&key(path))
                .and_then(|row| row.blacklist_reason())
                .unwrap_or("no reason recorded")
                .to_string();
            return Err(BridgeError::Blacklisted {
                path: path.to_path_buf(),
                reason,
            });
        }
        Plugin::open_with(main, &self.audio, path, sample_rate)
    }

    /// Blacklists a plugin by path, hiding it from `iter`, `entries` and `find`
    /// and making [`open`](Self::open) refuse it.
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

    /// Probes one plugin file and adds it to the catalog, returning its
    /// [`PluginId`].
    ///
    /// For plugins known by path rather than found by a scan, such as one
    /// shipped inside an application bundle or a file the user picked.
    /// **Blocking**: the probe spawns a subprocess and waits on a handshake,
    /// up to about seven seconds if the plugin hangs. A frame-driven host should
    /// run [`probe_record`] on a worker and pass the result to
    /// [`apply`](Self::apply) instead.
    ///
    /// # Errors
    ///
    /// Returns any error of [`probe_record`], for example when the file
    /// extension is not a plugin format or the probe fails.
    pub fn register_path(&mut self, plugin_path: impl AsRef<Path>) -> Result<PluginId> {
        let delta = probe_record(plugin_path)?;
        let id = PluginId(delta.path().to_path_buf());
        self.apply(delta);
        Ok(id)
    }

    /// Applies a delta to the catalog, such as the one [`probe_record`]
    /// returns.
    ///
    /// The non-blocking half of [`register_path`](Self::register_path): run
    /// [`probe_record`] on any thread (it needs no catalog) and pass the result
    /// here. Call `save` (feature `json`) to persist.
    pub fn apply(&mut self, delta: Delta) {
        self.db.apply(delta);
    }

    /// Writes the in-memory catalog to its store: the JSON file at
    /// `config.db_path` (atomically; feature `json`), or the host's own saver
    /// ([`with_saver`](Self::with_saver)).
    ///
    /// # Errors
    ///
    /// Returns the store's I/O error; the previous file is then intact. With no
    /// store (no `json`, no saver) returns
    /// [`Unsupported`](std::io::ErrorKind::Unsupported).
    pub fn save(&mut self) -> std::io::Result<()> {
        self.finish_pending_save();
        write_through(self.saver.as_ref(), &self.db)
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
    /// The sentinel is removed only after the blacklist is saved. A catalog with
    /// no store (no `json`, no [`with_saver`](Self::with_saver)) cannot save, so
    /// the blacklist lives in memory and the sentinel stays armed: the next
    /// start blacklists the plugin again rather than forgetting it. Give such a
    /// catalog a saver to let the sentinel go.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a rescan is in flight: its armed sentinel names a live
    /// probe, not a crash.
    pub fn recover_crash(&mut self) -> std::result::Result<Vec<PathBuf>, ScanBusy> {
        self.finish_pending_save();
        let saver = self.saver.as_ref();
        self.scanner
            .recover_crash(&mut self.db, |db| write_through(saver, db))
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
    use tutti_plugin_db::{FileRow, PluginFormat, ScanPoll};

    /// A blacklisted row whose `mtime` matches what the file system reports,
    /// i.e. the file has not changed since it was recorded.
    fn blacklisted_row(path: &Path, reason: &str) -> FileRow {
        FileRow::blacklisted(reason.to_string(), &FileFacts::read(path))
    }

    fn catalog_with(path: &Path, row: FileRow) -> Plugins {
        let mut db = PluginDb::new();
        // Rows are keyed by canonical path, as a scan keys them.
        db.apply(Delta::put(key(path), row));
        Plugins::with_db(
            db,
            CatalogConfig::new("/nonexistent/db.json", crate::catalog::NO_SCAN_DIRS),
        )
    }

    /// The guarded door refuses a plugin the scanner recorded as a crasher,
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

        match plugins.open(None, &path, 48_000.0) {
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
    ///
    /// The load itself still fails (the fixture is not a plugin), but it must
    /// fail as a *load*, never as a `Blacklisted` refusal.
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

        let err = plugins
            .open(None, &path, 48_000.0)
            .expect_err("the fixture is not a loadable plugin");
        assert!(
            !matches!(err, BridgeError::Blacklisted { .. }),
            "a changed file must not be refused as blacklisted, got {err:?}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// The query and the door agree about a plugin whose file has changed.
    ///
    /// Were the query to read the raw flag while `open` used the mtime-aware
    /// check, an updated plugin would report blacklisted and open anyway. A
    /// browser greying entries out on the query then hides a plugin it could
    /// load — permanently, because the reinstall meant to lift the blacklist is
    /// exactly what makes the two diverge.
    ///
    /// Asserting *agreement* rather than a fixed answer is what makes this
    /// survive a future change to the staleness rule: whatever `open` decides,
    /// the query has to say the same thing.
    #[test]
    fn the_query_and_the_door_agree_after_the_file_changes() {
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
                fresh.open(None, &path, 48_000.0),
                Err(BridgeError::Blacklisted { .. })
            ),
            "unchanged and blacklisted: the door must refuse"
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
        let err = plugins
            .open(None, &path, 48_000.0)
            .expect_err("the fixture is not a loadable plugin");
        assert!(
            !matches!(err, BridgeError::Blacklisted { .. }),
            "the door must admit it too, so the two answers agree; got {err:?}"
        );

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

        let err =
            Plugin::open(None, &path, 48_000.0).expect_err("the fixture is not a loadable plugin");
        assert!(
            !matches!(err, BridgeError::Blacklisted { .. }),
            "Plugin::open has no catalog and cannot refuse on one, got {err:?}"
        );

        std::fs::remove_file(&path).ok();
    }

    /// The real edge: `prune_missing` stats each row's path and removes only
    /// the absent one. Mutation: drop the `.filter(|(path, _)| FileFacts::read(..).exists)`
    /// so every row counts as present (nothing is pruned).
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
        let mut plugins = Plugins::with_db(
            db,
            CatalogConfig::new("/nonexistent/db.json", crate::catalog::NO_SCAN_DIRS),
        );

        assert_eq!(plugins.prune_missing(), vec![gone]);
        assert_eq!(plugins.iter().count(), 1);
    }

    /// A crash recovered through the catalog is on disk before the pedal is
    /// gone, and a fresh `load` sees it. The pedal sits beside the database.
    /// Mutation: have `recover_crash` pass a `save` that does nothing.
    #[cfg(feature = "json")]
    #[test]
    fn a_recovered_crash_is_saved_and_survives_a_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path().join("db.json"), crate::catalog::NO_SCAN_DIRS);
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();

        let mut plugins = Plugins::empty(config.clone());
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));

        assert!(!config.pedal_path().exists());
        let restarted = Plugins::load(config);
        assert!(restarted.is_blacklisted(&crashy));
    }

    /// A catalog with a saver of its own gets the blacklist before the pedal
    /// goes, and the pedal goes after. Mutation: remove the pedal without
    /// calling the saver (the `write_through` closure in `recover_crash`).
    #[test]
    fn a_saver_receives_the_recovered_blacklist_before_the_pedal_goes() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path().join("db.json"), crate::catalog::NO_SCAN_DIRS);
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();
        let pedal = config.pedal_path();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let key_path = key(&crashy);

        let mut plugins = Plugins::empty(config).with_saver(move |db| {
            log.lock()
                .unwrap()
                .push((db.is_blacklisted(&key_path), pedal.exists()));
            Ok(())
        });
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));

        assert_eq!(*seen.lock().unwrap(), vec![(true, true)]);
        assert!(!plugins.config.pedal_path().exists());
    }

    /// A catalog with no store cannot have saved the blacklist, so the pedal
    /// stays armed and the blacklist is in memory only. Mutation: make
    /// `write_through` return `Ok(())` for no saver.
    #[cfg(not(feature = "json"))]
    #[test]
    fn a_catalog_with_no_store_keeps_the_pedal_armed() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path().join("db.json"), crate::catalog::NO_SCAN_DIRS);
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();

        let mut plugins = Plugins::empty(config.clone());
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));
        // A rescan recovers first; it must not delete what it could not save.
        plugins.rescan().unwrap();

        assert!(plugins.db().is_blacklisted(&key(&crashy)));
        assert!(config.pedal_path().exists());
        assert_eq!(
            plugins.save().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
    }

    /// An alias of a blacklisted plugin is refused like the plugin itself: the
    /// catalog keys by canonical path. Mutation: drop `key` from `blacklist`.
    #[cfg(unix)]
    #[test]
    fn a_blacklist_set_through_an_alias_holds_for_the_real_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("Real.vst3");
        std::fs::write(&real, b"x").unwrap();
        let alias = dir.path().join("Alias.vst3");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let mut plugins = Plugins::empty(CatalogConfig::new(
            dir.path().join("db.json"),
            crate::catalog::NO_SCAN_DIRS,
        ));

        plugins.blacklist(&alias, "boom");

        assert!(plugins.is_blacklisted(&real));
        assert!(plugins.is_blacklisted(&alias));
        assert_eq!(plugins.db().len(), 1);
    }

    /// The save after a rescan is off the owner's thread: `poll_rescan` reports
    /// `Done` while the saver is still running, and the next `save` waits for
    /// it, and dropping the catalog waits for it.
    /// Mutation: call the saver inline in `save_in_background`.
    #[test]
    fn the_save_after_a_rescan_does_not_block_the_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path().join("db.json"), crate::catalog::NO_SCAN_DIRS);
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(8);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::clone(&writes);
        let mut plugins = Plugins::empty(config).with_saver(move |_| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
            done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        let began = std::time::Instant::now();
        let scan = plugins.spawn_rescan().unwrap();
        let end = loop {
            match plugins.poll_rescan(&scan) {
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

    /// `remove` and `unblacklist` through an alias reach the row of a file that
    /// is gone: the key resolves the parent and keeps the name.
    /// Mutation: make `key` return the path as given.
    #[cfg(unix)]
    #[test]
    fn remove_through_an_alias_reaches_a_deleted_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let file = real_dir.join("Gone.vst3");
        std::fs::write(&file, b"x").unwrap();
        let alias_dir = dir.path().join("alias");
        std::os::unix::fs::symlink(&real_dir, &alias_dir).unwrap();
        let mut plugins = Plugins::empty(CatalogConfig::new(
            dir.path().join("db.json"),
            crate::catalog::NO_SCAN_DIRS,
        ));
        plugins.blacklist(&file, "boom");
        plugins.blacklist(real_dir.join("Also.vst3"), "boom");
        std::fs::remove_file(&file).unwrap();

        plugins.remove(alias_dir.join("Gone.vst3"));
        assert!(plugins.unblacklist(alias_dir.join("Also.vst3")));

        assert_eq!(plugins.db().len(), 0);
    }

    /// A plugin the user unblacklists, or removes, is not put back by the next
    /// recovery from an entry a failed save kept in the sentinel.
    /// Mutation: skip `forget_in_pedal` in `unblacklist` (the first assert), or
    /// in `remove` (the second).
    #[test]
    fn unblacklist_and_remove_drop_the_retained_sentinel_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path().join("db.json"), crate::catalog::NO_SCAN_DIRS);
        let (a, b) = (dir.path().join("A.vst3"), dir.path().join("B.vst3"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let entries = format!("{}\0{}", a.display(), b.display());
        std::fs::write(config.pedal_path(), entries).unwrap();
        let mut plugins =
            Plugins::empty(config.clone()).with_saver(|_| Err(std::io::Error::other("no disk")));
        assert_eq!(plugins.recover_crash().unwrap().len(), 2);
        assert!(config.pedal_path().exists(), "kept: the save failed");

        assert!(plugins.unblacklist(&a));
        plugins.rescan().unwrap();
        assert!(!plugins.is_blacklisted(&a), "the user's unblacklist held");
        assert!(plugins.is_blacklisted(&b), "the other entry is still kept");

        plugins.remove(&b);
        plugins.rescan().unwrap();
        assert!(plugins.db().row(&key(&b)).is_none(), "the removal held");
        assert!(!config.pedal_path().exists());
    }

    /// The whole catalog works through a root that is a link, as macOS's temp
    /// dir is: a sentinel entry, a blacklist, a lookup and an unblacklist all
    /// name the file by the link path and meet at the canonical key. Linux has
    /// no such temp dir, so the link is made here.
    /// Mutation: key `Plugins::blacklist` by the raw path; or drop the
    /// `canonical_path` map in `Pedal::recover`.
    #[cfg(unix)]
    #[test]
    fn a_catalog_under_a_symlinked_root_agrees_with_itself() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("A.vst3"), b"x").unwrap();
        std::fs::write(real.join("B.vst3"), b"x").unwrap();
        let root = dir.path().join("root");
        std::os::unix::fs::symlink(&real, &root).unwrap();
        let config = CatalogConfig::new(root.join("db.json"), crate::catalog::NO_SCAN_DIRS);
        std::fs::write(config.pedal_path(), root.join("A.vst3").to_str().unwrap()).unwrap();
        let mut plugins = Plugins::empty(config).with_saver(|_| Ok(()));

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
