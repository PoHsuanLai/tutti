//! Shared-memory layout descriptor for the audio slab. Creator and
//! opener must pass equal values for the mapping to be sound.

use serde::{Deserialize, Serialize};

use super::sample::SampleFormat;
use super::BusChannels;

/// Shared-memory slab descriptor: the shape both processes must agree on before
/// either maps a byte.
///
/// The two directions occupy **disjoint** regions, each a ring of `slots`
/// blocks:
///
/// ```text
/// [ header ][ input ring: slots x input_channels x samples ][ output ring: ... ]
/// ```
///
/// # Why there is no flat `channels` total
///
/// There used to be one, alongside the bus lists, and the pair was the bug. A
/// separately-serialized total is a second source of truth about the same fact,
/// and the two could disagree: the old `is_multibus()` returned false for a
/// plugin with one bus per direction, which collapsed the output region onto the
/// input region at offset 0 — while `channels` still said 2. The host then read
/// back its own input as though it were the plugin's output, at unity gain, and
/// nothing in the system could tell. Deriving every width from the bus lists
/// makes that disagreement unrepresentable.
///
/// Both lists are therefore **mandatory and non-empty**. A loader that cannot
/// determine a plugin's buses supplies a stereo default rather than an empty
/// list; "empty" no longer has a meaning to fall back to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlabLayout {
    /// Samples per channel in one block — the largest block that can cross the
    /// boundary, not the host's configured maximum buffer size.
    pub samples_per_channel: usize,
    pub format: SampleFormat,
    /// Ring depth, per direction. Carried on the wire (rather than assumed) so
    /// the opening side can reject a peer built with a different depth instead
    /// of silently addressing the wrong slot.
    pub slots: u32,
    /// Per-bus input channel counts, in bus-index order. Never empty.
    pub inputs: BusChannels,
    /// Per-bus output channel counts, in bus-index order. Never empty.
    pub outputs: BusChannels,
}

impl SlabLayout {
    pub fn sample_size(&self) -> usize {
        match self.format {
            SampleFormat::Float32 => std::mem::size_of::<f32>(),
            SampleFormat::Float64 => std::mem::size_of::<f64>(),
        }
    }

    /// Total flat channels the input direction occupies (sum across input buses).
    pub fn input_channels(&self) -> usize {
        self.inputs.iter().map(|l| l.count() as usize).sum()
    }

    /// Total flat channels the output direction occupies (sum across output buses).
    pub fn output_channels(&self) -> usize {
        self.outputs.iter().map(|l| l.count() as usize).sum()
    }

    /// Bytes one direction's ring occupies: every slot, every channel.
    fn ring_bytes(&self, channels: usize) -> usize {
        self.slots as usize * channels * self.samples_per_channel * self.sample_size()
    }

    pub fn input_ring_bytes(&self) -> usize {
        self.ring_bytes(self.input_channels())
    }

    pub fn output_ring_bytes(&self) -> usize {
        self.ring_bytes(self.output_channels())
    }

    /// Total mapping size: header, then both rings.
    ///
    /// `header_bytes` comes from the transport layer rather than being computed
    /// here, because this crate's protocol types must stay free of the mmap
    /// details — the header's size is target-dependent (cache-line padding) and
    /// belongs with the code that knows why.
    pub fn byte_size_with_header(&self, header_bytes: usize) -> usize {
        header_bytes + self.input_ring_bytes() + self.output_ring_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ChannelLayout;
    use smallvec::SmallVec;

    fn layout(inputs: &[ChannelLayout], outputs: &[ChannelLayout]) -> SlabLayout {
        SlabLayout {
            samples_per_channel: 64,
            format: SampleFormat::Float32,
            slots: 2,
            inputs: SmallVec::from_slice(inputs),
            outputs: SmallVec::from_slice(outputs),
        }
    }

    /// The shape that regressed. Both directions get their own ring, so the
    /// total is the sum — never the `max` that the old single-bus branch used to
    /// justify sharing one region in place.
    #[test]
    fn stereo_in_stereo_out_sizes_both_directions() {
        let l = layout(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]);
        assert_eq!(l.input_channels(), 2);
        assert_eq!(l.output_channels(), 2);
        // 2 slots x 2 ch x 64 samples x 4 bytes, per direction.
        assert_eq!(l.input_ring_bytes(), 2 * 2 * 64 * 4);
        assert_eq!(l.output_ring_bytes(), 2 * 2 * 64 * 4);
        assert_eq!(l.byte_size_with_header(128), 128 + 2 * (2 * 2 * 64 * 4));
    }

    /// A sidechain widens only the input ring; the two directions are sized
    /// independently.
    #[test]
    fn a_sidechain_widens_only_the_input_ring() {
        let l = layout(
            &[ChannelLayout::Stereo, ChannelLayout::Mono],
            &[ChannelLayout::Stereo],
        );
        assert_eq!(l.input_channels(), 3);
        assert_eq!(l.output_channels(), 2);
        assert_eq!(l.input_ring_bytes(), 2 * 3 * 64 * 4);
        assert_eq!(l.output_ring_bytes(), 2 * 2 * 64 * 4);
    }

    /// The ring multiplies both directions. Worth pinning: the whole sizing
    /// argument for the pipelining change is that shrinking
    /// `samples_per_channel` to the real block size more than pays for this.
    #[test]
    fn slots_multiply_the_ring() {
        let mut l = layout(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]);
        let one_slot = {
            l.slots = 1;
            l.input_ring_bytes()
        };
        l.slots = 2;
        assert_eq!(l.input_ring_bytes(), one_slot * 2);
    }

    /// f64 negotiation doubles the bytes without changing any channel count.
    #[test]
    fn f64_doubles_the_region() {
        let mut l = layout(&[ChannelLayout::Stereo], &[ChannelLayout::Stereo]);
        let f32_bytes = l.input_ring_bytes();
        l.format = SampleFormat::Float64;
        assert_eq!(l.sample_size(), 8);
        assert_eq!(l.input_ring_bytes(), f32_bytes * 2);
        assert_eq!(l.input_channels(), 2, "format does not affect width");
    }

    /// The wire carries the reshaped struct intact, bus lists included.
    #[test]
    fn round_trips_through_bincode() {
        let l = layout(
            &[ChannelLayout::Stereo, ChannelLayout::Mono],
            &[ChannelLayout::Stereo],
        );
        let bytes = bincode::serialize(&l).unwrap();
        let back: SlabLayout = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, l);
    }

    /// Mirror of the pre-reshape `SlabLayout`, to confirm that the wire break is
    /// real and not silently absorbed. bincode is not self-describing, so a v3
    /// payload fed to this struct would read `slots` out of the bytes that used
    /// to hold `channels` — a plausible small integer, and a wrong-sized
    /// mapping. `PROTOCOL_VERSION` is what actually prevents that; this test
    /// documents *why* the bump was mandatory rather than housekeeping.
    #[derive(serde::Serialize)]
    struct LegacySlabLayout {
        channels: u16,
        samples_per_channel: usize,
        format: SampleFormat,
        inputs: BusChannels,
        outputs: BusChannels,
    }

    #[test]
    fn a_legacy_payload_does_not_deserialize_into_the_new_shape() {
        let legacy = LegacySlabLayout {
            channels: 2,
            samples_per_channel: 512,
            format: SampleFormat::Float32,
            inputs: SmallVec::new(),
            outputs: SmallVec::new(),
        };
        let bytes = bincode::serialize(&legacy).unwrap();
        // Either it fails outright, or it produces something that is not the
        // layout the sender meant. Both are caught by the version gate; what
        // must never happen is a clean round-trip that maps a wrong-sized region.
        if let Ok(decoded) = bincode::deserialize::<SlabLayout>(&bytes) {
            assert!(
                decoded.inputs.is_empty() || decoded.samples_per_channel != 512,
                "a legacy payload must not decode into an equivalent new layout"
            );
        }
    }
}
