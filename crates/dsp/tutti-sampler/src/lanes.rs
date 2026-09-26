//! Planar scratch for the block read, and the kernels that run over it.
//!
//! A voice renders a block at a time into [`Lanes`] — one contiguous run of
//! frames per channel — and the owner then adds each lane into its output
//! with one [`accumulate`] per channel. Doc 013's "Sampler `PlaybackSlot`"
//! verdict: the per-voice read is gather-bound (every voice reads a different
//! wave at a different fractional position), so the vectors run *along time*,
//! over the lanes, not across voices.
//!
//! The kernels are plain loops over equal-length slices, written so the
//! compiler vectorises them (no bounds checks inside the loop: both slices
//! are cut to one length first). Each is the per-element arithmetic the
//! per-sample read did, in the same order, so a block read is bit-identical
//! to the frames it replaced — IEEE-754 `+` and `*` on one element give one
//! answer whether they run four at a time or one.

use crate::MAX_SAMPLER_CHANNELS;

/// Frames one lane holds: a block longer than this is read in pieces this
/// long.
///
/// 256, not the 64 `AudioUnit::process` is ever called with, so that a
/// disk voice's piece boundaries fall where its own block read's do
/// (`disk_voice::BLOCK_FRAMES`, one ring claim per piece): a longer block
/// renders exactly what a direct `DiskVoice::process` renders.
pub(crate) const LANE_FRAMES: usize = 256;

/// One channel of a block, planar.
pub(crate) type Lane = [f32; LANE_FRAMES];

/// A block of planar scratch, [`MAX_SAMPLER_CHANNELS`] lanes wide (8 KiB).
///
/// Owned by the node that renders through it, built once on the control
/// thread (boxed, so a node that moves does not move 8 KiB with it). Carries
/// nothing between blocks: every read writes each frame it hands on.
pub(crate) struct Lanes(Box<[Lane; MAX_SAMPLER_CHANNELS]>);

impl Lanes {
    /// Zeroed scratch. Allocates: control thread.
    pub(crate) fn new() -> Self {
        Self(Box::new([[0.0; LANE_FRAMES]; MAX_SAMPLER_CHANNELS]))
    }

    /// The lanes, one per channel.
    #[inline]
    pub(crate) fn lanes(&self) -> &[Lane; MAX_SAMPLER_CHANNELS] {
        &self.0
    }

    /// The lanes, one per channel, to write.
    #[inline]
    pub(crate) fn lanes_mut(&mut self) -> &mut [Lane; MAX_SAMPLER_CHANNELS] {
        &mut self.0
    }

    /// Frames `0..frames` of the first `width` lanes set to zero.
    #[inline]
    pub(crate) fn clear(&mut self, width: usize, frames: usize) {
        for lane in self.0.iter_mut().take(width) {
            lane[..frames].fill(0.0);
        }
    }

    /// Write `frame` (one sample per channel) into frame `i` of the lanes.
    #[inline]
    pub(crate) fn put(&mut self, i: usize, frame: &[f32]) {
        for (lane, &s) in self.0.iter_mut().zip(frame) {
            lane[i] = s;
        }
    }
}

impl std::fmt::Debug for Lanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lanes").finish_non_exhaustive()
    }
}

/// What a voice's block read renders through: the lanes it hands on, and the
/// lanes its stretch filter reads from. One per rendering node (a pool reads
/// every slot through its one), 16 KiB, built on the control thread.
#[derive(Debug)]
pub(crate) struct BlockScratch {
    /// The voice's output for the block, before the mix.
    pub(crate) out: Lanes,
    /// The source's frames, when a stretch filter sits between them and
    /// `out`.
    pub(crate) raw: Lanes,
}

impl BlockScratch {
    /// Zeroed scratch. Allocates: control thread.
    pub(crate) fn new() -> Self {
        Self {
            out: Lanes::new(),
            raw: Lanes::new(),
        }
    }
}

/// `dst[i] += src[i]` over the shorter of the two: the one mix a block read
/// ends in.
#[inline]
pub(crate) fn accumulate(dst: &mut [f32], src: &[f32]) {
    let n = dst.len().min(src.len());
    let (dst, src) = (&mut dst[..n], &src[..n]);
    for (d, &s) in dst.iter_mut().zip(src) {
        *d += s;
    }
}

/// `lane[i] *= gain`: one voice gain over a lane.
#[inline]
pub(crate) fn scale(lane: &mut [f32], gain: f32) {
    for s in lane.iter_mut() {
        *s *= gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The kernels are the per-element arithmetic, exactly.** Against a
    /// scalar loop over values chosen so rounding shows (thirds, added to
    /// values of like size): equal bit for bit, so a vectorised kernel can
    /// never be the reason a block read differs from the per-sample one.
    ///
    /// Mutation (run): `accumulate` computing `*d += s * 1.000_000_1` →
    /// fails. Mutation (run): `accumulate` stopping one short (`..n - 1`) →
    /// the last frame differs → fails.
    #[test]
    fn the_kernels_are_the_per_element_arithmetic() {
        let src: Vec<f32> = (0..LANE_FRAMES).map(|i| (i as f32 + 1.0) / 3.0).collect();
        let base: Vec<f32> = (0..LANE_FRAMES).map(|i| 1.0 + i as f32 * 0.1).collect();
        let mut dst = base.clone();
        accumulate(&mut dst, &src);
        for i in 0..LANE_FRAMES {
            assert_eq!(dst[i].to_bits(), (base[i] + src[i]).to_bits(), "frame {i}");
        }
        let mut lane = src.clone();
        scale(&mut lane, 0.3);
        for i in 0..LANE_FRAMES {
            assert_eq!(lane[i].to_bits(), (src[i] * 0.3).to_bits(), "frame {i}");
        }
    }
}
