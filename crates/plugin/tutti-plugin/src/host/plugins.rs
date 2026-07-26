//! [`Plugins`] — the primary user-facing catalog for discovered plugins.
//!
//! The caller supplies where the DB lives and which directories to scan —
//! this crate has no opinion on OS conventions or app names.
//!
//! ```no_run
//! # #[cfg(feature = "json")] {
//! use tutti_plugin::catalog::{Plugins, PluginsConfig};
//! use std::path::PathBuf;
//!
//! let plugins = PluginsConfig::new(
//!     PathBuf::from("/my/app/plugin-db.json"),
//!     vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")],
//! )
//! .build()
//! .with_fresh_scan();
//! # }
//! ```
//!
//! Custom persistence (any [`crate::catalog::PluginCatalog`] impl —
//! SQLite, in-memory, etc.) works without the `json` feature:
//!
//! ```ignore
//! use tutti_plugin::catalog::{Plugins, PluginsConfig};
//! let catalog = my_sqlite_catalog();
//! let plugins = Plugins::with_catalog(catalog, PluginsConfig::new(db, vec![]));
//! ```

use crate::error::{BridgeError, Result};
use crate::host::discovery::format_from_path;
use crate::host::discovery::record::PluginFormat;
use crate::host::discovery::{CatalogExt, PluginCatalog, PluginRecord, PluginScanner, ScanResult};
#[cfg(feature = "json")]
use crate::host::discovery::{JsonCatalog, ScanHandle};
use crate::host::handles::control_handle::PluginHandle;
use crate::host::node::PluginClient;
use crate::protocol::PluginDescriptor;
use crate::util::config::PluginsConfig;
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
/// [`PluginCatalog`] impl — defaults to a JSON file on disk when the
/// `json` feature is enabled (on by default).
pub struct Plugins {
    catalog: Box<dyn PluginCatalog>,
    config: PluginsConfig,
}

impl Plugins {
    /// Catalog backed by an arbitrary [`PluginCatalog`] impl. Use this
    /// to plug in SQLite, in-memory, or any other persistence.
    pub fn with_catalog(catalog: Box<dyn PluginCatalog>, config: PluginsConfig) -> Self {
        Self { catalog, config }
    }

    /// JSON-backed catalog with a custom config. Loads from
    /// `config.db_path`.
    #[cfg(feature = "json")]
    pub fn with_config(config: PluginsConfig) -> Self {
        let catalog = JsonCatalog::load(config.db_path.clone());
        Self {
            catalog: Box::new(catalog),
            config,
        }
    }

    /// Empty JSON-backed catalog (no DB load). For tests or manual management.
    #[cfg(feature = "json")]
    pub fn empty(config: PluginsConfig) -> Self {
        let catalog = JsonCatalog::empty(config.db_path.clone());
        Self {
            catalog: Box::new(catalog),
            config,
        }
    }

    /// Run a synchronous rescan and return `self`. Discards the
    /// [`ScanResult`]; use [`Plugins::rescan_sync`] if you need the tally.
    pub fn with_fresh_scan(mut self) -> Self {
        let _ = self.rescan_sync();
        self
    }

    /// Scan plugin directories asynchronously. Returns a handle with
    /// progress + result channels. The scanner persists its own catalog
    /// snapshot to disk on completion; call [`Plugins::reload`] afterward
    /// to refresh the in-memory view.
    ///
    /// JSON-only — custom catalogs can't be cheaply cloned across
    /// threads, so use [`Plugins::rescan_sync`] with them instead.
    #[cfg(feature = "json")]
    pub fn rescan(&self) -> ScanHandle {
        let catalog = JsonCatalog::load(self.config.db_path.clone());
        let scanner = PluginScanner::new(Box::new(catalog), self.config.pedal_path());
        scanner.scan_async(self.config.scan_dirs.clone())
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

    /// Reload the in-memory catalog from disk. Call after an async
    /// [`Plugins::rescan`] completes. JSON-only.
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

    /// Load a plugin by id. Returns a graph-ready `Box<dyn AudioUnit>`
    /// and a main-thread [`PluginHandle`]; both must be kept alive while
    /// the plugin runs.
    ///
    /// Format dispatch:
    /// - WASM: not loaded here — returns a `LoadFailed` error directing the
    ///   caller to `dawai_wasm_plugin::load` (the in-process wasmtime path
    ///   lives in the app's `dawai-wasm-plugin` crate). Hosts route by
    ///   format before calling this.
    /// - VST2 (with the `vst2` feature): runs entirely in the host process
    ///   (single AEffect for audio + editor).
    /// - Everything else: subprocess + IPC bridge (audio out-of-process,
    ///   editor lazily loaded in-host).
    pub fn load(
        &self,
        id: &PluginId,
        sample_rate: f64,
    ) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
        // `dawai:audio-plugin` is dawai's own format, hosted in-process by
        // the app's `dawai-wasm-plugin` crate. That crate lives in the app
        // workspace and depends on this one, so the dependency only ever
        // points dawai -> tutti; the wasmtime stack stays out of the engine.
        // Hosts route by format: WASM records go to
        // `dawai_wasm_plugin::load`, everything else here.
        if matches!(format_from_path(&id.0), Some(PluginFormat::Wasm)) {
            return Err(BridgeError::LoadFailed {
                path: id.0.clone(),
                stage: crate::error::LoadStage::Opening,
                reason: "WASM plugins are loaded in-process via the \
                         dawai-wasm-plugin crate (dawai_wasm_plugin::load), \
                         not tutti_plugin::Plugins::load"
                    .to_string(),
            });
        }
        #[cfg(feature = "vst2")]
        if matches!(format_from_path(&id.0), Some(PluginFormat::Vst2)) {
            return crate::format::vst2_in_process::load(&id.0, sample_rate);
        }
        let _ = format_from_path; // keep import live without the vst2 feature
        let _ = PluginFormat::Vst2;

        let client = PluginClient::new(self.config.to_bridge_config(), id.0.clone(), sample_rate)?;
        let handle = PluginHandle::from_client(&client);
        Ok((Box::new(client), handle))
    }

    /// Subprocess-formats variant of [`Plugins::load`] that returns the
    /// raw [`PluginClient`] instead of `Box<dyn AudioUnit>`. Rejects
    /// WASM (no `PluginClient` for the in-process path) — use
    /// [`Self::load`] for those.
    pub fn load_client(
        &self,
        id: &PluginId,
        sample_rate: f64,
    ) -> Result<(PluginClient, PluginHandle)> {
        if matches!(format_from_path(&id.0), Some(PluginFormat::Wasm)) {
            return Err(BridgeError::LoadFailed {
                path: id.0.clone(),
                stage: crate::error::LoadStage::Opening,
                reason: "WASM plugins are in-process only; use Plugins::load()".to_string(),
            });
        }
        let client = PluginClient::new(self.config.to_bridge_config(), id.0.clone(), sample_rate)?;
        let handle = PluginHandle::from_client(&client);
        Ok((client, handle))
    }

    /// Alias for [`Self::load_client`] — kept for API compatibility.
    pub fn load_blocking(
        &self,
        id: &PluginId,
        sample_rate: f64,
    ) -> Result<(PluginClient, PluginHandle)> {
        self.load_client(id, sample_rate)
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

    /// Register a WASM audio plugin bundled with an extension's
    /// manifest. Probes metadata via `tutti-plugin-server` and inserts
    /// a [`PluginRecord`] tagged with `ext_id` so deactivation can
    /// drop just the plugins owned by the extension.
    ///
    /// `manifest_index` is the position within the extension's
    /// `audio_plugins` list (0 = first bundled plugin). Lets editor
    /// extensions address bundled DSP plugins positionally.
    ///
    /// Standalone scanner-discovered plugins (`extension_id == None`)
    /// are unaffected by extension lifecycle.
    pub fn register_bundled_plugin(
        &mut self,
        ext_id: &str,
        manifest_index: u32,
        plugin_path: &Path,
    ) -> Result<PluginId> {
        let format = format_from_path(plugin_path).ok_or_else(|| BridgeError::LoadFailed {
            path: plugin_path.to_path_buf(),
            stage: crate::error::LoadStage::Opening,
            reason: format!(
                "extension {ext_id}: bundled plugin {} has no recognized extension",
                plugin_path.display()
            ),
        })?;

        let descriptor = crate::host::subprocess::probe_metadata(plugin_path)?;
        let modification_time =
            crate::host::discovery::file_modification_time(plugin_path).unwrap_or(0);

        self.catalog.upsert(PluginRecord {
            path: plugin_path.to_path_buf(),
            format,
            descriptor,
            modification_time,
            blacklist: crate::host::discovery::record::Blacklist::Ok,
            extension_id: Some(ext_id.to_string()),
            manifest_index: Some(manifest_index),
        });
        Ok(PluginId(plugin_path.to_path_buf()))
    }

    /// Drop every catalog entry tagged with `ext_id`. Called at
    /// extension deactivation. Returns the paths removed (running
    /// plugin instances are not torn down here — the audio graph
    /// continues to use them until the document removes them).
    pub fn unregister_bundled_plugins(&mut self, ext_id: &str) -> Vec<PathBuf> {
        self.catalog.remove_for_extension(ext_id)
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
