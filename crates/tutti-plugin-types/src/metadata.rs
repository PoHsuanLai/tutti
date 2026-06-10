//! Format-agnostic plugin load data.
//!
//! The catalog-identity half (name, vendor, version, and each format's native
//! classification) lives in `tutti-plugin` next to `PluginRecord`, because it
//! carries per-format vocabulary this crate deliberately knows nothing about.
//!
//! What stays here is [`LoadedPlugin`] — the engine-wiring data produced when a
//! plugin is actually instantiated (bus widths, latency, f64 support). It rides
//! the IPC `BridgeMessage::PluginLoaded` reply, so its `Serialize`/`Deserialize`
//! derives are gated behind the `serde` feature that `tutti-plugin` enables for
//! its bincode wire path.

use smallvec::SmallVec;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Per-bus channel counts for one process direction, in bus-index order.
///
/// An empty list means "single main bus" — the legacy single-bus convention,
/// where the one main bus's channel count is recovered from elsewhere. A
/// populated list is `[main, aux/sidechain, …]`, so summing it gives the total
/// channel width the host must supply flat (see `tutti-vst3-host`'s bus buffers).
pub type BusChannels = SmallVec<[usize; 4]>;

/// Engine-wiring data for a freshly instantiated plugin.
///
/// Produced by the plugin server at load time and returned over IPC; never
/// persisted (unlike the catalog `PluginDescriptor`, which is). Carries only
/// what the audio graph needs to wire the node — no format vocabulary, so it
/// stays in this format-agnostic crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LoadedPlugin {
    /// Per-bus input channel counts, main bus first. Empty == single main bus.
    pub inputs: BusChannels,
    /// Per-bus output channel counts, main bus first. Empty == single main bus.
    pub outputs: BusChannels,
    /// Reported initial processing latency, in samples (for PDC).
    pub latency_samples: usize,
    /// `true` if the plugin negotiated 64-bit sample processing at load.
    pub supports_f64: bool,
}

impl LoadedPlugin {
    /// Total input channel width across all buses (main + aux/sidechain). Falls
    /// back to `0` for an empty bus list — callers that need the single-bus main
    /// width supply it themselves (the count isn't carried here).
    pub fn total_inputs(&self) -> usize {
        self.inputs.iter().sum()
    }

    /// Total output channel width across all buses.
    pub fn total_outputs(&self) -> usize {
        self.outputs.iter().sum()
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    /// A populated multi-bus layout round-trips through the real bincode wire
    /// serializer and the totals sum across buses.
    #[test]
    fn multi_bus_round_trips() {
        let loaded = LoadedPlugin {
            inputs: SmallVec::from_slice(&[2, 1]), // stereo main + mono sidechain
            outputs: SmallVec::from_slice(&[2]),
            latency_samples: 128,
            supports_f64: true,
        };
        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, loaded);
        assert_eq!(back.total_inputs(), 3);
        assert_eq!(back.total_outputs(), 2);
    }

    /// An empty bus list (single-bus legacy) round-trips and sums to zero.
    #[test]
    fn empty_buses_round_trip() {
        let loaded = LoadedPlugin::default();
        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, loaded);
        assert!(back.inputs.is_empty());
        assert_eq!(back.total_inputs(), 0);
    }
}
