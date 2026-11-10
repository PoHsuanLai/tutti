//! Per-stream streamer plan for a streaming playback.

use std::sync::Arc;
use tutti_types::SampleRate;

use super::cache::StreamPin;
use super::command::StreamId;
use super::control::StreamRecord;
use super::rt_state::RtState;

/// Loop playback configuration for one stream.
pub(crate) struct LoopConfig {
    /// `(start, end)` in file frames, half-open, as it was asked for.
    pub(crate) range: (u64, u64),
    /// Crossfade length in frames: as asked for, or 0 when the fade's
    /// lead-in could not be read and the ring plays the loop hard — so a fork
    /// reading the stream's record plays what the live voice does.
    pub(crate) crossfade_frames: usize,
}

/// Active streaming connection for a stream.
///
/// Groups the stream's identity and optional loop config into one value.
/// Created by `start_streaming`, dropped with its plan when the stream closes.
pub(crate) struct Link {
    pub(crate) region_id: StreamId,
    /// The loop, as the streamer runs it. Written only through
    /// [`set_loop`](Self::set_loop), which also tells the stream's
    /// [`StreamRecord`].
    loop_config: Option<LoopConfig>,
    /// The file's own rate, as its header states it. Recorded rather than
    /// recovered from the stream's [`SrcRatio`] (`file / session`): the ratio
    /// is re-derived from this when the session rate moves
    /// (`SessionRate::set`), and a placement gate converts beats to file
    /// frames with it (`StreamReader::file_rate`).
    pub(crate) file_rate: SampleRate,
    /// The file's length in frames, as the stream found it: what a loop's end
    /// is clamped to, as every tier clamps it (`LoopSpan::new`).
    pub(crate) file_frames: u64,
    /// What a fork of a voice on this stream needs to play the same file
    /// itself: the stream's own small record, shared with every voice taken
    /// from it (`StreamReader`). The region writer that also knows the
    /// path is streamer-thread-local, out of any other thread's reach.
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
    /// loop changes, so the streamer's loop and the one a fork reads cannot
    /// disagree.
    pub(crate) fn set_loop(&mut self, config: Option<LoopConfig>) {
        self.record
            .set_loop(config.as_ref().map_or(crate::voice::LoopSetting::Off, |c| {
                crate::voice::LoopSetting::On {
                    start: tutti_types::SamplePosition(c.range.0 as f64),
                    end: tutti_types::SamplePosition(c.range.1 as f64),
                    crossfade_frames: c.crossfade_frames,
                }
            }));
        self.loop_config = config;
    }
}

/// The stream is over the moment its link goes (stopped), so a voice still
/// holding its record — on a ring the streamer no longer feeds — forks to
/// silence, as it plays live.
impl Drop for Link {
    fn drop(&mut self) {
        self.record.end();
    }
}

/// One stream's streamer-thread state: its link, and the control cell the
/// voice shares (the audio thread holds an `Arc<RtState>` clone that must stay
/// alive past the plan). Held in the plan map under the stream's id, and
/// dropped from it when the stream closes.
pub struct ChannelPlan {
    pub(crate) link: Option<Link>,
    pub(crate) rt_state: Arc<RtState>,
}

impl Default for ChannelPlan {
    fn default() -> Self {
        Self {
            link: None,
            rt_state: Arc::new(RtState::new()),
        }
    }
}

impl ChannelPlan {
    /// Install the stream's link under `region_id`.
    ///
    /// `cache_pin` keeps the streamed wave resident in the LRU cache for the
    /// stream's lifetime; it is stored in the `Link` and released when
    /// the stream closes and the link drops. Pass `None` for a stream that holds no
    /// resident cache entry (incremental disk streaming). `file_rate` is the
    /// file's own rate (see [`Link::file_rate`]); `file_path` the file, and
    /// `cache` the streamer's wave cache, both for the stream's
    /// [`StreamRecord`]. `file_frames` is the file's length (see
    /// [`Link::file_frames`]).
    pub fn start_streaming(
        &mut self,
        region_id: StreamId,
        cache_pin: Option<StreamPin>,
        file_rate: SampleRate,
        file_frames: u64,
        file_path: std::path::PathBuf,
        cache: std::sync::Weak<super::cache::LruCache>,
    ) {
        self.link = Some(Link {
            region_id,
            loop_config: None,
            file_rate,
            file_frames,
            record: Arc::new(StreamRecord::new(file_path, file_rate, cache)),
            _cache_pin: cache_pin,
        });
    }

    /// Clone of the RT-shared state handle for passing to the audio thread.
    pub fn rt_state(&self) -> Arc<RtState> {
        Arc::clone(&self.rt_state)
    }
}
