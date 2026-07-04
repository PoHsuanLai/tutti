//! User-facing configuration for the [`crate::catalog::Plugins`] catalog.
//!
//! The caller supplies `db_path` (where the JSON catalog lives) and
//! `scan_dirs` (where to look for plugins) — this library has no opinion
//! on OS conventions or app names.

use super::bridge::BridgeConfig;
use crate::protocol::SampleFormat;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PluginsConfig {
    /// Where the plugin database JSON file lives.
    pub db_path: PathBuf,

    /// Directories to scan for plugins.
    pub scan_dirs: Vec<PathBuf>,

    /// Preferred audio sample format.
    pub format: SampleFormat,

    /// Audio block size (in samples). Same buffer is used for every
    /// process call.
    pub buffer_size: usize,

    /// IPC request timeout.
    pub timeout: Duration,
}

impl PluginsConfig {
    /// Start a config with the required paths and sensible defaults for
    /// the remaining knobs.
    pub fn new(db_path: impl Into<PathBuf>, scan_dirs: Vec<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            scan_dirs,
            format: SampleFormat::Float32,
            buffer_size: 8192,
            timeout: Duration::from_secs(5),
        }
    }

    // --- chainable setters ---

    pub fn db_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = path.into();
        self
    }

    pub fn scan_dirs<I, P>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.scan_dirs = dirs.into_iter().map(Into::into).collect();
        self
    }

    pub fn format(mut self, format: SampleFormat) -> Self {
        self.format = format;
        self
    }

    pub fn buffer_size(mut self, n: usize) -> Self {
        self.buffer_size = n;
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Finish the builder and produce a default JSON-backed
    /// [`crate::catalog::Plugins`] catalog. For custom catalogs, pair with
    /// [`crate::catalog::Plugins::with_catalog`] directly instead.
    #[cfg(feature = "json")]
    pub fn build(self) -> crate::host::plugins::Plugins {
        crate::host::plugins::Plugins::with_config(self)
    }

    // --- internal resolution ---

    /// Pedal sentinel lives beside the DB file.
    pub(crate) fn pedal_path(&self) -> PathBuf {
        self.db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".scanning")
    }

    pub(crate) fn to_bridge_config(&self) -> BridgeConfig {
        BridgeConfig {
            preferred_format: self.format,
            max_buffer_size: self.buffer_size,
            timeout_ms: self.timeout.as_millis() as u64,
            ..BridgeConfig::default()
        }
    }
}
