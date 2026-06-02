//! Plugin discovery and persistence.
//!
//! [`PluginScanner`](crate::catalog::PluginScanner) walks filesystem
//! directories and probes each plugin in a sandboxed subprocess, recording
//! results in any [`PluginCatalog`](crate::catalog::PluginCatalog). The
//! default catalog is `JsonCatalog` (JSON file on disk, behind the `json`
//! feature); swap in your own impl to persist elsewhere. A dead-man's
//! pedal auto-blacklists plugins that crash during probing.

pub mod catalog;
#[cfg(feature = "json")]
pub mod database;
mod fs;
mod pedal;
pub mod record;
pub mod scanner;

pub use catalog::{CatalogExt, PluginCatalog};
#[cfg(feature = "json")]
pub use database::JsonCatalog;
pub use fs::{file_modification_time, format_from_path};
pub use record::{Blacklist, PluginFormat, PluginRecord};
pub use scanner::{PluginScanner, ScanHandle, ScanPhase, ScanProgress, ScanResult};
