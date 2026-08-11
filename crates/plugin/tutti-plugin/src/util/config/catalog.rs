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
//! Two structs rather than one: fused, a caller adjusting the audio block size
//! must restate the database path, and the combined type appears in discovery
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

/// An empty `scan_dirs` list, for [`CatalogConfig::new`].
///
/// `scan_dirs` is generic over `Into<PathBuf>`, so a bare `vec![]` or
/// `Vec::new()` has no element type to infer. This names it once instead of
/// making every "no directories yet" call site spell out a turbofish.
pub const NO_SCAN_DIRS: [PathBuf; 0] = [];

impl CatalogConfig {
    /// Start a config with the required paths.
    ///
    /// `scan_dirs` takes the same bound as the [`scan_dirs`](Self::scan_dirs)
    /// setter below, so the two agree about the one field they both write —
    /// they disagreed before, and the constructor was the stricter of the pair.
    pub fn new(
        db_path: impl Into<PathBuf>,
        scan_dirs: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            scan_dirs: scan_dirs.into_iter().map(Into::into).collect(),
        }
    }

    // --- chainable setters ---

    /// Sets where the plugin database JSON is read from and written to.
    pub fn db_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = path.into();
        self
    }

    /// Sets the directories a scan walks, replacing any already configured.
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

    /// Largest block, in **frames** per channel, that will cross the bridge.
    /// Becomes `BridgeConfig::max_buffer_size` and sizes the shared slab, so a
    /// later block may not exceed it.
    pub buffer_size: usize,

    /// How long to wait on a subprocess reply before erroring.
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

    /// Sets the sample format to request from each plugin.
    pub fn format(mut self, format: SampleFormat) -> Self {
        self.format = format;
        self
    }

    /// Sets the maximum block size, in **frames** per channel.
    pub fn buffer_size(mut self, n: usize) -> Self {
        self.buffer_size = n;
        self
    }

    /// Sets how long to wait on a subprocess reply before erroring.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The constructor accepts what the setter accepts.
    ///
    /// The two write the same field, and before this they disagreed: `new` took
    /// `Vec<PathBuf>` while `scan_dirs` took any `IntoIterator` of anything
    /// `Into<PathBuf>`. The `&str` array below is the case that would not
    /// compile.
    #[test]
    fn new_takes_the_same_dirs_the_setter_does() {
        let from_new = CatalogConfig::new("/tmp/db.json", ["/usr/lib/vst3", "/usr/lib/clap"]);
        let from_setter = CatalogConfig::new("/tmp/db.json", Vec::<PathBuf>::new())
            .scan_dirs(["/usr/lib/vst3", "/usr/lib/clap"]);

        assert_eq!(from_new.scan_dirs, from_setter.scan_dirs);
        assert_eq!(from_new.scan_dirs.len(), 2);
        assert_eq!(from_new.db_path, PathBuf::from("/tmp/db.json"));
    }

    /// An empty literal still infers. `Vec::new()` and `vec![]` are what the
    /// in-repo callers pass, and a bare `IntoIterator` bound can leave the
    /// element type unconstrained.
    #[test]
    fn an_empty_dir_list_still_infers() {
        let cfg = CatalogConfig::new("/tmp/db.json", Vec::<PathBuf>::new());
        assert!(cfg.scan_dirs.is_empty());
    }
}
