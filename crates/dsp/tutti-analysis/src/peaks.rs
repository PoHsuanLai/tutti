//! Waveform peak summaries — the min/max/RMS blocks a timeline draws.
//!
//! The carry is the partial block. Making it explicit is what fixes the
//! streaming path: the old version tracked a running total and indexed the
//! current chunk by *global* offset, but those offsets point into chunks that
//! have already been dropped, so any block spanning a chunk boundary was built
//! from a remnant or skipped outright.

use tutti_types::{Amplitude, ChannelLayout, Interleaved, Samples};

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

    /// Whether `chunk` is the width this config blocks at.
    ///
    /// The carried half-frame is `layout`-wide, so a chunk of a different width
    /// spliced onto it would realign at the wrong stride and fold two adjacent
    /// chunks into one wrong frame. That disagreement was inexpressible while
    /// the chunk was a bare slice.
    #[inline]
    pub fn chunk_matches(&self, chunk: Interleaved<'_>) -> bool {
        chunk.layout() == self.layout
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
///
/// **Two** carries, not one, and the second is easy to forget: a chunk can end
/// mid-*frame* as well as mid-*block*. Folding each chunk to mono
/// independently discards the ragged frame tail, so a stereo caller feeding
/// odd-length chunks loses samples permanently — the same class of loss the
/// partial-block carry exists to prevent, one level down.
///
/// That second carry is why [`step_peaks`] takes an
/// [`Interleaved`](tutti_types::Interleaved) rather than a slice plus a width:
/// the split between "folded now" and "carried forward" is a *frame* boundary
/// inside a *sample* buffer, and that is precisely the confusion the type
/// exists to make unwritable.
#[derive(Debug, Clone, Default)]
pub struct PeakState {
    /// Interleaved samples of an incomplete frame, awaiting the rest of it.
    partial_frame: Vec<f32>,
    /// Folded mono samples of an incomplete block.
    pending: Vec<f32>,
    /// Scratch for one chunk's mono fold. Not a carry — it is fully consumed
    /// within each [`step_peaks`] call and lives here only so the fold reuses
    /// one allocation across a stream instead of making a fresh `Vec` per chunk.
    mono: Vec<f32>,
    consumed: Samples,
}

impl PeakState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whole frames folded so far, excluding anything still pending.
    #[inline]
    pub fn consumed(&self) -> Samples {
        self.consumed
    }

    pub fn reset(&mut self) {
        self.partial_frame.clear();
        self.pending.clear();
        // `mono` is scratch, not carry — its capacity is kept deliberately.
        self.mono.clear();
        self.consumed = Samples(0);
    }
}

/// Fold a chunk, appending whatever whole blocks it completes.
///
/// Chunks need not align to block or frame boundaries; both remainders are
/// carried.
pub fn step_peaks(
    cfg: &PeakConfig,
    state: &mut PeakState,
    chunk: Interleaved<'_>,
    out: &mut Vec<PeakBlock>,
) {
    let block = cfg.samples_per_block.get();
    if block == 0 || !cfg.chunk_matches(chunk) {
        return;
    }

    // Splice the carried half-frame in front of the chunk, fold the whole
    // frames of the result, and carry whatever is left over — the ragged
    // *frame* tail, one level below the ragged *block* tail handled after this.
    //
    // `Interleaved` is what turns this from a hand-rolled realign into an
    // ordinary window: it tolerates a ragged tail by design (`len()` counts
    // whole frames and `window` is denominated in frames), so the *only* place
    // `× stride` appears is the split point of what was folded from what is
    // carried. The common case — a caller whose chunks are already
    // frame-aligned, which is every render loop — now folds the chunk in place
    // and touches the carry buffer not at all.
    let mut carry = core::mem::take(&mut state.partial_frame);
    let mut mono = core::mem::take(&mut state.mono);
    if carry.is_empty() {
        chunk.fold_to_mono_into(&mut mono);
        carry.extend_from_slice(&chunk.samples()[chunk.len() * chunk.stride()..]);
    } else {
        carry.extend_from_slice(chunk.samples());
        let spliced = Interleaved::new(&carry, chunk.layout());
        let folded_samples = spliced.len() * spliced.stride();
        spliced
            .window(0..spliced.len())
            .fold_to_mono_into(&mut mono);
        carry.drain(..folded_samples);
    }
    state.partial_frame = carry;

    state.consumed = Samples(state.consumed.get() + mono.len());
    fold_blocks(state, block, &mono, out);
    state.mono = mono;
}

/// Cut `mono` into whole blocks, completing the pending one first and carrying
/// the remainder. Split out of [`step_peaks`] only so the scratch buffer it
/// reads can be handed straight back to the state on return.
fn fold_blocks(state: &mut PeakState, block: usize, mono: &[f32], out: &mut Vec<PeakBlock>) {
    let mut rest = mono;

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
pub fn summarize(cfg: &PeakConfig, buffer: Interleaved<'_>) -> Vec<PeakBlock> {
    let mut out = Vec::new();
    let mut state = PeakState::new();
    step_peaks(cfg, &mut state, buffer, &mut out);
    finish(&mut state, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(block: usize) -> PeakConfig {
        PeakConfig::new(Samples(block), ChannelLayout::MONO)
    }

    #[test]
    fn block_values_are_exact_for_a_ramp() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), Interleaved::new(&samples, ChannelLayout::MONO));

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
    /// Swept across **every layout**, not just mono. The first version of this
    /// test only ran at `ChannelLayout::MONO`, where folding short-circuits to
    /// a copy — so it proved nothing about the fold, and missed a bug that
    /// silently discarded audio on every non-frame-aligned stereo chunk.
    ///
    /// Chunk sizes are mostly coprime with both the block size and the channel
    /// counts, so nearly every chunk ends mid-frame *and* mid-block.
    #[test]
    fn streaming_matches_batch_at_every_chunk_size_and_layout() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::QUAD,
            ChannelLayout::from(6u16),
        ] {
            let channels = layout.count() as usize;
            // 500 whole frames, so batch and streaming see the same input.
            let samples: Vec<f32> = (0..500 * channels).map(|i| i as f32).collect();

            for block in [1usize, 3, 100] {
                let cfg = PeakConfig::new(Samples(block), layout);
                let batch = summarize(&cfg, Interleaved::new(&samples, layout));

                for chunk in [1usize, 3, 7, 33, 100, 250, 333, 501, 1024] {
                    let mut state = PeakState::new();
                    let mut streamed = Vec::new();
                    for part in samples.chunks(chunk) {
                        step_peaks(
                            &cfg,
                            &mut state,
                            Interleaved::new(part, layout),
                            &mut streamed,
                        );
                    }
                    finish(&mut state, &mut streamed);

                    assert_eq!(
                        streamed, batch,
                        "{layout:?}, block {block}, chunk {chunk} disagrees with batch"
                    );
                    assert_eq!(
                        state.consumed(),
                        Samples(500),
                        "{layout:?}, block {block}, chunk {chunk}: frame count"
                    );
                }
            }
        }
    }

    /// The specific loss the layout sweep exists to catch: a stereo chunk that
    /// ends between L and R must carry that half-frame, not drop it.
    #[test]
    fn a_chunk_ending_mid_frame_carries_the_partial_frame() {
        let cfg = PeakConfig::new(Samples(1), ChannelLayout::STEREO);
        // Two frames: (1,2) folds to 1.5, (3,4) folds to 3.5.
        let samples = [1.0f32, 2.0, 3.0, 4.0];

        let mut state = PeakState::new();
        let mut streamed = Vec::new();
        // Fed one sample at a time, every chunk ends mid-frame.
        for part in samples.chunks(1) {
            step_peaks(
                &cfg,
                &mut state,
                Interleaved::new(part, ChannelLayout::STEREO),
                &mut streamed,
            );
        }
        finish(&mut state, &mut streamed);

        assert_eq!(streamed.len(), 2, "both frames must survive");
        assert_eq!(streamed[0].min, 1.5);
        assert_eq!(streamed[1].min, 3.5);
        assert_eq!(state.consumed(), Samples(2));
    }

    /// A chunk whose own width disagrees with the config's is skipped.
    ///
    /// The carried half-frame is `cfg.layout`-wide. Splicing a chunk of a
    /// different width onto it realigns at the wrong stride, so the frame
    /// straddling the join is built from two channels that were never adjacent.
    /// Before the width travelled with the buffer this was not a case anyone
    /// could write down — the config's layout was simply assumed to describe
    /// whatever slice arrived.
    #[test]
    fn a_chunk_of_the_wrong_width_is_skipped() {
        let cfg = PeakConfig::new(Samples(1), ChannelLayout::STEREO);
        let mut state = PeakState::new();
        let mut out = Vec::new();

        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&[1.0, 2.0, 3.0, 4.0], ChannelLayout::QUAD),
            &mut out,
        );
        finish(&mut state, &mut out);

        assert!(out.is_empty(), "a quad chunk must not be blocked as stereo");
        assert_eq!(state.consumed(), Samples(0));
    }

    #[test]
    fn a_trailing_partial_block_is_kept() {
        let samples: Vec<f32> = (0..250).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), Interleaved::new(&samples, ChannelLayout::MONO));

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2].min, 200.0);
        assert_eq!(blocks[2].max, 249.0, "only the 50 samples present");
    }

    #[test]
    fn stereo_folds_rather_than_reading_one_channel() {
        // Left ramps, right is its negation, so a fold gives silence while
        // reading channel 0 alone would give the ramp.
        let samples: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100), ChannelLayout::STEREO),
            Interleaved::new(&samples, ChannelLayout::STEREO),
        );

        assert_eq!(blocks.len(), 2, "frames, not interleaved samples");
        assert_eq!(blocks[0].min, 0.0);
        assert_eq!(blocks[0].max, 0.0, "L and R cancel");
    }

    /// Surround input folds every channel — the case the app-side hand-rolled
    /// downmixes got wrong.
    #[test]
    fn surround_keeps_the_centre_channel() {
        // 5.1 with only the centre non-zero.
        let samples: Vec<f32> = (0..100)
            .flat_map(|_| [0.0, 0.0, 1.0, 0.0, 0.0, 0.0])
            .collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(50), ChannelLayout::from(6u16)),
            Interleaved::new(&samples, ChannelLayout::from(6u16)),
        );

        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].max > 0.0, "centre must survive the fold");
    }

    #[test]
    fn consumed_counts_frames_not_samples() {
        let cfg = PeakConfig::new(Samples(100), ChannelLayout::STEREO);
        let mut state = PeakState::new();
        let mut out = Vec::new();

        let samples: Vec<f32> = (0..400).map(|i| i as f32).collect(); // 200 frames
        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&samples, ChannelLayout::STEREO),
            &mut out,
        );

        assert_eq!(state.consumed(), Samples(200));
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(summarize(&mono(100), Interleaved::new(&[], ChannelLayout::MONO)).is_empty());
        assert!(summarize(&mono(0), Interleaved::new(&[1.0, 2.0], ChannelLayout::MONO)).is_empty());
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

        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&[1.0; 150], ChannelLayout::MONO),
            &mut out,
        );
        state.reset();
        out.clear();

        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&[2.0; 100], ChannelLayout::MONO),
            &mut out,
        );
        finish(&mut state, &mut out);

        assert_eq!(out.len(), 1, "the carried 50 samples were discarded");
        assert_eq!(out[0].min, 2.0);
        assert_eq!(state.consumed(), Samples(100));
    }
}
