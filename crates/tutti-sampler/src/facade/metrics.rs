//! Diagnostics accessor. See [`View`].

use super::system::Sampler;
use crate::butler::{Snapshot, Stats};

/// Read-only view of sampler diagnostics.
///
/// Obtained from [`Sampler::metrics`](super::system::Sampler::metrics).
/// Cheap to construct (`Copy`) — call it on demand rather than caching the
/// handle.
///
/// # Example
///
/// ```no_run
/// # use tutti_sampler::Sampler;
/// # let sampler = Sampler::builder(48_000.0).build().unwrap();
/// let m = sampler.metrics();
/// let io = m.io();
/// println!("{} bytes read, hit rate {:.1}%", io.bytes_read, io.cache_hit_rate() * 100.0);
///
/// if let Some(fill) = m.buffer_fill(0) {
///     println!("channel 0 buffer is {:.0}% full", fill * 100.0);
/// }
/// ```
#[derive(Copy, Clone)]
pub struct View<'a> {
    sampler: &'a Sampler,
}

impl<'a> View<'a> {
    pub(crate) fn new(sampler: &'a Sampler) -> Self {
        Self { sampler }
    }

    /// Snapshot of bytes/op counters and cache hit rate.
    ///
    /// Counters are cumulative since the last [`reset_io`](Self::reset_io)
    /// (or since the system was built). The throughput field is a live
    /// signal computed over a rolling window and is *not* cleared by reset.
    pub fn io(&self) -> Snapshot {
        self.sampler.butler_metrics().snapshot()
    }

    /// Reset the cumulative I/O counters to zero.
    ///
    /// The throughput tracker (in [`io`](Self::io)`.read_rate`) is left
    /// alone — it's a sliding-window live signal, not a counter.
    pub fn reset_io(&self) {
        self.sampler.butler_metrics().reset();
    }

    /// LRU file-cache statistics (entries, bytes, configured limits).
    pub fn cache(&self) -> Stats {
        self.sampler.butler_cache().stats()
    }

    /// Streaming-buffer fullness for one channel, in `0.0..=1.0`.
    ///
    /// Values near `0.0` indicate impending underrun. Returns `None` if
    /// the channel isn't currently streaming.
    pub fn buffer_fill(&self, channel_index: usize) -> Option<f32> {
        self.sampler
            .butler_plans()
            .get(&channel_index)
            .map(|s| s.rt_state().buffer_fill())
    }

    /// Underruns observed on one channel since the previous call,
    /// resetting the per-channel counter.
    ///
    /// The `take_` prefix signals consumption: a subsequent call with no
    /// new underruns returns `0`.
    pub fn take_underruns(&self, channel_index: usize) -> u64 {
        self.sampler
            .butler_plans()
            .get(&channel_index)
            .map_or(0, |s| s.rt_state().take_underruns())
    }

    /// Sum of [`take_underruns`](Self::take_underruns) across every active
    /// channel, also consuming each per-channel counter.
    pub fn take_total_underruns(&self) -> u64 {
        self.sampler
            .butler_plans()
            .iter()
            .map(|entry| entry.value().rt_state().take_underruns())
            .sum()
    }
}
