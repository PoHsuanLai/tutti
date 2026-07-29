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

use crate::error::{BridgeError, Result};
use crate::host::discovery::format_from_path;
use crate::host::discovery::record::PluginFormat;
#[cfg(feature = "json")]
use crate::host::discovery::JsonCatalog;
use crate::host::discovery::{
    CatalogExt, PluginCatalog, PluginRecord, PluginScanner, ScanHandle, ScanResult,
};
use crate::host::handles::control_handle::PluginHandle;
use crate::host::node::PluginClient;
use crate::protocol::PluginDescriptor;
use crate::util::config::{AudioConfig, CatalogConfig};
use std::path::{Path, PathBuf};

/// Opaque identifier for a plugin in a [`Plugins`] catalog.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PluginId(PathBuf);

impl PluginId {
    /// Construct from a plugin file path. The path must match a record
    /// in the catalog at load time; otherwise `Plugins::load` returns
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

    /// The audio settings every load from this catalog uses.
    ///
    /// The counterpart to [`with_audio_config`](Self::with_audio_config), and
    /// what makes an **off-thread** load possible. [`load`](Self::load) and
    /// [`load_client`](Self::load_client) take `&self`, so a caller that must
    /// not block its thread — a frame-driven host, where a load costs a
    /// subprocess launch of half a second to fifteen — cannot call them
    /// directly: the borrow would have to outlive the frame. Reading the config
    /// here, cloning it with the [`PluginId`], and calling
    /// [`load_with`](Self::load_with) on the worker is the way across, and the
    /// [`load_client_with`] on the worker is the way across, and the settings a
    /// host chose ride along instead of being silently replaced by
    /// [`AudioConfig::default`].
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
    pub fn register_path(&mut self, plugin_path: &Path) -> Result<PluginId> {
        let record = PluginRecord::probe(plugin_path)?;
        self.catalog.upsert(record);
        Ok(PluginId(plugin_path.to_path_buf()))
    }

    /// Load a plugin by id. Returns a graph-ready `Box<dyn AudioUnit>`
    /// and a main-thread [`PluginHandle`]; both must be kept alive while
    /// the plugin runs.
    ///
    /// Format dispatch:
    /// - VST2 (with the `vst2` feature): runs entirely in the host process
    ///   (single AEffect for audio + editor).
    /// - Everything else: subprocess + IPC bridge (audio out-of-process,
    ///   editor lazily loaded in-host).
    pub fn load(
        &self,
        id: &PluginId,
        sample_rate: f64,
    ) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
        #[cfg(feature = "vst2")]
        if matches!(format_from_path(&id.0), Some(PluginFormat::Vst2)) {
            return crate::format::vst2_in_process::load(&id.0, sample_rate);
        }
        let _ = format_from_path; // keep import live without the vst2 feature
        let _ = PluginFormat::Vst2;

        let (client, handle) = load_client_with(&self.audio, id, sample_rate)?;
        Ok((Box::new(client), handle))
    }

    /// Subprocess-formats variant of [`Plugins::load`] that returns the
    /// raw [`PluginClient`] instead of `Box<dyn AudioUnit>`.
    pub fn load_client(
        &self,
        id: &PluginId,
        sample_rate: f64,
    ) -> Result<(PluginClient, PluginHandle)> {
        load_client_with(&self.audio, id, sample_rate)
    }

    /// Shortcut for [`Plugins::find`] + [`Plugins::load`].
    pub fn load_by_name(
        &self,
        name: &str,
        sample_rate: f64,
    ) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
        let id = self
            .find(name)
            .ok_or_else(|| BridgeError::PluginNotFound { name: name.into() })?;
        self.load(&id, sample_rate)
    }

    /// Commit the in-memory catalog to its backing store.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.catalog.flush()
    }
}

/// Load a plugin from an [`AudioConfig`] alone, with no catalog.
///
/// The catalog's only contribution to a load is its [`AudioConfig`] — the
/// record is looked up beforehand to get the [`PluginId`], and nothing else is
/// read. Splitting that out is what lets a load run **off the caller's thread**:
/// [`Plugins::load`] and [`Plugins::load_client`] take `&self`, so a frame-driven
/// host cannot hold the borrow across the half-second-to-fifteen-second
/// subprocess launch. It reads [`Plugins::audio_config`], clones it with the id,
/// and calls this from a worker.
///
/// `Plugins::load_client` is this function with the config supplied, so there is
/// one implementation rather than two that can drift.
///
/// Subprocess formats only — the in-process VST2 path is chosen by
/// [`Plugins::load`], which dispatches on format before reaching here.
pub fn load_client_with(
    audio: &AudioConfig,
    id: &PluginId,
    sample_rate: f64,
) -> Result<(PluginClient, PluginHandle)> {
    let client = PluginClient::new(audio.to_bridge_config(), id.0.clone(), sample_rate)?;
    let handle = PluginHandle::from_client(&client);
    Ok((client, handle))
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
