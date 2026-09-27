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
//! let plugins = Plugins::with_json_catalog(CatalogConfig::new(
//!     PathBuf::from("/my/app/plugin-db.json"),
//!     vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")],
//! ))
//! .with_fresh_scan();
//! # }
//! ```
//!
//! Custom persistence (any [`crate::catalog::PluginCatalog`] impl —
//! SQLite, in-memory, etc.) works without the `json` feature:
//!
//! ```no_run
//! use std::collections::HashMap;
//! use std::path::{Path, PathBuf};
//! use tutti_plugin::catalog::{CatalogConfig, PluginCatalog, PluginRecord, Plugins};
//!
//! // Four methods are required; `flush` defaults to a no-op, which suits a
//! // store that is not durable.
//! #[derive(Default)]
//! struct InMemory(HashMap<PathBuf, PluginRecord>);
//!
//! impl PluginCatalog for InMemory {
//!     fn get(&self, path: &Path) -> Option<&PluginRecord> { self.0.get(path) }
//!     fn upsert(&mut self, record: PluginRecord) { self.0.insert(record.path.clone(), record); }
//!     fn remove(&mut self, path: &Path) { self.0.remove(path); }
//!     fn iter(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
//!         Box::new(self.0.values())
//!     }
//! }
//!
//! let plugins = Plugins::with_catalog(
//!     Box::new(InMemory::default()),
//!     CatalogConfig::new(PathBuf::from("/unused"), vec![PathBuf::from("/usr/lib/vst3")]),
//! );
//! ```
//!
//! Audio knobs are separate and default sensibly; set them with
//! [`Plugins::with_audio_config`] when the defaults don't fit.

use crate::error::{BridgeError, Result};
#[cfg(feature = "json")]
use crate::host::discovery::JsonCatalog;
use crate::host::discovery::{
    CatalogExt, PluginCatalog, PluginRecord, PluginScanner, ScanHandle, ScanResult,
};
use crate::host::plugin::Plugin;
use crate::protocol::PluginDescriptor;
use crate::util::config::{AudioConfig, CatalogConfig};
use std::path::{Path, PathBuf};
use tutti_core::SampleRate;

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
/// Backed by any [`PluginCatalog`] implementation; [`Plugins::with_catalog`]
/// takes your own, and `Plugins::with_json_catalog` supplies a file-backed one
/// when the `json` feature is enabled. Every mutation is in-memory until
/// [`save`](Self::save) is called.
pub struct Plugins {
    catalog: Box<dyn PluginCatalog>,
    config: CatalogConfig,
    audio: AudioConfig,
}

impl Plugins {
    /// Creates a catalog backed by any [`PluginCatalog`] implementation.
    ///
    /// Use this to plug in SQLite, an in-memory store or any other persistence.
    ///
    /// Audio settings default ([`AudioConfig::default`]); override with
    /// [`Plugins::with_audio_config`].
    pub fn with_catalog(catalog: Box<dyn PluginCatalog>, config: CatalogConfig) -> Self {
        Self {
            catalog,
            config,
            audio: AudioConfig::default(),
        }
    }

    /// Creates a JSON-backed catalog loaded from `config.db_path`.
    ///
    /// See [`JsonCatalog::load`](crate::catalog::JsonCatalog::load) for how a
    /// missing or corrupt file is handled. Requires the `json` feature.
    #[cfg(feature = "json")]
    pub fn with_json_catalog(config: CatalogConfig) -> Self {
        let catalog = JsonCatalog::load(config.db_path.clone());
        Self::with_catalog(Box::new(catalog), config)
    }

    /// Creates an empty JSON-backed catalog without reading `config.db_path`.
    ///
    /// [`save`](Self::save) still writes to `config.db_path`. Requires the
    /// `json` feature.
    #[cfg(feature = "json")]
    pub fn empty(config: CatalogConfig) -> Self {
        let catalog = JsonCatalog::empty(config.db_path.clone());
        Self::with_catalog(Box::new(catalog), config)
    }

    /// Sets the audio settings applied to every plugin this catalog opens.
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

    /// Runs a blocking scan and returns `self`, for use in a builder chain.
    ///
    /// Discards the [`ScanResult`]; use [`Plugins::rescan`] if you need it.
    pub fn with_fresh_scan(mut self) -> Self {
        let _ = self.rescan();
        self
    }

    /// Scans the plugin directories on a background thread, consuming `self`.
    ///
    /// Returns the [`ScanHandle`] (progress and result channels) and a
    /// [`ScanTicket`] that gives the catalog back once the scan completes. See
    /// [`rescan`](Self::rescan) for the blocking form.
    ///
    /// Works with any [`PluginCatalog`] implementation: the live catalog is
    /// moved onto the scanner thread rather than reloaded from disk, so
    /// unsaved in-memory records are kept.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the scanner thread.
    ///
    /// # Examples
    ///
    ///
    /// ```no_run
    /// # use tutti_plugin::catalog::Plugins;
    /// # fn ex(plugins: Plugins) {
    /// let (handle, ticket) = plugins.spawn_rescan();
    /// for progress in &handle.progress_rx {
    ///     println!("{}/{}", progress.current, progress.total);
    /// }
    /// let result = handle.result_rx.recv().unwrap();
    /// println!("{} new", result.new);
    /// let plugins = ticket.join().expect("scanner thread panicked");
    /// # }
    /// ```
    pub fn spawn_rescan(self) -> (ScanHandle, ScanTicket) {
        let scanner = PluginScanner::new(self.catalog, self.config.pedal_path());
        let handle = scanner.spawn_scan(&self.config.scan_dirs);
        let ticket = ScanTicket {
            catalog_rx: handle.catalog_rx.clone(),
            config: self.config,
            audio: self.audio,
        };
        (handle, ticket)
    }

    /// Scans the plugin directories on the calling thread and returns the
    /// summary.
    ///
    /// Blocks while each new or changed plugin is probed in a subprocess. The
    /// in-memory catalog is updated before this returns.
    pub fn rescan(&mut self) -> ScanResult {
        // Move the current catalog into the scanner; leave a throwaway
        // placeholder while scanning. Works for any catalog impl because
        // the placeholder is never observed by callers.
        let placeholder: Box<dyn PluginCatalog> = Box::new(PlaceholderCatalog);
        let catalog = std::mem::replace(&mut self.catalog, placeholder);
        let mut scanner = PluginScanner::new(catalog, self.config.pedal_path());
        let result = scanner.scan(&self.config.scan_dirs);
        self.catalog = scanner.into_catalog();
        result
    }

    /// Replaces the in-memory catalog with the contents of the JSON database
    /// file.
    ///
    /// Useful when another process may have rewritten the file. Unsaved
    /// in-memory changes are lost. After [`Plugins::spawn_rescan`] no reload is
    /// needed: the catalog comes back through [`ScanTicket::join`]. Requires
    /// the `json` feature.
    #[cfg(feature = "json")]
    pub fn reload(&mut self) {
        self.catalog = Box::new(JsonCatalog::load(self.config.db_path.clone()));
    }

    /// Iterates the ids and descriptors of all non-blacklisted plugins.
    pub fn iter(&self) -> impl Iterator<Item = (PluginId, &PluginDescriptor)> {
        self.catalog
            .plugins()
            .map(|r| (PluginId(r.path.clone()), &r.descriptor))
    }

    /// Iterates the full [`PluginRecord`]s of all non-blacklisted plugins.
    ///
    /// Use this when you need `format` or `extension_id`, for example to group
    /// plugins by format in a browser.
    pub fn records(&self) -> impl Iterator<Item = &PluginRecord> {
        self.catalog.plugins()
    }

    /// Returns the id of the first non-blacklisted plugin with this display
    /// name.
    pub fn find(&self, name: &str) -> Option<PluginId> {
        self.catalog
            .plugins()
            .find(|r| r.descriptor.name == name)
            .map(|r| PluginId(r.path.clone()))
    }

    /// Returns the descriptor of a non-blacklisted plugin by id.
    pub fn info(&self, id: &PluginId) -> Option<&PluginDescriptor> {
        self.catalog
            .plugins()
            .find(|r| r.path == id.0)
            .map(|r| &r.descriptor)
    }

    /// Iterates the blacklisted records, including the recorded reason.
    ///
    /// [`iter`](Self::iter) and [`records`](Self::records) exclude these; use
    /// this to show hidden plugins and offer to [`unblacklist`](Self::unblacklist)
    /// them.
    pub fn blacklisted(&self) -> impl Iterator<Item = &PluginRecord> {
        self.catalog.blacklisted()
    }

    /// Returns `true` if [`open`](Self::open) would refuse the plugin at
    /// `path`.
    ///
    /// A blacklisted plugin whose file has changed since it was blacklisted
    /// (a reinstall or update) is not refused, so this returns `false` for it.
    /// [`CatalogExt::is_blacklisted`] answers the different question "was this
    /// ever blacklisted".
    //
    // Must stay in step with `open`: both use the mtime-aware check.
    ///
    /// [`CatalogExt::is_blacklisted`]: crate::catalog::CatalogExt::is_blacklisted
    /// [`CatalogExt::is_blacklisted_and_unchanged`]: crate::catalog::CatalogExt::is_blacklisted_and_unchanged
    pub fn is_blacklisted(&self, path: &Path) -> bool {
        self.catalog.is_blacklisted_and_unchanged(path)
    }

    /// Opens a plugin with this catalog's audio settings, refusing one that is
    /// blacklisted.
    ///
    /// [`Plugin::open`] opens any path without consulting a catalog; this adds
    /// the blacklist check. The path need not have been scanned. The check is
    /// mtime-aware ([`CatalogExt::is_blacklisted_and_unchanged`]), so a
    /// reinstall or update re-admits the plugin without clearing anything.
    /// Blocks while the plugin-server subprocess launches.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeError::Blacklisted`] with the recorded reason if the
    /// plugin is blacklisted and unchanged; a host can offer to load it anyway
    /// through [`Plugin::open`]. Otherwise returns any error of
    /// [`Plugin::open_with`].
    ///
    /// [`Plugin::open`]: crate::catalog::Plugin::open
    pub fn open(&self, path: &Path, sample_rate: impl Into<SampleRate>) -> Result<Plugin> {
        if self.catalog.is_blacklisted_and_unchanged(path) {
            let reason = self
                .catalog
                .get(path)
                .and_then(|r| r.blacklist.reason())
                .unwrap_or("no reason recorded")
                .to_string();
            return Err(BridgeError::Blacklisted {
                path: path.to_path_buf(),
                reason,
            });
        }
        Plugin::open_with(&self.audio, path, sample_rate)
    }

    /// Blacklists a plugin by path, hiding it from `iter`, `records` and `find`
    /// and making [`open`](Self::open) refuse it.
    pub fn blacklist(&mut self, path: &Path, reason: impl Into<String>) {
        self.catalog.blacklist(path, reason.into());
    }

    /// Clears one blacklist entry so the plugin is re-probed on the next scan.
    ///
    /// Returns `true` if a blacklisted record was found and cleared. Offer this
    /// to users: crash recovery also blacklists after a force-quit, power loss
    /// or out-of-memory kill, so false positives are expected.
    pub fn unblacklist(&mut self, path: &Path) -> bool {
        self.catalog.unblacklist(path)
    }

    /// Clears every blacklist entry and returns the paths cleared.
    pub fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        self.catalog.clear_blacklist()
    }

    /// Removes one record, blacklisted or not.
    ///
    /// Unlike [`Self::unblacklist`] this forgets the plugin entirely, so the
    /// next scan treats it as new.
    pub fn remove(&mut self, path: &Path) {
        self.catalog.remove(path);
    }

    /// Probes one plugin file and adds it to the catalog, returning its
    /// [`PluginId`].
    ///
    /// For plugins known by path rather than found by a scan, such as one
    /// shipped inside an application bundle or a file the user picked.
    /// **Blocking**: the probe spawns a subprocess and waits on a handshake,
    /// up to about seven seconds if the plugin hangs. A frame-driven host should
    /// run [`PluginRecord::probe`] on a worker and pass the result to
    /// [`register_record`](Self::register_record) instead.
    ///
    /// # Errors
    ///
    /// Returns any error of [`PluginRecord::probe`], for example when the file
    /// extension is not a plugin format or the probe fails.
    pub fn register_path(&mut self, plugin_path: &Path) -> Result<PluginId> {
        let record = PluginRecord::probe(plugin_path)?;
        Ok(self.register_record(record))
    }

    /// Adds an already-probed record to the catalog, returning its
    /// [`PluginId`].
    ///
    /// The non-blocking half of [`register_path`](Self::register_path): run
    /// [`PluginRecord::probe`] on any thread (it needs no catalog) and pass the
    /// result here. A record with the same path is replaced. Call
    /// [`save`](Self::save) to persist.
    pub fn register_record(&mut self, record: PluginRecord) -> PluginId {
        let id = PluginId(record.path.clone());
        self.catalog.upsert(record);
        id
    }

    /// Writes the in-memory catalog to its backing store.
    ///
    /// Calls [`PluginCatalog::flush`], which is a no-op for a store that does
    /// not persist.
    ///
    /// # Errors
    ///
    /// Returns the I/O error of the backing store's `flush`, for example when
    /// the JSON file cannot be written.
    pub fn save(&mut self) -> std::io::Result<()> {
        self.catalog.flush()
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
    /// Idempotent, and a no-op when no sentinel is present.
    pub fn recover_crash(&mut self) {
        // The scanner owns the recovery, and it takes the catalog by value, so
        // this is a move out and back rather than a borrow — the same shape as
        // `rescan`, and for the same reason: any `PluginCatalog` impl must
        // work here, not just cheaply-reloadable file-backed ones.
        let placeholder: Box<dyn PluginCatalog> = Box::new(PlaceholderCatalog);
        let catalog = std::mem::replace(&mut self.catalog, placeholder);
        let mut scanner = PluginScanner::new(catalog, self.config.pedal_path());
        scanner.recover_crash();
        self.catalog = scanner.into_catalog();
    }

    /// Removes records whose plugin file does not exist and returns their
    /// paths.
    ///
    /// A scan only adds what it finds, so an uninstalled plugin keeps its
    /// record until this is called. Scans do not prune on their own because a
    /// temporarily unavailable directory (an unmounted volume, a network share)
    /// would lose all its records and need a full re-probe.
    pub fn prune_missing(&mut self) -> Vec<PathBuf> {
        let missing: Vec<PathBuf> = self
            .catalog
            .iter()
            .filter(|r| !r.path.exists())
            .map(|r| r.path.clone())
            .collect();
        for path in &missing {
            self.catalog.remove(path);
        }
        missing
    }
}

/// Gives back the [`Plugins`] catalog that [`Plugins::spawn_rescan`] moved onto
/// its scan thread.
///
/// Holding a ticket does not block; [`join`](Self::join) waits for the scan and
/// [`try_join`](Self::try_join) polls.
pub struct ScanTicket {
    catalog_rx: crossbeam_channel::Receiver<Box<dyn PluginCatalog>>,
    config: CatalogConfig,
    /// Carried across the scan so a rescanned catalog keeps the audio settings
    /// the caller chose. Rebuilding with `AudioConfig::default()` here would
    /// silently reset a customised block size or timeout on every rescan.
    audio: AudioConfig,
}

impl ScanTicket {
    /// Blocks until the scan finishes and returns the catalog.
    ///
    /// The returned [`Plugins`] keeps the catalog config and audio settings it
    /// had before the scan.
    ///
    /// # Errors
    ///
    /// Returns the [`CatalogConfig`] if the scan thread died without handing
    /// the catalog back (a panic inside the scanner), so the caller can build
    /// a fresh catalog without losing its scan directories.
    pub fn join(self) -> std::result::Result<Plugins, CatalogConfig> {
        match self.catalog_rx.recv() {
            Ok(catalog) => Ok(Plugins {
                catalog,
                config: self.config,
                audio: self.audio,
            }),
            Err(_) => Err(self.config),
        }
    }

    /// Returns the catalog if the scan has finished, without blocking.
    ///
    /// For frame-driven hosts (a Bevy system, a UI tick) that must not stall.
    ///
    /// # Errors
    ///
    /// Returns `Err(self)` while the scan is still running (poll again), and
    /// also if the scan thread died without handing the catalog back.
    pub fn try_join(self) -> std::result::Result<Plugins, Self> {
        match self.catalog_rx.try_recv() {
            Ok(catalog) => Ok(Plugins {
                catalog,
                config: self.config,
                audio: self.audio,
            }),
            Err(_) => Err(self),
        }
    }
}

/// Zero-state stand-in used during `rescan` so the live catalog can
/// be moved into the scanner and restored afterward.
struct PlaceholderCatalog;

impl PluginCatalog for PlaceholderCatalog {
    fn get(&self, _path: &Path) -> Option<&PluginRecord> {
        None
    }
    fn upsert(&mut self, _record: PluginRecord) {}
    fn remove(&mut self, _path: &Path) {}
    fn iter(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
        Box::new(std::iter::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::discovery::record::Blacklist;

    /// Minimal in-memory catalog, so these tests do not need the `json`
    /// feature or a file on disk.
    #[derive(Default)]
    struct MemCatalog(Vec<PluginRecord>);

    impl PluginCatalog for MemCatalog {
        fn get(&self, path: &Path) -> Option<&PluginRecord> {
            self.0.iter().find(|r| r.path == path)
        }
        fn upsert(&mut self, record: PluginRecord) {
            self.0.retain(|r| r.path != record.path);
            self.0.push(record);
        }
        fn remove(&mut self, path: &Path) {
            self.0.retain(|r| r.path != path);
        }
        fn iter(&self) -> Box<dyn Iterator<Item = &PluginRecord> + '_> {
            Box::new(self.0.iter())
        }
    }

    /// A blacklisted record whose `modification_time` matches what the file
    /// system reports, i.e. the file has not changed since it was recorded.
    fn blacklisted_record(path: &Path, reason: &str) -> PluginRecord {
        PluginRecord {
            path: path.to_path_buf(),
            format: crate::host::discovery::record::PluginFormat::Vst3,
            descriptor: PluginDescriptor::default(),
            modification_time: crate::host::discovery::file_modification_time(path).unwrap_or(0),
            blacklist: Blacklist::Blacklisted {
                reason: reason.to_string(),
            },
        }
    }

    fn catalog_with(record: PluginRecord) -> Plugins {
        let mut mem = MemCatalog::default();
        mem.upsert(record);
        Plugins::with_catalog(
            Box::new(mem),
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

        let plugins = catalog_with(blacklisted_record(&path, "SIGSEGV during probe"));

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
        let mut record = blacklisted_record(&path, "crashed once");
        record.modification_time = record.modification_time.saturating_sub(1_000);
        let plugins = catalog_with(record);

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
        let fresh = catalog_with(blacklisted_record(&path, "SIGSEGV during probe"));
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
        // `file_modification_time` is second-resolution, so a rewrite inside
        // the same second leaves the times equal and the test proves nothing.
        let mut stale = blacklisted_record(&path, "SIGSEGV during probe");
        stale.modification_time = stale.modification_time.saturating_sub(1_000);
        let plugins = catalog_with(stale);

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

        let plugins = catalog_with(blacklisted_record(&path, "SIGSEGV during probe"));
        assert!(plugins.is_blacklisted(&path), "fixture should be recorded");

        let err = Plugin::open(&path, 48_000.0).expect_err("the fixture is not a loadable plugin");
        assert!(
            !matches!(err, BridgeError::Blacklisted { .. }),
            "Plugin::open has no catalog and cannot refuse on one, got {err:?}"
        );

        std::fs::remove_file(&path).ok();
    }
}
