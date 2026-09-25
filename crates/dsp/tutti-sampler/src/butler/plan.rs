//! Per-channel butler plan for a streaming playback.

use std::sync::Arc;
use tutti_core::{AtomicU64, Ordering, PlaybackRate, SampleRate, SrcRatio};

use super::cache::StreamPin;
use super::command::RegionId;
use super::prefetch::SharedReader;
use super::rt_state::RtState;
use crate::voice::types::Direction;

/// Where the reader stands relative to an active loop, as classified each
/// butler cycle by [`ChannelPlan::check_loop_status`].
///
/// Positions throughout are file **frames**, matching `read_position`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoopStatus {
    /// Nothing to do: not looping, or still clear of the loop end.
    Normal,
    /// Within `crossfade_frames` of the loop end — time to arm the loop
    /// crossfade, before the wrap rather than at it.
    ApproachingEnd,
    /// At or past the loop end. Carries the loop start **frame** to wrap to.
    AtEnd(u64),
}

/// Loop playback configuration for one streaming channel.
pub(crate) struct LoopConfig {
    /// `(start, end)` in file **frames**, half-open.
    pub(crate) range: (u64, u64),
    /// Crossfade length in **frames**; 0 disables the fade and wraps hard.
    pub(crate) crossfade_frames: usize,
    /// Cached fadein samples from loop start; avoids re-reading on each loop.
    /// Flat interleaved at the region ring's width.
    pub(crate) preloop_buffer: Option<Vec<f32>>,
}

/// Active streaming connection for a channel.
///
/// Groups the ring buffer consumer, region identity, and optional loop config
/// into one value. Created by `start_streaming`, dropped by `stop_streaming`.
/// `region_id` and `read_position` are cached so the butler reads them without
/// touching the [`SharedReader`]. The reader itself sits behind an `ArcSwap`
/// (not a `Mutex`): the audio thread loads it wait-free and is its sole
/// consumer; the butler only ever *replaces* it and requests ring resets via
/// `RtState`.
pub(crate) struct Link {
    pub(crate) consumer: SharedReader,
    pub(crate) region_id: RegionId,
    pub(crate) read_position: Arc<AtomicU64>,
    pub(crate) loop_config: Option<LoopConfig>,
    /// The file's own rate, as its header states it. Recorded rather than
    /// recovered from the stream's [`SrcRatio`] (`file / session`): the ratio
    /// is re-derived from this when the session rate moves
    /// (`SessionRate::set`), and a placement gate converts beats to file
    /// frames with it (`Status::take_disk_voice`).
    pub(crate) file_rate: SampleRate,
    /// Keeps the streamed wave pinned in the [`LruCache`](super::cache::LruCache)
    /// for exactly the stream's lifetime, so a fully-buffered (hence cold)
    /// stream is never evicted mid-read. `None` when the region streams
    /// incrementally from disk and holds no resident cache entry to protect.
    /// Dropped by `stop_streaming` (which drops the whole `Link`), releasing the
    /// pin.
    pub(crate) _cache_pin: Option<StreamPin>,
}

/// Per-channel butler-thread state.
///
/// `Idle` when no file is streaming, `Streaming` when active. The enum makes
/// it impossible to have a loop config without an active link.
///
/// `pdc_preroll` and `rt_state` live outside the enum because they persist
/// across idle/streaming transitions (the audio thread holds an `Arc<RtState>`
/// clone that must stay alive).
pub struct ChannelPlan {
    pub(crate) link: Option<Link>,
    /// How far to pre-roll this channel's read head for plugin delay
    /// compensation, in **frames** — subtracted from every target file position,
    /// so a larger preroll seeks earlier. Sourced from the published
    /// compensation table, whose entries are [`Samples`](tutti_core::Samples) at
    /// the session rate.
    pub(crate) pdc_preroll: u64,
    pub(crate) rt_state: Arc<RtState>,
}

impl Default for ChannelPlan {
    fn default() -> Self {
        Self {
            link: None,
            pdc_preroll: 0,
            rt_state: Arc::new(RtState::new()),
        }
    }
}

impl ChannelPlan {
    /// Attach a ring buffer consumer. Reads region_id + read_position once
    /// under one lock so the audio thread never re-locks to get them.
    ///
    /// `cache_pin` keeps the streamed wave resident in the LRU cache for the
    /// stream's lifetime; it is stored in the `Link` and released when
    /// `stop_streaming` drops the link. Pass `None` for a stream that holds no
    /// resident cache entry (incremental disk streaming). `file_rate` is the
    /// file's own rate (see [`Link::file_rate`]).
    pub fn start_streaming(
        &mut self,
        consumer: SharedReader,
        cache_pin: Option<StreamPin>,
        file_rate: SampleRate,
    ) {
        let (region_id, read_position) = {
            let cell = consumer.load();
            (cell.region_id(), cell.read_position_shared())
        };
        self.link = Some(Link {
            consumer,
            region_id,
            read_position,
            loop_config: None,
            file_rate,
            _cache_pin: cache_pin,
        });
    }

    /// Fully reset the channel: drop the link (and its loop config), pdc
    /// preroll, and reset `rt_state` to defaults. The `rt_state` Arc itself
    /// stays alive because the audio thread may still hold a clone.
    pub fn stop_streaming(&mut self) {
        self.link = None;
        self.pdc_preroll = 0;
        self.rt_state.set_speed(PlaybackRate::UNITY);
        self.rt_state.set_direction(Direction::Forward);
        self.rt_state.set_seeking(false);
        self.rt_state.set_src_ratio(SrcRatio::UNITY);
        self.rt_state.clear_loop_crossfade();
    }

    /// Clone of the RT-shared state handle for passing to the audio thread.
    pub fn rt_state(&self) -> Arc<RtState> {
        Arc::clone(&self.rt_state)
    }

    /// Request the audio thread drop the ring's stale contents (after the butler
    /// repositions the stream on a seek / loop-wrap).
    ///
    /// The butler must not pop the SPSC consumer itself — that would race the
    /// audio thread. Instead it bumps a lock-free reset epoch in `RtState`; the
    /// audio thread, which owns the ring, clears it when it next observes the
    /// bump. Safe to call even when idle (no active link): the epoch simply has
    /// no consumer to act on it.
    pub fn flush_buffer(&self) {
        self.rt_state.request_ring_reset();
    }

    /// The active loop config, if streaming and looping.
    pub(crate) fn loop_config(&self) -> Option<&LoopConfig> {
        self.link.as_ref()?.loop_config.as_ref()
    }

    /// Classify the current read position relative to the loop.
    pub fn check_loop_status(&self) -> LoopStatus {
        let Some(link) = self.link.as_ref() else {
            return LoopStatus::Normal;
        };
        let Some(loop_cfg) = link.loop_config.as_ref() else {
            return LoopStatus::Normal;
        };
        let (loop_start, loop_end) = loop_cfg.range;

        let read_pos = link.read_position.load(Ordering::Relaxed);

        if read_pos >= loop_end {
            return LoopStatus::AtEnd(loop_start);
        }

        if loop_cfg.crossfade_frames > 0 {
            let crossfade_start = loop_end.saturating_sub(loop_cfg.crossfade_frames as u64);
            if read_pos >= crossfade_start {
                return LoopStatus::ApproachingEnd;
            }
        }

        LoopStatus::Normal
    }

    /// Bracket a reposition, so the audio thread mutes rather than rendering a
    /// stream whose read head is moving.
    ///
    /// Takes `&self`: the flag lives in the `Arc`-shared `RtState`, which has
    /// interior mutability, so no exclusive borrow of the plan is needed.
    pub fn set_seeking(&self, seeking: bool) {
        self.rt_state.set_seeking(seeking);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_status_normal() {
        let state = ChannelPlan::default();
        assert_eq!(state.check_loop_status(), LoopStatus::Normal);
    }

    #[test]
    fn test_stop_streaming_resets_everything() {
        let mut state = ChannelPlan {
            pdc_preroll: 1000,
            ..Default::default()
        };
        state.rt_state.set_speed(PlaybackRate::new(2.0));
        state.rt_state.set_reverse(true);

        state.stop_streaming();

        assert_eq!(state.pdc_preroll, 0);
        assert!(state.link.is_none());
        assert!(state.loop_config().is_none());
        assert_eq!(state.rt_state.speed(), PlaybackRate::UNITY);
        assert!(!state.rt_state.is_reverse());
    }
}
