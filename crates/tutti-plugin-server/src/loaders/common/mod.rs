//! Shared helpers used by format-specific loaders.

use tutti_plugin::server::{LoadedPlugin, PluginDescriptor};

/// The two metadata snapshots every loaded plugin carries: the catalog
/// [`PluginDescriptor`] (identity + native class) and the runtime
/// [`LoadedPlugin`] (its buses, latency, f64). Each loader stores one of
/// these and forwards the `descriptor()` / `loaded()` trait methods to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Meta {
    /// Catalog identity — name, vendor, unique id, native class. Fixed at load.
    pub descriptor: PluginDescriptor,
    /// Runtime shape — the buses, latency in `Samples`, tail, and
    /// the declared feature flags. A loader refreshes fields here when the
    /// plugin reports a change, so this is not read-only after load.
    pub loaded: LoadedPlugin,
}
