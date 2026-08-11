//! [`Plugins`] — the primary user-facing catalog for discovered plugins.
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
//! // Four methods is the whole contract; `flush` defaults to a no-op, which is
//! // right for a store that is not durable.
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

/// Opaque identifier for a plugin in a [`Plugins`] catalog.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PluginId(PathBuf);

impl PluginId {
    /// Construct from a plugin file path. The path must match a record
    /// in the catalog at load time; otherwise `Plugins::open` returns
    /// `BridgeError::PluginNotFound`.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Underlying path (for save/round-trip).
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl From<PathBuf> for PluginId {
    fn from(path: PathBuf) -> Self {
        Self(path)
    }
}

/// Catalog of discoverable + loadable plugins. Backed by any
/// [`PluginCatalog`] impl; `Plugins::with_json_catalog` supplies a
/// file-backed one when the opt-in `json` feature is enabled.
pub struct Plugins {
    catalog: Box<dyn PluginCatalog>,
    config: CatalogConfig,
    audio: AudioConfig,
}

impl Plugins {
    /// Catalog backed by an arbitrary [`PluginCatalog`] impl. Use this
    /// to plug in SQLite, in-memory, or any other persistence.
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

    /// JSON-backed catalog, loaded from `config.db_path`. Requires the
    /// `json` feature.
    ///
    /// This is the "just give me a working catalog" path. Construction lives
    /// here rather than on the config struct: config describes, the host layer
    /// builds — the reverse made `util::config` depend on `host::plugins`.
    #[cfg(feature = "json")]
    pub fn with_json_catalog(config: CatalogConfig) -> Self {
        let catalog = JsonCatalog::load(config.db_path.clone());
        Self::with_catalog(Box::new(catalog), config)
    }

    /// Empty JSON-backed catalog (no DB load). For tests or manual management.
    #[cfg(feature = "json")]
    pub fn empty(config: CatalogConfig) -> Self {
        let catalog = JsonCatalog::empty(config.db_path.clone());
        Self::with_catalog(Box::new(catalog), config)
    }

    /// Override the audio settings applied to every plugin this catalog
    /// loads. Chainable.
    pub fn with_audio_config(mut self, audio: AudioConfig) -> Self {
        self.audio = audio;
        self
    }

    /// The audio settings this catalog hands to a load.
    ///
    /// The counterpart to [`with_audio_config`](Self::with_audio_config).
    /// Opening does not go through the catalog — [`Plugin::open_with`] takes
    /// these settings and a path — so this is how a host that configured them
    /// here passes them along rather than falling back to
    /// [`AudioConfig::default`].
    ///
    /// Being a plain value read off `&self` is also what makes an **off-thread**
    /// load work: clone it, hand it to a worker with the path, and no borrow of
    /// the catalog has to outlive the frame while a subprocess takes half a
    /// second to fifteen to launch.
    ///
    /// [`Plugin::open_with`]: crate::catalog::Plugin::open_with
    pub fn audio_config(&self) -> &AudioConfig {
        &self.audio
    }

    /// Run a blocking rescan and return `self`. Discards the
    /// [`ScanResult`]; use [`Plugins::rescan`] if you need the tally.
    pub fn with_fresh_scan(mut self) -> Self {
        let _ = self.rescan();
        self
    }

    /// Scan plugin directories on a background thread, consuming `self`.
    /// Returns the scan handle (progress + result channels) alongside a
    /// [`ScanTicket`] that yields the catalog back once the scan completes.
    ///
    /// Named for [`std::process::Command::spawn`]: a thread is created and
    /// something must be joined. This is not an `async fn` and returns no
    /// future — see [`rescan`](Self::rescan) for the blocking form.
    ///
    /// Works with any [`PluginCatalog`] impl: the live catalog is *moved* onto
    /// the scanner thread rather than reloaded from disk, so this assumes no
    /// JSON and discards no in-memory record that was never flushed.
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

    /// Scan on the calling thread (blocking). Returns the scan summary; the
    /// in-memory catalog is refreshed before returning.
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

    /// Rebuild the in-memory catalog by re-reading the JSON database file.
    ///
    /// Only meaningful for a JSON-backed catalog whose file another process
    /// may have rewritten — after [`Plugins::spawn_rescan`] the catalog comes back
    /// through [`ScanTicket::join`] instead, with no reload needed.
    #[cfg(feature = "json")]
    pub fn reload(&mut self) {
        self.catalog = Box::new(JsonCatalog::load(self.config.db_path.clone()));
    }

    /// Iterate all non-blacklisted plugins.
    pub fn iter(&self) -> impl Iterator<Item = (PluginId, &PluginDescriptor)> {
        self.catalog
            .plugins()
            .map(|r| (PluginId(r.path.clone()), &r.descriptor))
    }

    /// Iterate full non-blacklisted [`PluginRecord`]s. Use this when the
    /// caller needs `format` or `extension_id` (e.g. a UI grouping
    /// records by format or by owning extension).
    pub fn records(&self) -> impl Iterator<Item = &PluginRecord> {
        self.catalog.plugins()
    }

    /// Find a plugin by display name.
    pub fn find(&self, name: &str) -> Option<PluginId> {
        self.catalog
            .plugins()
            .find(|r| r.descriptor.name == name)
            .map(|r| PluginId(r.path.clone()))
    }

    /// Look up a plugin's catalog descriptor by id.
    pub fn info(&self, id: &PluginId) -> Option<&PluginDescriptor> {
        self.catalog
            .plugins()
            .find(|r| r.path == id.0)
            .map(|r| &r.descriptor)
    }

    /// Iterate the blacklisted records — the plugins that were hidden and
    /// why. `iter`/`records` deliberately exclude them, so without this a
    /// blacklisted plugin is simply absent with no way for a UI to say so.
    pub fn blacklisted(&self) -> impl Iterator<Item = &PluginRecord> {
        self.catalog.blacklisted()
    }

    /// `true` if [`open`](Self::open) would refuse the plugin at `path`.
    ///
    /// **Answers the same question `open` asks, and must keep doing so.** Both
    /// go through the mtime-aware [`CatalogExt::is_blacklisted_and_unchanged`];
    /// reading the raw [`CatalogExt::is_blacklisted`] here would report `true`
    /// for a plugin whose file had changed since it was blacklisted, while
    /// `open` admitted it. A host greying out a browser entry on this answer
    /// then hides a plugin it could have loaded — and hides it *permanently*,
    /// because the reinstall that was supposed to lift the blacklist is exactly
    /// what makes the two disagree.
    ///
    /// A raw flag check is still available as
    /// [`CatalogExt::is_blacklisted`] for a host that genuinely wants "was this
    /// ever blacklisted" — but that is a different question, and it is not the
    /// one a UI asking "can I load this" wants.
    ///
    /// [`CatalogExt::is_blacklisted`]: crate::catalog::CatalogExt::is_blacklisted
    /// [`CatalogExt::is_blacklisted_and_unchanged`]: crate::catalog::CatalogExt::is_blacklisted_and_unchanged
    pub fn is_blacklisted(&self, path: &Path) -> bool {
        self.catalog.is_blacklisted_and_unchanged(path)
    }

    /// Open a plugin, refusing one this catalog recorded as having brought a
    /// scan down.
    ///
    /// The guarded door. [`Plugin::open`] is the plain one — it takes a path
    /// and nothing else, so it has no crash history to consult. Which of the
    /// two a host wants is a decision, so both exist and the difference is the
    /// catalog.
    ///
    /// Takes a `&Path`, not a [`PluginId`]: requiring an id would mean "scan
    /// before you can open", which is exactly the coupling
    /// [`Plugin::open`] exists to remove. Both doors take the same argument and
    /// differ only in the guard.
    ///
    /// The check is mtime-aware ([`CatalogExt::is_blacklisted_and_unchanged`]),
    /// so a reinstall or vendor update re-admits the plugin without the host
    /// clearing anything. On refusal the error carries the recorded reason, so
    /// a host can name the plugin and offer to load it anyway — that offer
    /// routes to [`Plugin::open`].
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

    /// Blacklist a plugin by path, hiding it from `iter`/`records`/`find`.
    pub fn blacklist(&mut self, path: &Path, reason: impl Into<String>) {
        self.catalog.blacklist(path, reason.into());
    }

    /// Clear one blacklist entry so the plugin is re-probed on the next scan.
    /// Returns `true` if a blacklisted record was found and cleared.
    ///
    /// The inverse of [`Self::blacklist`]. False positives are expected — the
    /// dead-man's pedal fires on force-quit, power loss, and OOM-kill just as
    /// readily as on a real plugin crash — so a blacklist with no inverse
    /// hides a working plugin forever.
    pub fn unblacklist(&mut self, path: &Path) -> bool {
        self.catalog.unblacklist(path)
    }

    /// Clear every blacklist entry. Returns the paths cleared. The bulk
    /// escape hatch for "all my plugins vanished after a crash".
    pub fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        self.catalog.clear_blacklist()
    }

    /// Drop one record entirely (blacklisted or not). Unlike
    /// [`Self::unblacklist`] this forgets the plugin was ever seen, so the
    /// next scan treats it as brand new.
    pub fn remove(&mut self, path: &Path) {
        self.catalog.remove(path);
    }

    /// Probe one plugin file and add it to the catalog, without scanning a
    /// directory. Returns its [`PluginId`].
    ///
    /// For plugins the host knows about by path rather than by scan — one
    /// shipped inside an application bundle, say, or a file the user pointed
    /// at directly. The scan directories in [`CatalogConfig`] are for the
    /// standard install locations; this is the escape hatch for everything
    /// else. Errors if the extension is unrecognized or the probe fails.
    /// **Blocking** — the probe spawns a subprocess and waits on a handshake,
    /// up to about seven seconds if the plugin hangs. A frame-driven host should
    /// run [`PluginRecord::probe`] on a worker and hand the result to
    /// [`register_record`](Self::register_record) instead.
    pub fn register_path(&mut self, plugin_path: &Path) -> Result<PluginId> {
        let record = PluginRecord::probe(plugin_path)?;
        Ok(self.register_record(record))
    }

    /// Add an already-probed record to the catalog, returning its [`PluginId`].
    ///
    /// The non-blocking half of [`register_path`](Self::register_path): a caller
    /// that cannot afford the probe on its own thread runs
    /// [`PluginRecord::probe`] wherever it likes — the probe needs no catalog —
    /// and calls this with the result. `register_path` is exactly these two
    /// steps, so the two paths cannot disagree about what registering means.
    ///
    /// In-memory only, like every other catalog mutation; call
    /// [`flush`](Self::flush) to persist.
    pub fn register_record(&mut self, record: PluginRecord) -> PluginId {
        let id = PluginId(record.path.clone());
        self.catalog.upsert(record);
        id
    }

    /// Commit the in-memory catalog to its backing store.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.catalog.flush()
    }

    /// If a previous scan died mid-probe, blacklist whatever it was probing.
    ///
    /// The scan arms a sentinel file before each probe and clears it after, so a
    /// sentinel surviving into the next run means the scanner *host* went down —
    /// a plugin crash, but equally a force-quit, a power loss, or an OOM kill.
    /// False positives are therefore expected, and
    /// [`unblacklist`](Self::unblacklist) is the counterpart a host must offer.
    ///
    /// Both scan paths already call this. It is public here so a host can
    /// surface "a plugin brought your last session down" at startup **without**
    /// paying for a full rescan: the recovery reads one sentinel and one record,
    /// where a scan probes every plugin on disk in its own subprocess.
    ///
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

    /// Drop records whose plugin file no longer exists.
    ///
    /// Uninstalling a plugin leaves its record behind — a scan only ever *adds*
    /// what it finds, so nothing else removes one, and the entry stays visible
    /// in a browser indefinitely. Returns the paths that were forgotten.
    ///
    /// Not part of a scan: a directory that is temporarily unavailable (an
    /// unmounted volume, a network share) would otherwise have its whole
    /// contents forgotten on the next rescan, and re-probing all of it is far
    /// more expensive than leaving a stale row. Pruning is a decision a host
    /// makes deliberately.
    ///
    /// Returns the paths rather than a count, because "three plugins vanished"
    /// is not something a host can act on and "these three vanished" is —
    /// `CatalogExt::prune_missing` computes the list and drops it.
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

/// Claim on the catalog a spawned [`Plugins::spawn_rescan`] took ownership of.
///
/// `rescan` consumes the [`Plugins`] because the catalog moves onto the scan
/// thread; this is how you get one back. Holding a ticket does not block —
/// [`join`](Self::join) is where you wait.
pub struct ScanTicket {
    catalog_rx: crossbeam_channel::Receiver<Box<dyn PluginCatalog>>,
    config: CatalogConfig,
    /// Carried across the scan so a rescanned catalog keeps the audio settings
    /// the caller chose. Rebuilding with `AudioConfig::default()` here would
    /// silently reset a customised block size or timeout on every rescan.
    audio: AudioConfig,
}

impl ScanTicket {
    /// Block until the scan finishes, then rebuild [`Plugins`] around the
    /// returned catalog.
    ///
    /// Errors only if the scan thread died without handing the catalog back
    /// (a panic inside the scanner); the config is returned so the caller can
    /// rebuild a fresh catalog rather than losing its scan directories.
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

    /// Take the catalog back if the scan has already finished, without
    /// blocking. `Err(self)` means the scan is still running — poll again.
    ///
    /// For frame-driven hosts (a Bevy system, a UI tick) that must not stall.
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
            CatalogConfig::new(PathBuf::from("/nonexistent/db.json"), vec![]),
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
