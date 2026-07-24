//! Shared helpers used by format-specific loaders.

use tutti_plugin::server::{BusChannels, LoadedPlugin, PluginDescriptor};

/// The two metadata snapshots every loaded plugin carries: the catalog
/// [`PluginDescriptor`] (identity + native class) and the runtime
/// [`LoadedPlugin`] (per-bus widths, latency, f64). Each loader stores one of
/// these and forwards the `descriptor()` / `loaded()` trait methods to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Meta {
    pub descriptor: PluginDescriptor,
    pub loaded: LoadedPlugin,
}

/// Build a [`BusChannels`] holding a single main bus of `channels`.
pub(crate) fn single_bus(channels: usize) -> BusChannels {
    let mut v = BusChannels::new();
    v.push(channels);
    v
}
