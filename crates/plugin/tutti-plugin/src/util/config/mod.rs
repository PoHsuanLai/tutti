//! Plugin host configuration, in three layers — each one a different
//! question, so each one its own struct.
//!
//! - [`CatalogConfig`] (in [`catalog`]) — **where plugins live**: the database
//!   path and the scan directories. Read by discovery; never by audio.
//! - [`AudioConfig`] (in [`catalog`]) — **how a loaded plugin runs**: sample
//!   format, block size, IPC timeout. Read per load.
//! - [`BridgeConfig`] (in [`bridge`]) — the **low-level, per-subprocess**
//!   transport tuning: socket path, shared-memory prefix, buffer size,
//!   timeout, sample format. One drives a single plugin-server connection;
//!   `AudioConfig::to_bridge_config` derives it.
//!
//! Nothing here constructs a [`Plugins`](crate::catalog::Plugins). Config is a
//! leaf: it describes, the host layer builds. The previous
//! `PluginsConfig::build()` inverted that, so `util::config` reached back into
//! `host::plugins` for a type it otherwise knew nothing about.

pub mod bridge;
pub mod catalog;

pub use bridge::BridgeConfig;
pub use catalog::{AudioConfig, CatalogConfig, NO_SCAN_DIRS};
