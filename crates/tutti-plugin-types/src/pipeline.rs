//! The audio path's block pipeline: the chunk ceiling both ends of the process
//! edge size to, and what the path counts of the blocks and chunks it sent.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The largest chunk that crosses the process edge: the largest device
/// callback a live plugin keeps in step with, and so the ceiling on the
/// pipeline's declared latency (85 ms at 48 kHz).
///
/// The shared-memory slab is sized to this at launch (`subprocess::launch`,
/// the other consumer of this constant; or to the host's smaller
/// `max_buffer_size`), before the node is prepared — so `tutti-plugin-ipc`'s batcher (`Batcher::prepare`)
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

/// What the pipeline reports to its controls
/// (`PluginControls::missed_chunks` and `chunk_in_flight_answered`
/// in `tutti-plugin-host`):
/// written on the audio thread with relaxed atomics, read anywhere.
///
/// Here, beside [`BlockCounts`], because the batcher in `tutti-plugin-ipc`
/// writes it and `tutti-plugin-host`'s controls read it, and neither crate may
/// depend on the other.
#[derive(Default)]
pub struct ChunkCounters {
    /// The chunk submitted and not collected yet; 0 for none (sequences
    /// start at 1).
    pub in_flight: AtomicU64,
    /// Chunks the server had not answered when the node collected them
    /// (played as silence), the bridge alive.
    pub missed: AtomicU64,
    /// Whether the chunk of a sequence has been answered, set when the node is
    /// bound (the shared-memory slab's reply flag, which this file does not
    /// name).
    answered: OnceLock<Box<dyn Fn(u64) -> bool + Send + Sync>>,
}

impl std::fmt::Debug for ChunkCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkCounters")
            .field("in_flight", &self.in_flight)
            .field("missed", &self.missed)
            .finish_non_exhaustive()
    }
}

impl ChunkCounters {
    /// Records how to tell that a chunk is answered (the node's bind).
    pub fn attach(&self, answered: impl Fn(u64) -> bool + Send + Sync + 'static) {
        let _ = self.answered.set(Box::new(answered));
    }

    /// See `PluginControls::missed_chunks`.
    pub fn missed(&self) -> u64 {
        self.missed.load(Ordering::Relaxed)
    }

    /// See `PluginControls::chunk_in_flight_answered`.
    pub fn in_flight_answered(&self) -> bool {
        let seq = self.in_flight.load(Ordering::Acquire);
        seq == 0 || self.answered.get().is_none_or(|answered| answered(seq))
    }
}
