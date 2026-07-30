//! Shared helpers used by format-specific loaders.

use tutti_plugin::server::{BusChannels, ChannelLayout, LoadedPlugin, PluginDescriptor};

/// The two metadata snapshots every loaded plugin carries: the catalog
/// [`PluginDescriptor`] (identity + native class) and the runtime
/// [`LoadedPlugin`] (per-bus widths, latency, f64). Each loader stores one of
/// these and forwards the `descriptor()` / `loaded()` trait methods to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Meta {
    pub descriptor: PluginDescriptor,
    pub loaded: LoadedPlugin,
}

/// Build a [`BusChannels`] holding a single main bus of `layout`.
///
/// Takes a layout rather than a raw count so a caller that already has one
/// (every FFI-inbound conversion produces a [`ChannelLayout`]) passes it
/// straight through instead of degrading it to a `usize` for this function to
/// rebuild. Callers holding only a count still pass it — the `Into` accepts
/// `u8`/`u16`/`u32`/`usize`. Note there is deliberately no `From<i32>`, so a
/// bare integer literal must be named (`ChannelLayout::Stereo`) or suffixed.
pub(crate) fn single_bus(layout: impl Into<ChannelLayout>) -> BusChannels {
    let mut v = BusChannels::new();
    v.push(layout.into());
    v
}
