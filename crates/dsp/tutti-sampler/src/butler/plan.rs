//! Per-channel butler plan for a streaming playback.

use std::sync::Arc;
use tutti_core::{PlaybackRate, SampleRate, SrcRatio};

use super::cache::StreamPin;
use super::command::RegionId;
use super::control::StreamRecord;
use super::prefetch::SharedReader;
use super::rt_state::RtState;
use crate::voice::types::Direction;

/// Loop playback configuration for one streaming channel.
pub(crate) struct LoopConfig {
    /// `(start, end)` in file **frames**, half-open, as it was asked for.
    pub(crate) range: (u64, u64),
    /// Crossfade length in **frames**: as asked for, or 0 when the fade's
    /// lead-in could not be read and the ring plays the loop hard — so a fork
    /// reading the stream's record plays what the live voice does.
    pub(crate) crossfade_frames: usize,
}

/// Active streaming connection for a channel.
///
/// Groups the ring buffer consumer, region identity, and optional loop config
/// into one value. Created by `start_streaming`, dropped by `stop_streaming`.
/// `region_id` is cached so the butler reads it without touching the
/// [`SharedReader`], the ring every voice taken from the stream reads by
/// position (`prefetch::Ring`).
pub(crate) struct Link {
    pub(crate) consumer: SharedReader,
    pub(crate) region_id: RegionId,
    /// The loop, as the butler runs it. Written only through
    /// [`set_loop`](Self::set_loop), which also tells the stream's
    /// [`StreamRecord`].
    loop_config: Option<LoopConfig>,
    /// The file's own rate, as its header states it. Recorded rather than
    /// recovered from the stream's [`SrcRatio`] (`file / session`): the ratio
    /// is re-derived from this when the session rate moves
    /// (`SessionRate::set`), and a placement gate converts beats to file
    /// frames with it (`Status::take_disk_voice`).
    pub(crate) file_rate: SampleRate,
    /// The file's length in frames, as the stream found it: what a loop's end
    /// is clamped to, as every tier clamps it (`LoopSpan::new`).
    pub(crate) file_frames: u64,
    /// What a fork of a voice on this stream needs to play the same file
    /// itself: the stream's own small record, shared with every voice taken
    /// from it (`take_streaming_unit`). The region writer that also knows the
    /// path is butler-thread-local, out of any other thread's reach.
    pub(crate) record: Arc<StreamRecord>,
    /// Keeps the streamed wave pinned in the [`LruCache`](super::cache::LruCache)
    /// for exactly the stream's lifetime, so a fully-buffered (hence cold)
    /// stream is never evicted mid-read. `None` when the region streams
    /// incrementally from disk and holds no resident cache entry to protect.
    /// Dropped by `stop_streaming` (which drops the whole `Link`), releasing the
    /// pin.
    pub(crate) _cache_pin: Option<StreamPin>,
}

impl Link {
    /// Set or clear the loop, and tell the stream's record: the one place a
    /// loop changes, so the butler's loop and the one a fork reads cannot
    /// disagree.
    pub(crate) fn set_loop(&mut self, config: Option<LoopConfig>) {
        self.record
            .set_loop(config.as_ref().map_or(crate::voice::LoopSetting::Off, |c| {
                crate::voice::LoopSetting::On {
                    start: tutti_core::SamplePosition(c.range.0 as f64),
                    end: tutti_core::SamplePosition(c.range.1 as f64),
                    crossfade_frames: c.crossfade_frames,
                }
            }));
        self.loop_config = config;
    }
}

/// The stream is over the moment its link goes (stopped, or replaced by a
/// new stream on the channel), so a voice still holding its record — on a
/// ring the butler no longer feeds — forks to silence, as it plays live.
impl Drop for Link {
    fn drop(&mut self) {
        self.record.end();
    }
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
    /// Attach a ring buffer consumer, caching its region id.
    ///
    /// `cache_pin` keeps the streamed wave resident in the LRU cache for the
    /// stream's lifetime; it is stored in the `Link` and released when
    /// `stop_streaming` drops the link. Pass `None` for a stream that holds no
    /// resident cache entry (incremental disk streaming). `file_rate` is the
    /// file's own rate (see [`Link::file_rate`]); `file_path` the file, and
    /// `cache` the butler's wave cache, both for the stream's
    /// [`StreamRecord`]. `file_frames` is the file's length (see
    /// [`Link::file_frames`]).
    pub fn start_streaming(
        &mut self,
        consumer: SharedReader,
        cache_pin: Option<StreamPin>,
        file_rate: SampleRate,
        file_frames: u64,
        file_path: std::path::PathBuf,
        cache: std::sync::Weak<super::cache::LruCache>,
    ) {
        let region_id = consumer.region_id();
        self.link = Some(Link {
            consumer,
            region_id,
            loop_config: None,
            file_rate,
            file_frames,
            record: Arc::new(StreamRecord::new(file_path, file_rate, cache)),
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
        self.rt_state.set_src_ratio(SrcRatio::UNITY);
    }

    /// Clone of the RT-shared state handle for passing to the audio thread.
    pub fn rt_state(&self) -> Arc<RtState> {
        Arc::clone(&self.rt_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(state.rt_state.speed(), PlaybackRate::UNITY);
        assert!(!state.rt_state.is_reverse());
    }
}
