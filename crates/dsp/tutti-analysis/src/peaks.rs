//! Waveform peak summaries — the min/max/RMS blocks a timeline draws.
//!
//! The carry is the partial block. Making it explicit is what fixes the
//! streaming path: the old version tracked a running total and indexed the
//! current chunk by *global* offset, but those offsets point into chunks that
//! have already been dropped, so any block spanning a chunk boundary was built
//! from a remnant or skipped outright.
//!
//! # Per channel, not folded
//!
//! Blocking is **per channel**: a stereo input yields two series, and
//! [`PeakBlocks::channel`] hands back one of them. Folding is a caller's
//! choice ([`PeakBlocks::to_mono`]), not this module's default.
//!
//! It was the default, and that was wrong for the consumer this module names in
//! its first line. Folding to mono before blocking destroys the per-channel
//! excursions at the earliest possible moment, and a waveform drawn from the
//! result misreports the audio:
//!
//! - a hard-panned double-track folds to something visibly narrower than either
//!   channel;
//! - a **phase-inverted** pair folds to a flat line — the clip looks like
//!   silence and is not.
//!
//! The rest of the module's channel-awareness was always right: [`PeakConfig`]
//! carries a [`ChannelLayout`], [`PeakConfig::chunk_matches`] rejects a
//! mis-strided chunk, and [`PeakState`] carries a ragged *frame* tail so a
//! caller feeding odd-length chunks loses nothing. The fold was the one step
//! that threw that care away before it could pay off.
//!
//! A meter or a loudness reading genuinely wants one number per block. Those
//! call [`PeakBlocks::to_mono`], where the reduction is visible at the call
//! site — and where its semantics can be stated, because they are *not* the
//! semantics of folding first.

use tutti_types::{Amplitude, ChannelLayout, Interleaved, Samples};

/// One block of a waveform summary, for one channel.
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

    /// The block covering both inputs' samples.
    ///
    /// Used two ways, and they are the same operation: merging adjacent blocks
    /// of one channel (how a coarser mipmap tier is built) and merging the same
    /// block across channels (how [`PeakBlocks::to_mono`] reduces).
    ///
    /// `min`/`max` merge exactly — the union of two ranges is the range of the
    /// union. **`rms` does not.** A root-mean-square is not linear in its
    /// inputs, so this returns the quadratic mean, `sqrt((a² + b²) / 2)`, which
    /// is exact only when the two blocks hold the same number of samples. Every
    /// caller in this crate merges equal-length blocks — pairwise tiers and
    /// per-channel reduction both do — except a ragged final block, where the
    /// `rms` of the last merged block is approximate and the `min`/`max` are
    /// not. Nothing draws `rms`; it is there for meters, which read whole
    /// blocks.
    #[inline]
    pub fn merge(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
            rms: Amplitude(((self.rms.0 * self.rms.0 + other.rms.0 * other.rms.0) * 0.5).sqrt()),
        }
    }
}

/// A whole summary: `layout.count()` channels, each of the same length.
///
/// **Channel-major and one flat allocation**, not a `Vec<Vec>`. Two reasons,
/// and the second is the one that matters downstream: `channel(c)` is a
/// contiguous slice, which is exactly the layout a GPU texture upload wants —
/// one row per channel — so a renderer hands the whole buffer over without
/// re-packing it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeakBlocks {
    /// Channel-major: channel `c` occupies `[c * per_channel .. (c+1) * per_channel]`.
    blocks: Vec<PeakBlock>,
    layout: ChannelLayout,
    per_channel: usize,
}

impl PeakBlocks {
    /// Wrap blocks that are already channel-major.
    ///
    /// For consumers that *derive* one summary from another rather than
    /// blocking samples — building a coarser mipmap tier by merging pairs, most
    /// of all. Those already hold the finished blocks, and routing them back
    /// through [`step_peaks`] would mean inventing samples that reduce to the
    /// values they already have.
    ///
    /// `blocks.len()` must be `layout.count() * per_channel`; a mismatch means
    /// the caller's own bookkeeping disagrees with itself, so this **panics**
    /// rather than truncating — a silently short channel would draw as a clip
    /// that ends early.
    pub fn from_channel_major(
        blocks: Vec<PeakBlock>,
        layout: ChannelLayout,
        per_channel: usize,
    ) -> Self {
        let expected = layout.count() as usize * per_channel;
        assert_eq!(
            blocks.len(),
            expected,
            "{} blocks for {} channels of {per_channel}",
            blocks.len(),
            layout.count(),
        );
        Self {
            blocks,
            layout,
            per_channel,
        }
    }

    /// The width these blocks describe.
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Blocks in **one** channel — the length of every [`channel`](Self::channel)
    /// slice, not the length of the backing buffer.
    #[inline]
    pub fn blocks_per_channel(&self) -> usize {
        self.per_channel
    }

    /// Whether there is not a single block.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.per_channel == 0
    }

    /// One channel's blocks, or `None` past the width.
    ///
    /// Returns `Option` rather than panicking because the channel index is
    /// routinely a loop bound from a *different* layout — a renderer drawing
    /// two rows over a mono summary, say — and "there is no such channel" is an
    /// ordinary answer there, not a bug.
    #[inline]
    pub fn channel(&self, ch: usize) -> Option<&[PeakBlock]> {
        if ch >= self.layout.count() as usize {
            return None;
        }
        Some(&self.blocks[ch * self.per_channel..(ch + 1) * self.per_channel])
    }

    /// Every channel's blocks, channel-major and contiguous.
    ///
    /// The upload path: one slice, no re-packing.
    #[inline]
    pub fn as_flat(&self) -> &[PeakBlock] {
        &self.blocks
    }

    /// Reduce to one series, merging the channels block by block.
    ///
    /// **This is not the same as folding the samples first, and the difference
    /// is the reason this module stopped doing that.** Folding samples then
    /// blocking gives the min/max of the *sum*, so a phase-inverted pair
    /// cancels to a flat line. Blocking then merging gives min-of-mins and
    /// max-of-maxes — the envelope of what is actually present, which cannot
    /// cancel.
    ///
    /// So this is the right reduction for a *waveform* that needs one row. It
    /// is **not** a downmix: a caller that wants the mono signal's own peaks —
    /// what the listener hears through a mono fold — folds the samples with
    /// [`Interleaved::fold_to_mono_into`] and blocks that instead.
    ///
    /// `rms` merges quadratically; see [`PeakBlock::merge`].
    pub fn to_mono(&self) -> Vec<PeakBlock> {
        let channels = self.layout.count() as usize;
        if channels == 0 || self.per_channel == 0 {
            return Vec::new();
        }
        let mut out = self.blocks[..self.per_channel].to_vec();
        for ch in 1..channels {
            let plane = &self.blocks[ch * self.per_channel..(ch + 1) * self.per_channel];
            for (acc, block) in out.iter_mut().zip(plane) {
                *acc = acc.merge(*block);
            }
        }
        out
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
/// mid-*frame* as well as mid-*block*. Deinterleaving each chunk independently
/// discards the ragged frame tail, so a stereo caller feeding odd-length chunks
/// loses samples permanently — the same class of loss the partial-block carry
/// exists to prevent, one level down.
///
/// That second carry is why [`step_peaks`] takes an
/// [`Interleaved`](tutti_types::Interleaved) rather than a slice plus a width:
/// the split between "consumed now" and "carried forward" is a *frame* boundary
/// inside a *sample* buffer, and that is precisely the confusion the type
/// exists to make unwritable.
///
/// The partial-block carry is **per channel** — the same invariant N times, not
/// a new one. Channels advance in lockstep (every whole frame contributes one
/// sample to each), so all `pending` planes are always the same length; that is
/// what lets [`step_peaks`] emit blocks channel-major without buffering a whole
/// chunk's output.
#[derive(Debug, Clone, Default)]
pub struct PeakState {
    /// Interleaved samples of an incomplete frame, awaiting the rest of it.
    partial_frame: Vec<f32>,
    /// Per channel: samples of an incomplete block. One plane per channel, all
    /// the same length.
    pending: Vec<Vec<f32>>,
    /// Per channel: scratch for one chunk's deinterleave. Not a carry — fully
    /// consumed within each [`step_peaks`] call, and living here only so the
    /// split reuses one allocation per channel across a stream instead of
    /// making fresh `Vec`s per chunk.
    planes: Vec<Vec<f32>>,
    consumed: Samples,
}

impl PeakState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whole frames consumed so far, excluding anything still pending.
    #[inline]
    pub fn consumed(&self) -> Samples {
        self.consumed
    }

    pub fn reset(&mut self) {
        self.partial_frame.clear();
        for plane in &mut self.pending {
            plane.clear();
        }
        // `planes` is scratch, not carry — its capacity is kept deliberately.
        for plane in &mut self.planes {
            plane.clear();
        }
        self.consumed = Samples(0);
    }
}

/// Block a chunk, appending whatever whole blocks it completes.
///
/// Chunks need not align to block or frame boundaries; both remainders are
/// carried. Output is **channel-major**, so a chunk that completes `n` blocks
/// appends `n` blocks for channel 0, then `n` for channel 1, and so on — which
/// is why the accumulator is a [`PeakBlocks`] and not a bare `Vec`: appending
/// channel-major to a flat buffer requires knowing where each channel's run
/// ends, and only the accumulator does.
pub fn step_peaks(
    cfg: &PeakConfig,
    state: &mut PeakState,
    chunk: Interleaved<'_>,
    out: &mut PeakAccum,
) {
    let block = cfg.samples_per_block.get();
    if block == 0 || !cfg.chunk_matches(chunk) {
        return;
    }
    let channels = cfg.layout.count() as usize;
    out.begin(cfg.layout);

    // Splice the carried half-frame in front of the chunk, deinterleave the
    // whole frames of the result, and carry whatever is left over — the ragged
    // *frame* tail, one level below the ragged *block* tail handled after this.
    //
    // `Interleaved` is what turns this from a hand-rolled realign into an
    // ordinary window: it tolerates a ragged tail by design (`len()` counts
    // whole frames and `window` is denominated in frames), so the *only* place
    // `× stride` appears is the split point of what was consumed from what is
    // carried. The common case — a caller whose chunks are already
    // frame-aligned, which is every render loop — splits the chunk in place and
    // touches the carry buffer not at all.
    let mut carry = core::mem::take(&mut state.partial_frame);
    let mut planes = core::mem::take(&mut state.planes);
    planes.resize_with(channels, Vec::new);
    if carry.is_empty() {
        chunk.deinterleave_into(&mut planes);
        carry.extend_from_slice(&chunk.samples()[chunk.len() * chunk.stride()..]);
    } else {
        carry.extend_from_slice(chunk.samples());
        let spliced = Interleaved::new(&carry, chunk.layout());
        let consumed_samples = spliced.len() * spliced.stride();
        spliced
            .window(0..spliced.len())
            .deinterleave_into(&mut planes);
        carry.drain(..consumed_samples);
    }
    state.partial_frame = carry;

    // Every plane holds the same frame count, so channel 0 speaks for all.
    state.consumed = Samples(state.consumed.get() + planes[0].len());

    state.pending.resize_with(channels, Vec::new);
    for (ch, plane) in planes.iter().enumerate().take(channels) {
        cut_blocks(&mut state.pending[ch], block, plane, out.channel_mut(ch));
    }
    state.planes = planes;
}

/// Cut one channel's samples into whole blocks, completing the pending one
/// first and carrying the remainder.
///
/// Takes `pending` alone rather than the whole [`PeakState`] so it can be
/// called once per channel while the state's other planes stay borrowed.
fn cut_blocks(pending: &mut Vec<f32>, block: usize, samples: &[f32], out: &mut Vec<PeakBlock>) {
    let mut rest = samples;

    // Finish the block the previous call left half-built before taking whole
    // blocks out of this chunk.
    if !pending.is_empty() {
        let needed = block - pending.len();
        let take = needed.min(rest.len());
        pending.extend_from_slice(&rest[..take]);
        rest = &rest[take..];

        if pending.len() < block {
            return;
        }
        out.push(summarize_block(pending));
        pending.clear();
    }

    let mut chunks = rest.chunks_exact(block);
    for whole in chunks.by_ref() {
        out.push(summarize_block(whole));
    }
    pending.extend_from_slice(chunks.remainder());
}

/// Emit the trailing partial block of every channel, if any, and hand back the
/// finished summary.
///
/// [`summarize`] keeps a short final block, so the streaming path must too or
/// the two disagree on any input that is not a whole number of blocks.
pub fn finish(state: &mut PeakState, out: PeakAccum) -> PeakBlocks {
    let mut out = out;
    for (ch, pending) in state.pending.iter_mut().enumerate() {
        if !pending.is_empty() {
            out.channel_mut(ch).push(summarize_block(pending));
            pending.clear();
        }
    }
    out.into_blocks()
}

/// Where [`step_peaks`] appends, and what [`finish`] turns into a
/// [`PeakBlocks`].
///
/// A separate type from the finished summary because the two want opposite
/// layouts: appending wants one growable run per channel, while the result
/// wants one flat channel-major buffer. Flattening once at the end beats
/// splicing into the middle of a flat buffer on every chunk.
#[derive(Debug, Clone, Default)]
pub struct PeakAccum {
    channels: Vec<Vec<PeakBlock>>,
    layout: ChannelLayout,
}

impl PeakAccum {
    pub fn new() -> Self {
        Self::default()
    }

    /// Widen to `layout` if this is the first chunk. Idempotent.
    fn begin(&mut self, layout: ChannelLayout) {
        self.layout = layout;
        self.channels.resize_with(layout.count() as usize, Vec::new);
    }

    fn channel_mut(&mut self, ch: usize) -> &mut Vec<PeakBlock> {
        &mut self.channels[ch]
    }

    /// Flatten to channel-major.
    ///
    /// Channels are equal-length by construction — they advance one sample per
    /// whole frame — so the shortest is the true count and a longer one would
    /// be a bug in this module rather than bad input. `min` here is a floor
    /// that keeps a hypothetical mismatch from producing a jagged buffer
    /// downstream, not a case that should occur.
    fn into_blocks(self) -> PeakBlocks {
        let per_channel = self.channels.iter().map(Vec::len).min().unwrap_or(0);
        let mut blocks = Vec::with_capacity(per_channel * self.channels.len());
        for channel in &self.channels {
            blocks.extend_from_slice(&channel[..per_channel]);
        }
        PeakBlocks {
            blocks,
            layout: self.layout,
            per_channel,
        }
    }
}

/// Summarize a whole buffer, per channel.
///
/// Folds [`step_peaks`] — the same implementation the streaming path uses.
pub fn summarize(cfg: &PeakConfig, buffer: Interleaved<'_>) -> PeakBlocks {
    let mut out = PeakAccum::new();
    let mut state = PeakState::new();
    step_peaks(cfg, &mut state, buffer, &mut out);
    finish(&mut state, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(block: usize) -> PeakConfig {
        PeakConfig::new(Samples(block), ChannelLayout::MONO)
    }

    /// Channel `ch`'s blocks, for tests that know the channel exists.
    fn ch(blocks: &PeakBlocks, ch: usize) -> &[PeakBlock] {
        blocks.channel(ch).expect("channel is within the layout")
    }

    /// Stream `samples` in `chunk`-sample pieces. The streaming path, wrapped
    /// so a test states only what it varies.
    fn stream(cfg: &PeakConfig, samples: &[f32], chunk: usize) -> PeakBlocks {
        let mut state = PeakState::new();
        let mut out = PeakAccum::new();
        for part in samples.chunks(chunk) {
            step_peaks(
                cfg,
                &mut state,
                Interleaved::new(part, cfg.layout),
                &mut out,
            );
        }
        finish(&mut state, out)
    }

    #[test]
    fn block_values_are_exact_for_a_ramp() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), Interleaved::new(&samples, ChannelLayout::MONO));

        assert_eq!(blocks.blocks_per_channel(), 5);
        for (i, block) in ch(&blocks, 0).iter().enumerate() {
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
                    let streamed = stream(&cfg, &samples, chunk);

                    assert_eq!(
                        streamed, batch,
                        "{layout:?}, block {block}, chunk {chunk} disagrees with batch"
                    );
                    // `consumed` is not observable through `stream`, so re-run
                    // the one case that reports it.
                    let mut state = PeakState::new();
                    let mut out = PeakAccum::new();
                    for part in samples.chunks(chunk) {
                        step_peaks(&cfg, &mut state, Interleaved::new(part, layout), &mut out);
                    }
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
        // Two frames: L = (1, 3), R = (2, 4).
        let samples = [1.0f32, 2.0, 3.0, 4.0];

        // Fed one sample at a time, every chunk ends mid-frame.
        let streamed = stream(&cfg, &samples, 1);

        assert_eq!(streamed.blocks_per_channel(), 2, "both frames must survive");
        assert_eq!(ch(&streamed, 0)[0].min, 1.0);
        assert_eq!(ch(&streamed, 1)[0].min, 2.0);
        assert_eq!(ch(&streamed, 0)[1].min, 3.0);
        assert_eq!(ch(&streamed, 1)[1].min, 4.0);
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
        let mut out = PeakAccum::new();

        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&[1.0, 2.0, 3.0, 4.0], ChannelLayout::QUAD),
            &mut out,
        );
        let blocks = finish(&mut state, out);

        assert!(
            blocks.is_empty(),
            "a quad chunk must not be blocked as stereo"
        );
        assert_eq!(state.consumed(), Samples(0));
    }

    #[test]
    fn a_trailing_partial_block_is_kept() {
        let samples: Vec<f32> = (0..250).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), Interleaved::new(&samples, ChannelLayout::MONO));

        assert_eq!(blocks.blocks_per_channel(), 3);
        assert_eq!(ch(&blocks, 0)[2].min, 200.0);
        assert_eq!(ch(&blocks, 0)[2].max, 249.0, "only the 50 samples present");
    }

    /// A ragged final block must be kept for **every** channel, not just the
    /// first — the per-channel form of the trailing-block rule.
    #[test]
    fn a_trailing_partial_block_is_kept_on_every_channel() {
        // 250 stereo frames at block 100: two whole blocks and a 50-frame tail.
        let samples: Vec<f32> = (0..250).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100), ChannelLayout::STEREO),
            Interleaved::new(&samples, ChannelLayout::STEREO),
        );

        assert_eq!(blocks.blocks_per_channel(), 3);
        assert_eq!(ch(&blocks, 0)[2].max, 249.0);
        assert_eq!(ch(&blocks, 1)[2].min, -249.0);
    }

    /// **The bug this module was changed to fix.**
    ///
    /// A phase-inverted pair is the worst case: folding the samples before
    /// blocking cancels it to a flat line, so the clip renders as silence and
    /// is not. Per channel, both channels are a full-scale ramp.
    ///
    /// Mutation check: under the previous implementation every assertion below
    /// except the block count fails — `summarize` returned one series whose
    /// `min` and `max` were both 0.0. If this test ever passes against a
    /// folding implementation it has stopped testing what it names.
    #[test]
    fn a_phase_inverted_pair_does_not_cancel() {
        // L ramps up, R is its exact negation.
        let samples: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100), ChannelLayout::STEREO),
            Interleaved::new(&samples, ChannelLayout::STEREO),
        );

        assert_eq!(blocks.blocks_per_channel(), 2, "frames, not samples");
        assert_eq!(blocks.layout(), ChannelLayout::STEREO);

        // Neither channel is silent, and they are opposite.
        assert_eq!(ch(&blocks, 0)[0].min, 0.0);
        assert_eq!(ch(&blocks, 0)[0].max, 99.0);
        assert_eq!(ch(&blocks, 1)[0].min, -99.0);
        assert_eq!(ch(&blocks, 1)[0].max, 0.0);

        // And the reduction keeps the envelope rather than cancelling it —
        // this is what distinguishes `to_mono` from folding first.
        let merged = blocks.to_mono();
        assert_eq!(merged[0].min, -99.0);
        assert_eq!(merged[0].max, 99.0);
    }

    /// A hard-panned pair: one channel silent, the other full-scale.
    ///
    /// Folding halved the amplitude, so the clip drew at half height. Per
    /// channel the loud side is untouched and the silent side is visibly
    /// silent — which is the information a panned double-track needs to show.
    #[test]
    fn a_hard_panned_pair_keeps_the_loud_channel_at_full_scale() {
        // L silent, R full-scale square.
        let samples: Vec<f32> = (0..200)
            .flat_map(|i| [0.0, if i % 2 == 0 { 1.0 } else { -1.0 }])
            .collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100), ChannelLayout::STEREO),
            Interleaved::new(&samples, ChannelLayout::STEREO),
        );

        assert_eq!(ch(&blocks, 0)[0].min, 0.0, "silent channel is silent");
        assert_eq!(ch(&blocks, 0)[0].max, 0.0);
        assert_eq!(ch(&blocks, 1)[0].peak(), Amplitude(1.0), "not halved");
    }

    /// Every channel is blocked, including ones the downmix matrices weight to
    /// zero — a surround waveform shows what is in each channel, not what a
    /// stereo listener would hear.
    #[test]
    fn surround_keeps_every_channel_separately() {
        // 5.1 with only the centre non-zero.
        let layout = ChannelLayout::from(6u16);
        let samples: Vec<f32> = (0..100)
            .flat_map(|_| [0.0, 0.0, 1.0, 0.0, 0.0, 0.0])
            .collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(50), layout),
            Interleaved::new(&samples, layout),
        );

        assert_eq!(blocks.blocks_per_channel(), 2);
        assert_eq!(blocks.layout(), layout);
        assert_eq!(ch(&blocks, 2)[0].max, 1.0, "centre, at full scale");
        for silent in [0, 1, 3, 4, 5] {
            assert_eq!(
                ch(&blocks, silent)[0].max,
                0.0,
                "channel {silent} must stay silent, not borrow the centre"
            );
        }
        assert!(blocks.channel(6).is_none(), "no seventh channel");
    }

    /// On mono input the two semantics must coincide — the one case where
    /// `to_mono` reproduces the old output exactly.
    #[test]
    fn to_mono_on_mono_input_is_the_channel_itself() {
        let samples: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let blocks = summarize(&mono(100), Interleaved::new(&samples, ChannelLayout::MONO));

        assert_eq!(blocks.to_mono(), ch(&blocks, 0));
    }

    /// `as_flat` is channel-major and contiguous — the property the GPU upload
    /// path depends on, so it is asserted rather than assumed.
    #[test]
    fn as_flat_is_channel_major() {
        let samples: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let blocks = summarize(
            &PeakConfig::new(Samples(100), ChannelLayout::STEREO),
            Interleaved::new(&samples, ChannelLayout::STEREO),
        );

        let flat = blocks.as_flat();
        let per = blocks.blocks_per_channel();
        assert_eq!(flat.len(), per * 2);
        assert_eq!(&flat[..per], ch(&blocks, 0));
        assert_eq!(&flat[per..], ch(&blocks, 1));
    }

    #[test]
    fn consumed_counts_frames_not_samples() {
        let cfg = PeakConfig::new(Samples(100), ChannelLayout::STEREO);
        let mut state = PeakState::new();
        let mut out = PeakAccum::new();

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

        // `to_mono` and `channel` on an empty summary answer rather than panic.
        let empty = summarize(&mono(100), Interleaved::new(&[], ChannelLayout::MONO));
        assert!(empty.to_mono().is_empty());
        assert!(empty.as_flat().is_empty());

        let mut state = PeakState::new();
        let blocks = finish(&mut state, PeakAccum::new());
        assert!(blocks.is_empty(), "finish on an empty state emits nothing");
    }

    #[test]
    fn reset_returns_to_a_fresh_state() {
        let cfg = mono(100);
        let mut state = PeakState::new();
        let mut out = PeakAccum::new();

        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&[1.0; 150], ChannelLayout::MONO),
            &mut out,
        );
        state.reset();
        let mut out = PeakAccum::new();

        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&[2.0; 100], ChannelLayout::MONO),
            &mut out,
        );
        let blocks = finish(&mut state, out);

        assert_eq!(
            blocks.blocks_per_channel(),
            1,
            "the carried 50 samples were discarded"
        );
        assert_eq!(ch(&blocks, 0)[0].min, 2.0);
        assert_eq!(state.consumed(), Samples(100));
    }

    /// `reset` must clear every channel's carry, not just the first — a stereo
    /// stream reset mid-block would otherwise splice the old right channel onto
    /// the new one.
    #[test]
    fn reset_clears_every_channel_carry() {
        let cfg = PeakConfig::new(Samples(100), ChannelLayout::STEREO);
        let mut state = PeakState::new();
        let mut out = PeakAccum::new();

        // 50 frames: half a block on both channels, nothing emitted.
        let partial: Vec<f32> = (0..50).flat_map(|_| [1.0f32, -1.0]).collect();
        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&partial, ChannelLayout::STEREO),
            &mut out,
        );
        state.reset();

        // A full block of a different value. If either carry survived, its
        // channel would report the old value's excursion too.
        let full: Vec<f32> = (0..100).flat_map(|_| [2.0f32, -2.0]).collect();
        let mut out = PeakAccum::new();
        step_peaks(
            &cfg,
            &mut state,
            Interleaved::new(&full, ChannelLayout::STEREO),
            &mut out,
        );
        let blocks = finish(&mut state, out);

        assert_eq!(blocks.blocks_per_channel(), 1);
        assert_eq!(ch(&blocks, 0)[0].min, 2.0, "left carry survived reset");
        assert_eq!(ch(&blocks, 1)[0].max, -2.0, "right carry survived reset");
    }

    /// `merge` is the tier-building and channel-reducing primitive, so its
    /// range union is exact and its `rms` is the quadratic mean.
    #[test]
    fn merge_unions_the_range_and_takes_the_quadratic_mean() {
        let a = PeakBlock {
            min: -1.0,
            max: 0.5,
            rms: Amplitude(0.0),
        };
        let b = PeakBlock {
            min: -0.25,
            max: 2.0,
            rms: Amplitude(1.0),
        };
        let m = a.merge(b);

        assert_eq!(m.min, -1.0, "the lower of the two mins");
        assert_eq!(m.max, 2.0, "the upper of the two maxes");
        // sqrt((0² + 1²) / 2), not (0 + 1) / 2.
        assert!((m.rms.0 - 0.5f32.sqrt()).abs() < 1e-6, "got {}", m.rms.0);
    }
}
