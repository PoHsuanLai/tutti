//! Multi-resolution min/max/RMS waveform summaries for visualization.

use tutti_core::ChannelLayout;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WaveformBlock {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WaveformSummary {
    pub blocks: Vec<WaveformBlock>,
    pub samples_per_block: usize,
    pub total_samples: usize,
    /// Samples of the in-progress block carried across [`append_samples`] calls.
    ///
    /// Not serialized and not part of the public shape: it is only meaningful
    /// mid-stream, and a summary read back from disk is already complete.
    ///
    /// [`append_samples`]: Self::append_samples
    #[cfg_attr(feature = "serde", serde(skip))]
    pending: Vec<f32>,
}

impl WaveformSummary {
    pub fn new(samples_per_block: usize) -> Self {
        Self {
            blocks: Vec::new(),
            samples_per_block,
            total_samples: 0,
            pending: Vec::new(),
        }
    }

    pub(crate) fn with_capacity(samples_per_block: usize, num_blocks: usize) -> Self {
        Self {
            blocks: Vec::with_capacity(num_blocks),
            samples_per_block,
            total_samples: 0,
            pending: Vec::new(),
        }
    }

    /// Assemble a finished summary from blocks computed elsewhere — a decoded
    /// cache payload, or a producer that already blocks its own input.
    ///
    /// There is no partial block to carry: the caller is handing over work
    /// that is already complete.
    pub fn from_blocks(
        blocks: Vec<WaveformBlock>,
        samples_per_block: usize,
        total_samples: usize,
    ) -> Self {
        Self {
            blocks,
            samples_per_block,
            total_samples,
            pending: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn peak(&self) -> f32 {
        self.blocks
            .iter()
            .map(|b| b.min.abs().max(b.max.abs()))
            .fold(0.0f32, |a, b| a.max(b))
    }

    pub fn average_rms(&self) -> f32 {
        if self.blocks.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.blocks.iter().map(|b| b.rms).sum();
        sum / self.blocks.len() as f32
    }

    /// Append samples incrementally without loading the entire file at once.
    ///
    /// Chunks need not align to block boundaries: samples left over from one
    /// call are carried into the next, so streaming in arbitrary chunk sizes
    /// produces exactly the blocks a single [`compute_summary`] over the
    /// concatenation would. Call [`finish`](Self::finish) to emit the trailing
    /// partial block, if any.
    pub fn append_samples(&mut self, samples: &[f32]) {
        if samples.is_empty() || self.samples_per_block == 0 {
            return;
        }

        self.total_samples += samples.len();

        // Complete the block left half-built by the previous call before
        // consuming whole blocks out of `samples` directly. Indexing by a
        // global offset (as this once did) cannot work — the samples those
        // offsets refer to belong to chunks that have already been dropped.
        let mut rest = samples;
        if !self.pending.is_empty() {
            let needed = self.samples_per_block - self.pending.len();
            let take = needed.min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];

            if self.pending.len() < self.samples_per_block {
                return;
            }
            self.blocks.push(compute_block(&self.pending));
            self.pending.clear();
        }

        let mut chunks = rest.chunks_exact(self.samples_per_block);
        for block in chunks.by_ref() {
            self.blocks.push(compute_block(block));
        }
        self.pending.extend_from_slice(chunks.remainder());
    }

    /// Emit the trailing partial block and return the finished summary.
    ///
    /// [`compute_summary`] keeps a final short block, so streaming must too or
    /// the two paths disagree on any input that is not a whole number of
    /// blocks.
    pub fn finish(mut self) -> Self {
        if !self.pending.is_empty() {
            self.blocks.push(compute_block(&self.pending));
            self.pending.clear();
        }
        self
    }
}

fn compute_block(samples: &[f32]) -> WaveformBlock {
    if samples.is_empty() {
        return WaveformBlock::default();
    }

    let min = samples.iter().copied().fold(f32::INFINITY, f32::min);
    let max = samples.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();

    WaveformBlock {
        min,
        max,
        rms: (sum_sq / samples.len() as f32).sqrt(),
    }
}

/// Summarizes the first channel. For interleaved stereo, pass
/// [`ChannelLayout::Stereo`].
pub fn compute_summary(
    samples: &[f32],
    layout: ChannelLayout,
    samples_per_block: usize,
) -> WaveformSummary {
    let channels = layout.count() as usize;
    if samples.is_empty() || samples_per_block == 0 || channels == 0 {
        return WaveformSummary::new(samples_per_block);
    }

    let channel_samples = samples.len() / channels;
    let num_blocks = channel_samples.div_ceil(samples_per_block);
    let mut summary = WaveformSummary::with_capacity(samples_per_block, num_blocks);
    summary.total_samples = channel_samples;

    summary.blocks.extend((0..num_blocks).map(|block_idx| {
        let start = block_idx * samples_per_block;
        let end = (start + samples_per_block).min(channel_samples);
        let count = end - start;

        let (min, max, sum_sq) = (start..end)
            .map(|i| samples[i * channels])
            .fold((f32::MAX, f32::MIN, 0.0f32), |(min, max, sum), s| {
                (min.min(s), max.max(s), sum + s * s)
            });

        WaveformBlock {
            min: if min == f32::MAX { 0.0 } else { min },
            max: if max == f32::MIN { 0.0 } else { max },
            rms: if count > 0 {
                (sum_sq / count as f32).sqrt()
            } else {
                0.0
            },
        }
    }));

    summary
}

/// Multiple zoom levels for efficient rendering.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MultiResolutionSummary {
    /// Index 0 = finest, higher = coarser.
    pub levels: Vec<WaveformSummary>,
    pub base_samples_per_block: usize,
}

impl MultiResolutionSummary {
    /// Each level is 2x coarser than the previous.
    pub fn from_samples(
        samples: &[f32],
        layout: ChannelLayout,
        base_samples_per_block: usize,
        num_levels: usize,
    ) -> Self {
        let mut levels = Vec::with_capacity(num_levels);

        levels.push(compute_summary(samples, layout, base_samples_per_block));

        for level in 1..num_levels {
            let prev = &levels[level - 1];
            let coarse = downsample_summary(prev);
            levels.push(coarse);
        }

        Self {
            levels,
            base_samples_per_block,
        }
    }

    /// Returns the coarsest level if index is out of bounds.
    pub fn at_level(&self, level: usize) -> &WaveformSummary {
        self.levels
            .get(level)
            .unwrap_or_else(|| self.levels.last().unwrap())
    }

    /// Picks the coarsest level where `samples_per_block <= samples_per_pixel`.
    pub fn for_zoom(&self, samples_per_pixel: usize) -> &WaveformSummary {
        for (i, summary) in self.levels.iter().enumerate() {
            if summary.samples_per_block >= samples_per_pixel {
                return if i > 0 { &self.levels[i - 1] } else { summary };
            }
        }
        self.levels.last().unwrap()
    }

    pub fn num_levels(&self) -> usize {
        self.levels.len()
    }
}

fn downsample_summary(summary: &WaveformSummary) -> WaveformSummary {
    let new_samples_per_block = summary.samples_per_block * 2;
    let num_blocks = summary.blocks.len().div_ceil(2);
    let mut result = WaveformSummary::with_capacity(new_samples_per_block, num_blocks);
    result.total_samples = summary.total_samples;

    result.blocks.extend(summary.blocks.chunks(2).map(|pair| {
        let a = &pair[0];
        pair.get(1).map_or(*a, |b| WaveformBlock {
            min: a.min.min(b.min),
            max: a.max.max(b.max),
            rms: ((a.rms * a.rms + b.rms * b.rms) / 2.0).sqrt(),
        })
    }));

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_summary_mono() {
        let samples: Vec<f32> = (0..1000).map(|i| (i as f32 / 100.0).sin()).collect();

        let summary = compute_summary(&samples, ChannelLayout::Mono, 100);

        assert_eq!(summary.len(), 10);
        assert_eq!(summary.samples_per_block, 100);
        assert_eq!(summary.total_samples, 1000);

        for block in &summary.blocks {
            assert!(block.min <= block.max);
            assert!(block.rms >= 0.0);
        }
    }

    #[test]
    fn test_multi_resolution() {
        let samples: Vec<f32> = (0..1024).map(|i| (i as f32 / 50.0).sin()).collect();

        let multi = MultiResolutionSummary::from_samples(&samples, ChannelLayout::Mono, 64, 4);

        assert_eq!(multi.levels.len(), 4);
        assert_eq!(multi.levels[0].samples_per_block, 64);
        assert_eq!(multi.levels[1].samples_per_block, 128);
        assert_eq!(multi.levels[2].samples_per_block, 256);
        assert_eq!(multi.levels[3].samples_per_block, 512);

        assert!(multi.levels[1].len() <= multi.levels[0].len());
        assert!(multi.levels[2].len() <= multi.levels[1].len());
    }

    #[test]
    fn test_empty_samples() {
        let summary = compute_summary(&[], ChannelLayout::Mono, 100);
        assert!(summary.is_empty());
    }

    /// Block values, not just block shape.
    ///
    /// The existing tests check `min <= max` and `rms >= 0`, which every
    /// possible implementation satisfies. A ramp makes each block's numbers
    /// exactly predictable.
    #[test]
    fn block_values_are_exact_for_a_ramp() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let summary = compute_summary(&samples, ChannelLayout::Mono, 100);

        assert_eq!(summary.len(), 5);
        for (i, block) in summary.blocks.iter().enumerate() {
            let lo = (i * 100) as f32;
            assert_eq!(block.min, lo);
            assert_eq!(block.max, lo + 99.0);

            // RMS of 100 consecutive integers starting at `lo`.
            let expected = ((0..100)
                .map(|k| {
                    let s = lo + k as f32;
                    s * s
                })
                .sum::<f32>()
                / 100.0)
                .sqrt();
            assert!(
                (block.rms - expected).abs() < 1e-2,
                "block {i} rms {} != {expected}",
                block.rms
            );
        }

        assert_eq!(summary.peak(), 499.0);
    }

    /// A trailing partial block is kept, and summarizes only what it holds.
    #[test]
    fn trailing_partial_block_is_kept() {
        let samples: Vec<f32> = (0..250).map(|i| i as f32).collect();
        let summary = compute_summary(&samples, ChannelLayout::Mono, 100);

        assert_eq!(summary.len(), 3, "div_ceil keeps the short final block");
        assert_eq!(summary.blocks[2].min, 200.0);
        assert_eq!(summary.blocks[2].max, 249.0, "only the 50 samples present");
        assert_eq!(summary.total_samples, 250);
    }

    /// Interleaved input reads channel 0 and counts frames, not samples.
    #[test]
    fn stereo_reads_the_first_channel_only() {
        // Left ramps up, right is constant and far larger — if the right
        // channel leaked in, min/max would show it.
        let samples: Vec<f32> = (0..200)
            .flat_map(|i| [i as f32, 9999.0])
            .collect();
        let summary = compute_summary(&samples, ChannelLayout::Stereo, 100);

        assert_eq!(summary.len(), 2);
        assert_eq!(summary.total_samples, 200, "frames, not interleaved samples");
        assert_eq!(summary.blocks[0].min, 0.0);
        assert_eq!(summary.blocks[0].max, 99.0);
        assert_eq!(summary.blocks[1].max, 199.0);
    }

    /// `downsample_summary` halves the block count, unions min/max, and takes
    /// the quadratic mean of the two RMS values.
    #[test]
    fn downsampling_unions_extremes_and_rms_combines_quadratically() {
        let samples: Vec<f32> = (0..400).map(|i| i as f32).collect();
        let multi = MultiResolutionSummary::from_samples(&samples, ChannelLayout::Mono, 100, 2);

        let fine = &multi.levels[0];
        let coarse = &multi.levels[1];

        assert_eq!(fine.len(), 4);
        assert_eq!(coarse.len(), 2);
        assert_eq!(coarse.samples_per_block, 200);
        assert_eq!(
            coarse.total_samples, fine.total_samples,
            "downsampling changes resolution, not duration"
        );

        for (i, block) in coarse.blocks.iter().enumerate() {
            let (a, b) = (&fine.blocks[i * 2], &fine.blocks[i * 2 + 1]);
            assert_eq!(block.min, a.min.min(b.min));
            assert_eq!(block.max, a.max.max(b.max));
            let expected = ((a.rms * a.rms + b.rms * b.rms) / 2.0).sqrt();
            assert!((block.rms - expected).abs() < 1e-2);
        }
    }

    /// An odd block count carries the last block through unpaired.
    #[test]
    fn downsampling_an_odd_block_count_keeps_the_last_block_as_is() {
        let samples: Vec<f32> = (0..300).map(|i| i as f32).collect();
        let multi = MultiResolutionSummary::from_samples(&samples, ChannelLayout::Mono, 100, 2);

        assert_eq!(multi.levels[0].len(), 3);
        assert_eq!(multi.levels[1].len(), 2);
        // The unpaired third block is copied verbatim, not halved or dropped.
        assert_eq!(multi.levels[1].blocks[1], multi.levels[0].blocks[2]);
    }

    /// `at_level` saturates at the coarsest level rather than panicking.
    #[test]
    fn at_level_saturates_past_the_end() {
        let samples: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let multi = MultiResolutionSummary::from_samples(&samples, ChannelLayout::Mono, 64, 3);

        assert_eq!(multi.num_levels(), 3);
        assert_eq!(multi.at_level(0).samples_per_block, 64);
        assert_eq!(multi.at_level(2).samples_per_block, 256);
        assert_eq!(
            multi.at_level(99).samples_per_block,
            256,
            "out of range returns the coarsest"
        );
    }

    /// `for_zoom` picks one level finer than the first that is coarse enough.
    ///
    /// Characterizing the off-by-one deliberately: the loop returns
    /// `levels[i - 1]` on match, so asking for a zoom that a level exactly
    /// covers yields the level *below* it. Pinned as-is so the rewrite has to
    /// decide about it explicitly rather than drift.
    #[test]
    fn for_zoom_picks_the_level_below_the_first_match() {
        let samples: Vec<f32> = (0..4000).map(|i| i as f32).collect();
        let multi = MultiResolutionSummary::from_samples(&samples, ChannelLayout::Mono, 64, 3);
        // levels: 64, 128, 256

        // Finer than every level: level 0.
        assert_eq!(multi.for_zoom(1).samples_per_block, 64);
        // Exactly level 0's width: still level 0 (i == 0, no step back).
        assert_eq!(multi.for_zoom(64).samples_per_block, 64);
        // Exactly level 1's width: steps back to level 0.
        assert_eq!(multi.for_zoom(128).samples_per_block, 64);
        // Exactly level 2's width: steps back to level 1.
        assert_eq!(multi.for_zoom(256).samples_per_block, 128);
        // Coarser than every level: the coarsest.
        assert_eq!(multi.for_zoom(100_000).samples_per_block, 256);
    }

    #[test]
    fn test_streaming_append() {
        let mut summary = WaveformSummary::new(100);

        let chunk1: Vec<f32> = (0..250).map(|i| (i as f32 / 50.0).sin()).collect();
        let chunk2: Vec<f32> = (250..500).map(|i| (i as f32 / 50.0).sin()).collect();

        summary.append_samples(&chunk1);
        assert_eq!(summary.len(), 2); // 250 / 100 = 2 complete blocks

        summary.append_samples(&chunk2);
        assert_eq!(summary.len(), 5);
        assert_eq!(summary.total_samples, 500);
    }

    /// The law: streaming in arbitrary chunks must agree with one batch call.
    ///
    /// Chunk sizes deliberately coprime with the block size, so nearly every
    /// block spans a chunk boundary. The shipped implementation indexed into
    /// the current chunk using *global* offsets, so those spanning blocks were
    /// computed from a remnant or skipped outright: 500 ramp samples in two
    /// 250-chunks at block=100 produced 4 blocks instead of 5, with samples
    /// 100..199 never read.
    #[test]
    fn streaming_matches_batch_on_unaligned_chunks() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let block = 100;

        for chunk in [7usize, 100, 250, 333, 500, 501] {
            let mut streamed = WaveformSummary::new(block);
            for part in samples.chunks(chunk) {
                streamed.append_samples(part);
            }
            let streamed = streamed.finish();
            let batch = compute_summary(&samples, ChannelLayout::Mono, block);

            assert_eq!(
                streamed.len(),
                batch.len(),
                "block count differs at chunk size {chunk}"
            );
            assert_eq!(streamed.total_samples, batch.total_samples);
            for (i, (s, b)) in streamed.blocks.iter().zip(&batch.blocks).enumerate() {
                assert_eq!(s.min, b.min, "block {i} min differs at chunk size {chunk}");
                assert_eq!(s.max, b.max, "block {i} max differs at chunk size {chunk}");
                assert!(
                    (s.rms - b.rms).abs() < 1e-3,
                    "block {i} rms differs at chunk size {chunk}: {} vs {}",
                    s.rms,
                    b.rms
                );
            }
        }
    }

    /// The specific case from the repro: no block may be skipped.
    #[test]
    fn cross_chunk_blocks_are_not_dropped() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let mut summary = WaveformSummary::new(100);
        summary.append_samples(&samples[..250]);
        summary.append_samples(&samples[250..]);
        let summary = summary.finish();

        assert_eq!(summary.len(), 5);
        // Block 1 spans the chunk boundary and is the one that used to vanish.
        assert_eq!(summary.blocks[1].min, 100.0);
        assert_eq!(summary.blocks[1].max, 199.0);
        assert_eq!(summary.blocks[4].min, 400.0);
        assert_eq!(summary.blocks[4].max, 499.0);
    }
}
