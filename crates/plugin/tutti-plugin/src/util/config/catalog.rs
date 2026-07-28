//! User-facing configuration for the [`crate::catalog::Plugins`] catalog.
//!
//! Two structs, because they answer two unrelated questions and change at
//! different times:
//!
//! - [`CatalogConfig`] — *where plugins live*. The database path and the scan
//!   directories. Set once at startup, read by discovery.
//! - [`AudioConfig`] — *how a loaded plugin runs*. Sample format, block size,
//!   IPC timeout. Read per `load`, and lowered into a [`BridgeConfig`] for each
//!   plugin subprocess.
//!
//! They used to be one struct, which made the module doc's claim of a
//! two-layer split untrue: a caller adjusting the audio block size had to
//! restate the database path, and `PluginsConfig` appeared in discovery
//! signatures that never read a single audio field.
//!
//! The caller supplies `db_path` and `scan_dirs` — this library has no opinion
//! on OS conventions or app names.

use super::bridge::BridgeConfig;
use crate::protocol::SampleFormat;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the plugin database lives and which directories to scan.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// Where the plugin database file lives. Also fixes the location of the
    /// dead-man's-pedal sentinel, which sits beside it.
    pub db_path: PathBuf,

    /// Directories to scan for plugins.
    pub scan_dirs: Vec<PathBuf>,
}

impl CatalogConfig {
    /// Start a config with the required paths.
    pub fn new(db_path: impl Into<PathBuf>, scan_dirs: Vec<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            scan_dirs,
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

    // --- internal resolution ---

    /// Pedal sentinel lives beside the DB file.
    pub(crate) fn pedal_path(&self) -> PathBuf {
        self.db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".scanning")
    }
}

/// How a loaded plugin runs: the audio knobs lowered into each plugin
/// subprocess's [`BridgeConfig`].
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Preferred audio sample format.
    pub format: SampleFormat,

    /// Audio block size (in samples). Same buffer is used for every
    /// process call.
    pub buffer_size: usize,

    /// IPC request timeout.
    pub timeout: Duration,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            format: SampleFormat::Float32,
            buffer_size: 8192,
            timeout: Duration::from_secs(5),
        }
    }
}

impl AudioConfig {
    /// Defaults: `Float32`, 8192-sample blocks, 5 s IPC timeout.
    pub fn new() -> Self {
        Self::default()
    }

    // --- chainable setters ---

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

    // --- internal resolution ---

    pub(crate) fn to_bridge_config(&self) -> BridgeConfig {
        BridgeConfig {
            preferred_format: self.format,
            max_buffer_size: self.buffer_size,
            timeout_ms: self.timeout.as_millis() as u64,
            ..BridgeConfig::default()
        }
    }
}
