//! Butler thread configuration.

/// Ring, cache and crossfade sizing for the butler thread.
///
/// `Default` is tuned for 64-channel streaming on a typical desktop; use
/// `Default` + struct-update to change one field.
#[derive(Debug, Clone, Copy)]
pub struct BufferConfig {
    /// Baseline refill size in **frames**, before the varifill strategy scales
    /// it by buffer urgency, disk throughput and playback speed. 16384 by
    /// default, chosen to align a stereo `f32` read on 16 KB.
    pub chunk_size: usize,
    /// Ceiling on resident whole-file waves in the LRU cache. 64 by default.
    /// Pinned entries (those backing a live stream) are never evicted, so this
    /// is a target rather than a hard bound.
    pub cache_max_entries: usize,
    /// Ceiling on the LRU cache's total resident bytes. 1 GiB by default. An
    /// oversized wave is still admitted when nothing evictable remains, so this
    /// too is a target rather than a hard bound.
    pub cache_max_bytes: u64,
    /// Crossfade length in **frames** applied when the butler repositions a live
    /// stream (timeline seek or a PDC preroll change). 512 by default, about
    /// 12 ms at 44.1 kHz — long enough to hide the discontinuity, short enough
    /// not to smear the seek target.
    pub seek_crossfade_frames: usize,
    /// When true, refills for three or more concurrent streams run on rayon,
    /// each worker holding an exclusive `&mut` to a different region ring.
    pub parallel_io: bool,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            chunk_size: 16384,
            cache_max_entries: 64,
            cache_max_bytes: 1024 * 1024 * 1024, // 1GB
            seek_crossfade_frames: 512,
            parallel_io: true,
        }
    }
}
