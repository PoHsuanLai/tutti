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
//! Nothing here constructs a `tutti-plugin`'s `Plugins`. Config is a
//! leaf: it describes, the host layer builds.

pub mod bridge;
pub mod catalog;

pub(crate) use bridge::unique_name;
pub use bridge::{BridgeConfig, unique_socket_path};
pub use catalog::{AudioConfig, CatalogConfig, NO_SCAN_DIRS};
