//! Shared-memory layout descriptor for the audio slab. Creator and
//! opener must pass equal values for the mapping to be sound.

use serde::{Deserialize, Serialize};

use super::BusChannels;
use super::SampleFormat;

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
/// # There is no flat `channels` total, deliberately
///
/// Every width derives from the bus lists. A separately-serialized total would be
/// a second source of truth about the same fact, and the two disagreeing is
/// silent: a region-offset calculation that collapses the output ring onto the
/// input ring at offset 0, while the total still reads 2, makes the host read
/// back its own input as though it were the plugin's output, at unity gain, with
/// nothing in the system able to tell. Deriving makes that unrepresentable.
///
/// Both lists are therefore **mandatory and non-empty**: a direction with no
/// buses (an instrument's inputs) is one bus of no channels, which
/// [`new`](Self::new) makes of an empty list and which occupies no bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlabLayout {
    /// Samples per channel in one block — the largest block that can cross the
    /// boundary, not the host's configured maximum buffer size.
    pub samples_per_channel: usize,
    /// Width of one sample in the rings, which fixes their stride.
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
    /// Returns the size of one sample in bytes, per [`format`](Self::format).
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

    /// Returns the bytes the input ring occupies, across all slots and channels.
    pub fn input_ring_bytes(&self) -> usize {
        self.ring_bytes(self.input_channels())
    }

    /// Returns the bytes the output ring occupies, across all slots and channels.
    pub fn output_ring_bytes(&self) -> usize {
        self.ring_bytes(self.output_channels())
    }

    /// Total mapping size: header, then both rings, or `None` when it does not
    /// fit in `usize`.
    ///
    /// Checked because a layout crosses the wire: the server sizes its mapping
    /// from whatever the peer sent, and an overflowed size would map a small
    /// file whose offsets the layout then places past its end. Every offset
    /// into the slab is below this figure, so checking it covers them.
    ///
    /// `header_bytes` comes from the transport layer rather than being computed
    /// here, because this crate's protocol types must stay free of the mmap
    /// details — the header's size is target-dependent (cache-line padding) and
    /// belongs with the code that knows why.
    #[must_use]
    pub fn checked_byte_size_with_header(&self, header_bytes: usize) -> Option<usize> {
        let channels = |buses: &BusChannels| {
            buses
                .iter()
                .try_fold(0usize, |sum, l| sum.checked_add(l.count() as usize))
        };
        let ring = |channels: usize| {
            (self.slots as usize)
                .checked_mul(channels)?
                .checked_mul(self.samples_per_channel)?
                .checked_mul(self.sample_size())
        };
        header_bytes
            .checked_add(ring(channels(&self.inputs)?)?)?
            .checked_add(ring(channels(&self.outputs)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ChannelLayout;
    use crate::transport::shm::RING_SLOTS;

    fn layout(inputs: &[ChannelLayout], outputs: &[ChannelLayout]) -> SlabLayout {
        SlabLayout {
            samples_per_channel: 64,
            format: SampleFormat::Float32,
            // Derived, not a literal: every other construction site uses
            // `RING_SLOTS`, and a helper that hardcodes the depth would keep
            // passing if the ring ever grew.
            slots: RING_SLOTS as u32,
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        }
    }

    /// Both directions get their own ring, so the total is the sum — never the
    /// `max` that a single-bus branch would use to justify sharing one region
    /// in place.
    #[test]
    fn stereo_in_stereo_out_sizes_both_directions() {
        let l = layout(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        assert_eq!(l.input_channels(), 2);
        assert_eq!(l.output_channels(), 2);
        // 2 slots x 2 ch x 64 samples x 4 bytes, per direction.
        assert_eq!(l.input_ring_bytes(), 2 * 2 * 64 * 4);
        assert_eq!(l.output_ring_bytes(), 2 * 2 * 64 * 4);
        assert_eq!(
            l.checked_byte_size_with_header(128),
            Some(128 + 2 * (2 * 2 * 64 * 4))
        );
    }

    /// A layout whose size does not fit in `usize` has no size: the server
    /// maps from a layout the peer sent, so a wrapped figure would map a small
    /// file under offsets that run past it.
    #[test]
    fn a_size_that_overflows_is_none() {
        let mut l = layout(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        // 2 slots x 2 channels x 2^62 = 2^64: each ring wraps to exactly 0, so
        // a wrapping computation answers a small, plausible size.
        l.samples_per_channel = 1 << (usize::BITS - 2);
        assert_eq!(l.checked_byte_size_with_header(0), None);
        l.samples_per_channel = 64;
        assert_eq!(l.checked_byte_size_with_header(usize::MAX), None);
    }

    /// A sidechain widens only the input ring; the two directions are sized
    /// independently.
    #[test]
    fn a_sidechain_widens_only_the_input_ring() {
        let l = layout(
            &[ChannelLayout::STEREO, ChannelLayout::MONO],
            &[ChannelLayout::STEREO],
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
        let mut l = layout(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
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
        let mut l = layout(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let f32_bytes = l.input_ring_bytes();
        l.format = SampleFormat::Float64;
        assert_eq!(l.sample_size(), 8);
        assert_eq!(l.input_ring_bytes(), f32_bytes * 2);
        assert_eq!(l.input_channels(), 2, "format does not affect width");
    }

    /// The wire carries the reshaped struct intact, bus lists included.
    #[test]
    fn round_trips_through_postcard() {
        let l = layout(
            &[ChannelLayout::STEREO, ChannelLayout::MONO],
            &[ChannelLayout::STEREO],
        );
        let bytes = postcard::to_allocvec(&l).unwrap();
        let back: SlabLayout = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, l);
    }
}
