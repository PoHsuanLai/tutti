//! Shared helpers used by format-specific loaders.

use tutti_plugin::server::{BusChannels, ChannelLayout, LoadedPlugin, PluginDescriptor};
use tutti_plugin_types::SampleRate;

/// The two metadata snapshots every loaded plugin carries: the catalog
/// [`PluginDescriptor`] (identity + native class) and the runtime
/// [`LoadedPlugin`] (per-bus widths, latency, f64). Each loader stores one of
/// these and forwards the `descriptor()` / `loaded()` trait methods to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Meta {
    /// Catalog identity — name, vendor, unique id, native class. Fixed at load.
    pub descriptor: PluginDescriptor,
    /// Runtime shape — per-bus channel widths, latency in `Samples`, tail, and
    /// the declared feature flags. A loader refreshes fields here when the
    /// plugin reports a change, so this is not read-only after load.
    pub loaded: LoadedPlugin,
}

/// Build a [`BusChannels`] holding a single main bus of `layout`.
///
/// Takes a layout rather than a raw count so a caller that already has one
/// (every FFI-inbound conversion produces a [`ChannelLayout`]) passes it
/// straight through instead of degrading it to a `usize` for this function to
/// rebuild. Callers holding only a count still pass it — the `Into` accepts
/// `u8`/`u16`/`u32`/`usize`. Note there is deliberately no `From<i32>`, so a
/// bare integer literal must be named (`ChannelLayout::STEREO`) or suffixed.
// The module is reachable in a loaderless *test* build, because
// `audio_pipeline`'s tests build a `Meta`. They do not build a `BusChannels`
// through this, so it alone needs the narrower gate: a loader that builds a
// bus list itself (CLAP asks the plugin per port, and the VST2 and VST3 host
// crates build their own, so none of those uses it).
#[cfg_attr(not(all(feature = "au", target_os = "macos")), allow(dead_code))]
pub(crate) fn single_bus(layout: impl Into<ChannelLayout>) -> BusChannels {
    vec![layout.into()]
}

/// Where a plugin stands after a refused rate, for the error: the rate it runs
/// at, or that it is deactivated.
pub(crate) fn standing(active: bool, rate: SampleRate) -> String {
    if active {
        format!("running at {}", rate.0)
    } else {
        "deactivated".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The standing of a refused rate says where the plugin is**: the rate it
    /// runs at while active, "deactivated" otherwise. A refusal that leaves a
    /// plugin deactivated is reachable only after a failed rollback, which no
    /// probe forces, so the sentence is pinned here, where every loader gets it.
    ///
    /// Mutation (run): `standing` always taking the active arm -> the
    /// "deactivated" assertion fails; always the inactive arm -> the "running at"
    /// assertion fails.
    #[test]
    fn a_standing_says_running_or_deactivated() {
        assert_eq!(standing(true, SampleRate(48_000.0)), "running at 48000");
        assert_eq!(standing(false, SampleRate(48_000.0)), "deactivated");
    }
}
