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
//! The database is a plain [`PluginDb`](tutti_plugin_db::PluginDb); a host that
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

use crate::error::{BridgeError, Result};
use crate::host::db_probe::{probe_plugin, probe_record};
use crate::host::plugin::Plugin;
use crate::protocol::PluginDescriptor;
use crate::util::config::{AudioConfig, CatalogConfig};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tutti_plugin_db::{
    BlacklistedFile, Delta, FileFacts, PluginDb, PluginEntry, Scan, ScanPoll, ScanResult, Scanner,
};
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

/// A catalog of scanned plugins that can list, blacklist and open them.
///
/// Wraps a [`PluginDb`]; [`Plugins::with_db`] takes your own, and
/// `Plugins::load` reads the versioned JSON file when the `json` feature is
/// enabled. Every change is in memory until `save` is called.
pub struct Plugins {
    db: PluginDb,
    scanner: Scanner,
    config: CatalogConfig,
    audio: AudioConfig,
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugins")
            .field("config", &self.config)
            .field("audio", &self.audio)
            .finish_non_exhaustive()
    }
}

/// Writes `db` to the config's file. With no `json` feature there is no file
/// format, and the host persists the database itself.
#[cfg(feature = "json")]
fn persist(db: &PluginDb, config: &CatalogConfig) -> std::io::Result<()> {
    tutti_plugin_db::save(db, &config.db_path)
}

#[cfg(not(feature = "json"))]
fn persist(_db: &PluginDb, _config: &CatalogConfig) -> std::io::Result<()> {
    Ok(())
}

impl Plugins {
    /// Creates a catalog over a database you built.
    ///
    /// Audio settings default ([`AudioConfig::default`]); override with
    /// [`Plugins::with_audio_config`].
    #[must_use]
    pub fn with_db(db: PluginDb, config: CatalogConfig) -> Self {
        let scanner = Scanner::new(config.pedal_path(), probe_plugin);
        Self {
            db,
            scanner,
            config,
            audio: AudioConfig::default(),
        }
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
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the scanner thread.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tutti_plugin::catalog::Plugins;
    /// # use tutti_plugin_db::ScanPoll;
    /// # fn ex(mut plugins: Plugins) {
    /// let scan = plugins.spawn_rescan();
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
    /// # }
    /// ```
    pub fn spawn_rescan(&mut self) -> Scan {
        let config = &self.config;
        self.scanner
            .spawn(&mut self.db, &config.scan_dirs, |db| persist(db, config))
    }

    /// Applies what a rescan has found so far and reports where it stands.
    /// Never blocks.
    ///
    /// A finished scan is saved (see `save`, feature `json`) before this returns
    /// [`ScanPoll::Done`]; a failed save is logged, not returned. Drop the
    /// [`Scan`] once it reports `Done` or `Died`.
    pub fn poll_rescan(&mut self, scan: &Scan) -> ScanPoll {
        let poll = scan.poll(&mut self.db);
        if matches!(poll, ScanPoll::Done(_)) {
            self.save_after_scan();
        }
        poll
    }

    /// Scans the plugin directories on the calling thread and returns the
    /// summary.
    ///
    /// Blocks while each new or changed plugin is probed in a subprocess. The
    /// in-memory catalog is updated, and saved, before this returns.
    pub fn rescan(&mut self) -> ScanResult {
        let config = &self.config;
        let result = self
            .scanner
            .scan(&mut self.db, &config.scan_dirs, |db| persist(db, config));
        self.save_after_scan();
        result
    }

    fn save_after_scan(&self) {
        if let Err(e) = persist(&self.db, &self.config) {
            tracing::warn!("failed to save the plugin database after a scan: {e}");
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
        let path = path.as_ref();
        self.db
            .is_blacklisted_and_unchanged(path, &FileFacts::read(path))
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
        path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Plugin> {
        let path = path.as_ref();
        if self.is_blacklisted(path) {
            let reason = self
                .db
                .row(path)
                .and_then(|row| row.blacklist_reason())
                .unwrap_or("no reason recorded")
                .to_string();
            return Err(BridgeError::Blacklisted {
                path: path.to_path_buf(),
                reason,
            });
        }
        Plugin::open_with(&self.audio, path, sample_rate)
    }

    /// Blacklists a plugin by path, hiding it from `iter`, `entries` and `find`
    /// and making [`open`](Self::open) refuse it.
    pub fn blacklist(&mut self, path: impl AsRef<Path>, reason: impl Into<String>) {
        let path = path.as_ref();
        self.db
            .blacklist(path, reason.into(), &FileFacts::read(path));
    }

    /// Clears one blacklist entry so the plugin is probed again on the next
    /// scan.
    ///
    /// Returns `true` if a blacklisted file was found and cleared. Offer this
    /// to users: crash recovery also blacklists after a force-quit, power loss
    /// or out-of-memory kill, so false positives are expected.
    pub fn unblacklist(&mut self, path: impl AsRef<Path>) -> bool {
        self.db.unblacklist(path.as_ref())
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
        self.db.apply(Delta::remove(path.as_ref()));
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

    /// Writes the in-memory catalog to the JSON file at `config.db_path`,
    /// atomically. Requires the `json` feature.
    ///
    /// # Errors
    ///
    /// Returns the I/O error of [`tutti_plugin_db::save`], for example when the
    /// file cannot be written; the previous file is then intact.
    #[cfg(feature = "json")]
    pub fn save(&self) -> std::io::Result<()> {
        persist(&self.db, &self.config)
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
    /// Idempotent, and a no-op when no sentinel is present. Returns the file
    /// that was blacklisted.
    pub fn recover_crash(&mut self) -> Option<PathBuf> {
        let config = &self.config;
        self.scanner
            .recover_crash(&mut self.db, |db| persist(db, config))
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
    use tutti_plugin_db::{FileRow, PluginFormat};

    /// A blacklisted row whose `mtime` matches what the file system reports,
    /// i.e. the file has not changed since it was recorded.
    fn blacklisted_row(path: &Path, reason: &str) -> FileRow {
        FileRow::blacklisted(reason.to_string(), &FileFacts::read(path))
    }

    fn catalog_with(path: &Path, row: FileRow) -> Plugins {
        let mut db = PluginDb::new();
        db.apply(Delta::put(path, row));
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

        match plugins.open(&path, 48_000.0) {
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
            .open(&path, 48_000.0)
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
                fresh.open(&path, 48_000.0),
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
            .open(&path, 48_000.0)
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

        let err = Plugin::open(&path, 48_000.0).expect_err("the fixture is not a loadable plugin");
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
        assert_eq!(plugins.recover_crash(), Some(crashy.clone()));

        assert!(!config.pedal_path().exists());
        let restarted = Plugins::load(config);
        assert!(restarted.is_blacklisted(&crashy));
    }
}
