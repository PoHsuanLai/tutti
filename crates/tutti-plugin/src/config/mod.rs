//! Plugin host configuration, in two layers.
//!
//! - [`PluginsConfig`] (in [`catalog`]) — the **app-facing** front door:
//!   where the plugin database lives, which directories to scan, and the
//!   audio knobs (format, buffer size, timeout). A host app builds one of
//!   these to set up the whole [`crate::catalog::Plugins`] catalog.
//! - [`BridgeConfig`] (in [`bridge`]) — the **low-level, per-subprocess**
//!   transport tuning: socket path, shared-memory prefix, buffer size,
//!   timeout, sample format. One of these drives a single plugin-server
//!   connection; `PluginsConfig::to_bridge_config` derives it.

pub mod bridge;
pub mod catalog;

pub use bridge::BridgeConfig;
pub use catalog::PluginsConfig;
