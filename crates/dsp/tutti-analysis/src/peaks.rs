//! Waveform peak summaries — the min/max/RMS blocks a timeline draws.
//!
//! The carry is the partial block. Making it explicit is what fixes the
//! streaming path: the old version tracked a running total and indexed the
//! current chunk by *global* offset, but those offsets point into chunks that
//! have already been dropped, so any block spanning a chunk boundary was built
//! from a remnant or skipped outright.

use tutti_types::{Amplitude, ChannelLayout, Samples};

/// One block of a waveform summary.
///
/// Array-of-structs on purpose: every consumer reads all three fields
/// together, both to draw and to serialize.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PeakBlock {
    pub min: f32,
    pub max: f32,
    pub rms: Amplitude,
}

impl PeakBlock {
    /// The larger of the two excursions — what a peak meter shows.
    #[inline]
    pub fn peak(&self) -> Amplitude {
        Amplitude(self.min.abs().max(self.max.abs()))
    }
}

/// How input is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakConfig {
    pub samples_per_block: Samples,
    pub layout: ChannelLayout,
}

impl PeakConfig {
    pub fn new(samples_per_block: impl Into<Samples>, layout: ChannelLayout) -> Self {
        Self {
            samples_per_block: samples_per_block.into(),
            layout,
        }
    }
}

/// Summarize one block. Stateless.
pub fn summarize_block(samples: &[f32]) -> PeakBlock {
    if samples.is_empty() {
        return PeakBlock::default();
    }

    let (min, max, sum_sq) = samples.iter().fold(
        (f32::INFINITY, f32::NEG_INFINITY, 0.0f32),
        |(min, max, sum), &s| (min.min(s), max.max(s), sum + s * s),
    );

    PeakBlock {
        min,
        max,
        rms: Amplitude((sum_sq / samples.len() as f32).sqrt()),
    }
}

/// Samples not yet forming a whole block.
///
/// The state the old implementation lacked, which is why non-aligned chunks
/// dropped a block.
#[derive(Debug, Clone, Default)]
pub struct PeakState {
    pending: Vec<f32>,
    consumed: Samples,
}

impl PeakState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whole samples folded so far, excluding anything still pending.
    #[inline]
    pub fn consumed(&self) -> Samples {
        self.consumed
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.consumed = Samples(0);
    }
}

/// Fold a chunk, appending whatever whole blocks it completes.
///
/// Chunks need not align to block boundaries; the remainder is carried.
pub fn step_peaks(
    cfg: &PeakConfig,
    state: &mut PeakState,
    chunk: &[f32],
    out: &mut Vec<PeakBlock>,
) {
    let block = cfg.samples_per_block.get();
    if block == 0 {
        return;
    }

    let mono = crate::fold_buffer_to_mono(chunk, cfg.layout);
    state.consumed = Samples(state.consumed.get() + mono.len());

    let mut rest = mono.as_slice();

    // Finish the block the previous call left half-built before taking whole
    // blocks out of this chunk.
    if !state.pending.is_empty() {
        let needed = block - state.pending.len();
        let take = needed.min(rest.len());
        state.pending.extend_from_slice(&rest[..take]);
        rest = &rest[take..];

        if state.pending.len() < block {
            return;
        }
        out.push(summarize_block(&state.pending));
        state.pending.clear();
    }

    let mut chunks = rest.chunks_exact(block);
    for whole in chunks.by_ref() {
        out.push(summarize_block(whole));
    }
    state.pending.extend_from_slice(chunks.remainder());
}

/// Emit the trailing partial block, if any.
///
/// [`summarize`] keeps a short final block, so the streaming path must too or
/// the two disagree on any input that is not a whole number of blocks.
pub fn finish(state: &mut PeakState, out: &mut Vec<PeakBlock>) {
    if !state.pending.is_empty() {
        out.push(summarize_block(&state.pending));
        state.pending.clear();
    }
}

/// Summarize a whole buffer.
///
/// Folds [`step_peaks`] — the same implementation the streaming path uses.
pub fn summarize(cfg: &PeakConfig, samples: &[f32]) -> Vec<PeakBlock> {
    let mut out = Vec::new();
    let mut state = PeakState::new();
    step_peaks(cfg, &mut state, samples, &mut out);
    finish(&mut state, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(block: usize) -> PeakConfig {
        PeakConfig::new(Samples(block), ChannelLayout::Mono)
    }

    #[test]
    fn block_values_are_exact_for_a_ramp() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), &samples);

        assert_eq!(blocks.len(), 5);
        for (i, block) in blocks.iter().enumerate() {
            let lo = (i * 100) as f32;
            assert_eq!(block.min, lo);
            assert_eq!(block.max, lo + 99.0);
            assert_eq!(block.peak(), Amplitude(lo + 99.0));
        }
    }

    /// The law: streaming in arbitrary chunks equals one batch call.
    ///
    /// Chunk sizes mostly coprime with the block size, so nearly every block
    /// spans a boundary — the case the old implementation dropped.
    #[test]
    fn streaming_matches_batch_at_every_chunk_size() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let cfg = mono(100);
        let batch = summarize(&cfg, &samples);

        for chunk in [1usize, 7, 33, 100, 250, 333, 500, 501] {
            let mut state = PeakState::new();
            let mut streamed = Vec::new();
            for part in samples.chunks(chunk) {
                step_peaks(&cfg, &mut state, part, &mut streamed);
            }
            finish(&mut state, &mut streamed);

            assert_eq!(streamed, batch, "chunk size {chunk} disagrees with batch");
        }
    }

    #[test]
    fn a_trailing_partial_block_is_kept() {
        let samples: Vec<f32> = (0..250).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), &samples);

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2].min, 200.0);
        assert_eq!(blocks[2].max, 249.0, "only the 50 samples present");
    }

    #[test]
    fn stereo_folds_rather_than_reading_one_channel() {
        // Left ramps, right is its negation, so a fold gives silence while
        // reading channel 0 alone would give the ramp.
        let samples: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(&PeakConfig::new(Samples(100), ChannelLayout::Stereo), &samples);

        assert_eq!(blocks.len(), 2, "frames, not interleaved samples");
        assert_eq!(blocks[0].min, 0.0);
        assert_eq!(blocks[0].max, 0.0, "L and R cancel");
    }

    /// Surround input folds every channel — the case the app-side hand-rolled
    /// downmixes got wrong.
    #[test]
    fn surround_keeps_the_centre_channel() {
        // 5.1 with only the centre non-zero.
        let samples: Vec<f32> = (0..100).flat_map(|_| [0.0, 0.0, 1.0, 0.0, 0.0, 0.0]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(50), ChannelLayout::Multi(6)),
            &samples,
        );

        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].max > 0.0, "centre must survive the fold");
    }

    #[test]
    fn consumed_counts_frames_not_samples() {
        let cfg = PeakConfig::new(Samples(100), ChannelLayout::Stereo);
        let mut state = PeakState::new();
        let mut out = Vec::new();

        let samples: Vec<f32> = (0..400).map(|i| i as f32).collect(); // 200 frames
        step_peaks(&cfg, &mut state, &samples, &mut out);

        assert_eq!(state.consumed(), Samples(200));
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(summarize(&mono(100), &[]).is_empty());
        assert!(summarize(&mono(0), &[1.0, 2.0]).is_empty());
        assert_eq!(summarize_block(&[]), PeakBlock::default());

        let mut state = PeakState::new();
        let mut out = Vec::new();
        finish(&mut state, &mut out);
        assert!(out.is_empty(), "finish on an empty state emits nothing");
    }

    #[test]
    fn reset_returns_to_a_fresh_state() {
        let cfg = mono(100);
        let mut state = PeakState::new();
        let mut out = Vec::new();

        step_peaks(&cfg, &mut state, &[1.0; 150], &mut out);
        state.reset();
        out.clear();

        step_peaks(&cfg, &mut state, &[2.0; 100], &mut out);
        finish(&mut state, &mut out);

        assert_eq!(out.len(), 1, "the carried 50 samples were discarded");
        assert_eq!(out[0].min, 2.0);
        assert_eq!(state.consumed(), Samples(100));
    }
}
