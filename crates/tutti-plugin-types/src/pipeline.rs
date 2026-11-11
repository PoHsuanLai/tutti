//! The audio path's block pipeline: the chunk ceiling both ends of the process
//! edge size to, and what the path counts of the blocks it sent.

/// The largest chunk that crosses the process edge: the largest device
/// callback a live plugin keeps in step with, and so the ceiling on the
/// pipeline's declared latency (85 ms at 48 kHz).
///
/// The shared-memory slab is sized to this at launch (`subprocess::launch`,
/// the other consumer of this constant; or to the host's smaller
/// `max_buffer_size`), before the node is prepared — so `tutti-plugin`'s batcher (`Batcher::prepare`)
/// can only narrow the chunk, never widen it, and the slab and the batcher
/// agree on the per-chunk ceiling by construction. A device calling back
/// with more frames than this gets a chunk of this many, which no longer
/// lines up with its callbacks (the batcher's module docs).
pub const MAX_CHUNK: usize = 4096;

/// What the audio path has made of the blocks it sent a plugin, ever
/// (`HostParams::block_counts` in `tutti-plugin`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockCounts {
    /// Blocks it gave up waiting for.
    pub abandoned: u64,
    /// Replies it took, on time or owed.
    pub replies: u64,
}
