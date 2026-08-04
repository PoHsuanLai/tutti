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
//! ```ignore
//! use tutti_plugin::catalog::{CatalogConfig, Plugins};
//! let catalog = my_sqlite_catalog();
//! let plugins = Plugins::with_catalog(catalog, CatalogConfig::new(db, vec![]));
//! ```
//!
//! Audio knobs are separate and default sensibly; set them with
//! [`Plugins::with_audio_config`] when the defaults don't fit.

use crate::error::Result;
#[cfg(feature = "json")]
use crate::host::discovery::JsonCatalog;
use crate::host::discovery::{
    CatalogExt, PluginCatalog, PluginRecord, PluginScanner, ScanHandle, ScanResult,
};
use crate::protocol::PluginDescriptor;
use crate::util::config::{AudioConfig, CatalogConfig};
use std::path::{Path, PathBuf};

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
/// [`PluginCatalog`] impl; [`Plugins::with_json_catalog`] supplies a
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

    /// Run a synchronous rescan and return `self`. Discards the
    /// [`ScanResult`]; use [`Plugins::rescan_sync`] if you need the tally.
    pub fn with_fresh_scan(mut self) -> Self {
        let _ = self.rescan_sync();
        self
    }

    /// Scan plugin directories asynchronously, consuming `self`. Returns the
    /// scan handle (progress + result channels) alongside a
    /// [`ScanTicket`] that yields the catalog back once the scan completes.
    ///
    /// Works with any [`PluginCatalog`] impl: the live catalog is *moved* onto
    /// the scanner thread rather than reloaded from disk, so this no longer
    /// assumes JSON — and no longer silently discards in-memory records that
    /// were never flushed.
    ///
    /// ```no_run
    /// # use tutti_plugin::catalog::Plugins;
    /// # fn ex(plugins: Plugins) {
    /// let (handle, ticket) = plugins.rescan();
    /// for progress in &handle.progress_rx {
    ///     println!("{}/{}", progress.current, progress.total);
    /// }
    /// let result = handle.result_rx.recv().unwrap();
    /// println!("{} new", result.new);
    /// let plugins = ticket.join().expect("scanner thread panicked");
    /// # }
    /// ```
    pub fn rescan(self) -> (ScanHandle, ScanTicket) {
        let scanner = PluginScanner::new(self.catalog, self.config.pedal_path());
        let handle = scanner.scan_async(self.config.scan_dirs.clone());
        let ticket = ScanTicket {
            catalog_rx: handle.catalog_rx.clone(),
            config: self.config,
            audio: self.audio,
        };
        (handle, ticket)
    }

    /// Scan synchronously. Returns the scan summary; the in-memory
    /// catalog is refreshed before returning.
    pub fn rescan_sync(&mut self) -> ScanResult {
        // Move the current catalog into the scanner; leave a throwaway
        // placeholder while scanning. Works for any catalog impl because
        // the placeholder is never observed by callers.
        let placeholder: Box<dyn PluginCatalog> = Box::new(PlaceholderCatalog);
        let catalog = std::mem::replace(&mut self.catalog, placeholder);
        let mut scanner = PluginScanner::new(catalog, self.config.pedal_path());
        let result = scanner.scan_sync(self.config.scan_dirs.clone());
        self.catalog = scanner.into_catalog();
        result
    }

    /// Rebuild the in-memory catalog by re-reading the JSON database file.
    ///
    /// Only meaningful for a JSON-backed catalog whose file another process
    /// may have rewritten — after [`Plugins::rescan`] the catalog comes back
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

    /// `true` if the plugin at `path` is blacklisted.
    pub fn is_blacklisted(&self, path: &Path) -> bool {
        self.catalog.is_blacklisted(path)
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
        // `rescan_sync`, and for the same reason: any `PluginCatalog` impl must
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

/// Claim on the catalog an async [`Plugins::rescan`] took ownership of.
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

/// Zero-state stand-in used during `rescan_sync` so the live catalog can
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
