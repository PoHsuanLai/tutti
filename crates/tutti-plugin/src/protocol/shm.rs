//! Shared-memory layout descriptor for the audio slab. Creator and
//! opener must pass equal values for the mapping to be sound.

use serde::{Deserialize, Serialize};

use super::metadata::{BusDirection, BusLayout};
use super::sample::SampleFormat;

/// Shared-memory slab descriptor. `channels` is the **flat total** across all
/// buses; `buses` (when non-empty) describes how that flat channel range is
/// partitioned into per-bus segments so both sides agree which flat channels
/// belong to which bus.
///
/// `buses` is `#[serde(default)]`: an older peer that never sent a bus list
/// deserializes to an empty vector == today's single-bus behaviour (all
/// `channels` belong to one main bus). `byte_size` depends only on the flat
/// `channels`, so the mmap size is identical with or without a bus list.
///
/// No longer `Copy` (the `buses` vector); it is cheap to `Clone` and passed by
/// reference on the hot read path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlabLayout {
    pub channels: usize,
    pub samples_per_channel: usize,
    pub format: SampleFormat,
    /// Per-bus flat-channel partition, ordered input buses then output buses
    /// (each in bus-index order). Empty == single flat bus (legacy).
    #[serde(default)]
    pub buses: Vec<BusLayout>,
}

impl SlabLayout {
    pub fn byte_size(&self) -> usize {
        let sample_size = match self.format {
            SampleFormat::Float32 => std::mem::size_of::<f32>(),
            SampleFormat::Float64 => std::mem::size_of::<f64>(),
        };
        self.channels * self.samples_per_channel * sample_size
    }

    /// True when a per-bus partition was negotiated. When false the slab is a
    /// single flat channel set shared in-place by both directions (legacy).
    pub fn is_multibus(&self) -> bool {
        !self.buses.is_empty()
    }

    fn direction_channels(&self, dir: BusDirection) -> usize {
        self.buses
            .iter()
            .filter(|b| b.direction == dir)
            .map(|b| b.channels)
            .sum()
    }

    /// Total flat channels the input direction occupies (sum across input
    /// buses). Falls back to the whole flat `channels` when single-bus legacy.
    pub fn input_channels(&self) -> usize {
        if self.is_multibus() {
            self.direction_channels(BusDirection::Input)
        } else {
            self.channels
        }
    }

    /// Total flat channels the output direction occupies (sum across output
    /// buses). Falls back to the whole flat `channels` when single-bus legacy.
    pub fn output_channels(&self) -> usize {
        if self.is_multibus() {
            self.direction_channels(BusDirection::Output)
        } else {
            self.channels
        }
    }

    /// Flat-channel base offset for the OUTPUT direction. Multi-bus slabs place
    /// the input direction at `[0, input_channels())` and the output direction
    /// at `[output_base(), output_base() + output_channels())` so the two
    /// directions never overlap (sidechain inputs would otherwise be clobbered
    /// by the in-place output write). Legacy single-bus stays in-place at 0.
    pub fn output_base(&self) -> usize {
        if self.is_multibus() {
            self.input_channels()
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::BusLayout;

    /// Empty bus list (single-bus legacy): bincode round-trips and `byte_size`
    /// is computed from the flat `channels` alone.
    #[test]
    fn empty_buses_round_trips() {
        let layout = SlabLayout {
            channels: 2,
            samples_per_channel: 512,
            format: SampleFormat::Float32,
            buses: Vec::new(),
        };
        let bytes = bincode::serialize(&layout).unwrap();
        let back: SlabLayout = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, layout);
        assert!(back.buses.is_empty());
        assert_eq!(back.byte_size(), 2 * 512 * 4);
    }

    /// A populated bus partition round-trips; `byte_size` still depends only on
    /// the flat `channels` (the bus list never changes the mmap size).
    #[test]
    fn multi_bus_round_trips_without_changing_byte_size() {
        let layout = SlabLayout {
            channels: 3,
            samples_per_channel: 256,
            format: SampleFormat::Float32,
            buses: vec![BusLayout::input(2), BusLayout::input(1)],
        };
        let bytes = bincode::serialize(&layout).unwrap();
        let back: SlabLayout = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, layout);
        assert_eq!(back.buses.len(), 2);
        // Flat-total byte size, unaffected by the partition.
        assert_eq!(back.byte_size(), 3 * 256 * 4);
    }

    /// Mirror of the pre-`buses` `SlabLayout` to confirm the same
    /// non-self-describing-bincode constraint as `PluginInfo`: an old payload
    /// cannot deserialize into the new struct.
    #[derive(serde::Serialize)]
    struct LegacySlabLayout {
        channels: usize,
        samples_per_channel: usize,
        format: SampleFormat,
    }

    #[test]
    fn bincode_legacy_slab_is_not_cross_version_compatible() {
        let legacy = LegacySlabLayout {
            channels: 2,
            samples_per_channel: 512,
            format: SampleFormat::Float32,
        };
        let bytes = bincode::serialize(&legacy).unwrap();
        assert!(bincode::deserialize::<SlabLayout>(&bytes).is_err());
    }
}
