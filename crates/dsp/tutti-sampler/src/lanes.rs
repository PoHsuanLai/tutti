//! Planar scratch for the block read, and the kernels that run over it.
//!
//! A voice renders a block at a time into [`Lanes`] — one contiguous run of
//! frames per channel — and the owner then adds each lane into its output
//! with one [`accumulate`] per channel. The per-voice read is gather-bound
//! (every voice reads a different wave at a different fractional position),
//! so the vectors run *along time*, over the lanes, not across voices.
//!
//! The kernels are plain loops over equal-length slices, written so the
//! compiler vectorises them (no bounds checks inside the loop: both slices
//! are cut to one length first). Each is the per-element arithmetic of a
//! frame-at-a-time read, in the same order, so a block read is bit-identical
//! to reading its frames one by one — IEEE-754 `+` and `*` on one element
//! give one answer whether they run four at a time or one.
//!
//! **Sized from `Prepare::max_block`.** A rendering node builds its
//! [`BlockScratch`] on the control thread and resizes it in its `prepare`,
//! so the lanes are as long as the longest block the executor will hand it
//! and a block renders in one piece. Nothing here is a fixed frame count.

use crate::MAX_SAMPLER_CHANNELS;
use tutti_core::SamplePosition;

/// Frames scratch built before any `prepare` holds: enough for a node a
/// host drives by hand without preparing it. A node handed a block longer
/// than its scratch renders it in pieces of [`BlockScratch::frames`], so
/// this is a floor for an unprepared node, never a ceiling.
pub(crate) const UNPREPARED_FRAMES: usize = 64;

/// One channel of a block, planar: as long as its scratch's frames.
pub(crate) type Lane = Vec<f32>;

/// A block of planar scratch, [`MAX_SAMPLER_CHANNELS`] lanes wide, each
/// lane as long as the longest block its owner is prepared for.
///
/// Owned by the node that renders through it, built and resized on the
/// control thread, boxed so a node that moves does not move it all. Carries
/// nothing between blocks: every read writes each frame it hands on.
pub(crate) struct Lanes(Box<[Lane; MAX_SAMPLER_CHANNELS]>);

impl Lanes {
    /// Zeroed scratch, `frames` frames per lane. Allocates: control thread.
    fn new(frames: usize) -> Self {
        Self(Box::new(std::array::from_fn(|_| vec![0.0; frames])))
    }

    /// Frames each lane holds.
    #[inline]
    pub(crate) fn frames(&self) -> usize {
        self.0[0].len()
    }

    /// Each lane exactly `frames` long, zeroed. Allocates: control thread.
    fn resize(&mut self, frames: usize) {
        for lane in self.0.iter_mut() {
            fit(lane, frames, 0.0);
        }
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
        f.debug_struct("Lanes")
            .field("frames", &self.frames())
            .finish_non_exhaustive()
    }
}

/// What a voice's block read renders through: the lanes it hands on, the
/// lanes its stretch filter reads from, and the block's positions and taps.
/// One per rendering node (a pool reads every slot through its one), sized
/// on the control thread from the node's `Prepare::max_block` (about 160
/// bytes a frame at the sampler's 8 channels: 80 KiB at 512 frames), so a
/// voice's block read touches no stack arrays of its own and renders a whole
/// block in one piece.
#[derive(Debug)]
pub(crate) struct BlockScratch {
    /// The voice's output for the block, before the mix.
    pub(crate) out: Lanes,
    /// The source's frames, when a stretch filter sits between them and
    /// `out`.
    pub(crate) raw: Lanes,
    /// Where each frame of the block reads (`None`: silence).
    pub(crate) positions: Vec<Option<SamplePosition>>,
    /// The disk tier's ring positions: the placed ones less its preroll.
    pub(crate) ring_positions: Vec<Option<f64>>,
    /// The memory tier's along-time read (`MemorySource::read_placed_lanes`).
    pub(crate) gather: Gather,
}

impl BlockScratch {
    /// Zeroed scratch for blocks of up to `frames` frames (at least one).
    /// Allocates: control thread.
    pub(crate) fn new(frames: usize) -> Self {
        let mut s = Self {
            out: Lanes::new(0),
            raw: Lanes::new(0),
            positions: Vec::new(),
            ring_positions: Vec::new(),
            gather: Gather {
                taps: Vec::new(),
                frac: Vec::new(),
                live: Vec::new(),
                y: std::array::from_fn(|_| Vec::new()),
            },
        };
        s.resize(frames);
        s
    }

    /// Scratch built before `prepare`: [`UNPREPARED_FRAMES`] long.
    pub(crate) fn unprepared() -> Self {
        Self::new(UNPREPARED_FRAMES)
    }

    /// The longest piece a block read through this scratch may be: the
    /// `max_block` it was last prepared for. A block is never longer than
    /// that (the executor's contract), so a node renders each block in one
    /// piece; a node driven by hand past it renders pieces this long.
    #[inline]
    pub(crate) fn frames(&self) -> usize {
        self.positions.len()
    }

    /// Sized for blocks of up to `frames` frames (at least one), every
    /// buffer zeroed and exactly that long: what a node's `prepare` calls
    /// with `Prepare::max_block`, so a re-prepare at a larger block grows it
    /// and one at a smaller block gives the memory back. Allocates: control
    /// thread only, never `process`.
    pub(crate) fn resize(&mut self, frames: usize) {
        let frames = frames.max(1);
        self.out.resize(frames);
        self.raw.resize(frames);
        fit(&mut self.positions, frames, None);
        fit(&mut self.ring_positions, frames, None);
        let g = &mut self.gather;
        fit(&mut g.taps, frames, [0; 4]);
        fit(&mut g.frac, frames, 0.0);
        fit(&mut g.live, frames, false);
        for lane in &mut g.y {
            fit(lane, frames, 0.0);
        }
    }
}

/// `v` exactly `frames` long (its capacity too), every element `fill`.
fn fit<T: Clone>(v: &mut Vec<T>, frames: usize, fill: T) {
    v.clear();
    v.resize(frames, fill);
    v.shrink_to_fit();
}

/// Each frame's four taps and fraction, and the taps gathered into four
/// lanes, for the cubic along time. Each as long as its scratch's frames,
/// and written in full for the frames a block reads before any is used.
#[derive(Debug)]
pub(crate) struct Gather {
    pub(crate) taps: Vec<[usize; 4]>,
    pub(crate) frac: Vec<f32>,
    /// Whether frame `i` has a sample (a position inside the file).
    pub(crate) live: Vec<bool>,
    pub(crate) y: [Lane; 4],
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
        // One 64-frame lane's worth (the length the lanes had when this was
        // written; any length shows the same arithmetic).
        const LANE_FRAMES: usize = 64;
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

    /// **A re-prepare resizes.** Scratch built at 64 frames and resized to
    /// 512 holds 512 frames in every buffer a block read writes (each lane,
    /// the positions, the gather); resized back to 64 it holds 64 again.
    /// A node's `prepare` calls `resize` with `Prepare::max_block`, so a
    /// block of the new length renders in one piece without allocating.
    ///
    /// Mutation (run): `resize` skipping `raw` → fails.
    #[test]
    fn a_resize_sizes_every_buffer() {
        let mut s = BlockScratch::new(64);
        for frames in [64usize, 512, 64, 1] {
            s.resize(frames);
            assert_eq!(s.frames(), frames);
            for lanes in [&s.out, &s.raw] {
                assert!(lanes.lanes().iter().all(|l| l.len() == frames));
            }
            assert_eq!(s.ring_positions.len(), frames);
            let g = &s.gather;
            assert_eq!(
                (g.taps.len(), g.frac.len(), g.live.len()),
                (frames, frames, frames)
            );
            assert!(g.y.iter().all(|l| l.len() == frames));
        }
        // Zero is refused to one frame: a block holds at least one.
        s.resize(0);
        assert_eq!(s.frames(), 1);
    }

    /// **Every voice node sizes its scratch from `Prepare::max_block`**, and
    /// a re-prepare resizes it: a pool, a `VoiceNode` and a `MemorySource`
    /// prepared at 512 render 512 frames in one piece, re-prepared at 1024
    /// render 1024, and at 64, 64. (The disk voice's own check is
    /// `disk_voice::tests::a_disk_voice_sizes_its_positions_from_max_block`.)
    ///
    /// Mutations (each run): `VoicePool::prepare` not resizing its scratch
    /// → the pool stays at 64 → fails; `VoiceNode::prepare` the same →
    /// fails; `MemorySource::prepare` not sizing its positions → fails.
    #[test]
    fn every_voice_node_sizes_its_scratch_from_max_block() {
        use crate::voice::{MemorySource, Voice, VoiceNode, VoicePool, VoiceSource};
        use tutti_core::{Beat, SampleRate, Samples};
        use tutti_graph::{Node, Prepare};

        let mut wave = tutti_io::Wave::new(2, 48_000.0);
        wave.push_frame(&[0.0, 0.0]);
        let wave = std::sync::Arc::new(wave);
        let source = MemorySource::placed(std::sync::Arc::clone(&wave), Beat::new(0.0), None);
        let mut pool = VoicePool::new();
        let mut node = VoiceNode::new(Voice {
            source: VoiceSource::Memory(source.clone()),
            play: Default::default(),
            channel_index: None,
        });
        let mut memory = source;
        assert_eq!(pool.scratch_frames(), UNPREPARED_FRAMES);
        assert_eq!(node.scratch_frames(), UNPREPARED_FRAMES);
        for max in [512usize, 1024, 64] {
            let p = Prepare::new(SampleRate(48_000.0), Samples(max));
            pool.prepare(&p);
            node.prepare(&p);
            memory.prepare(&p);
            assert_eq!(pool.scratch_frames(), max, "pool at {max}");
            assert_eq!(node.scratch_frames(), max, "voice node at {max}");
            assert_eq!(memory.scratch_frames(), max, "memory source at {max}");
        }
    }
}
