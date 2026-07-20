//! Per-channel butler plan for a streaming playback.

use parking_lot::Mutex;
use std::sync::Arc;
use tutti_core::{AtomicU64, Ordering};

use super::command::RegionId;
use super::prefetch::RegionReader;
use super::rt_state::RtState;
use crate::Direction;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoopStatus {
    Normal,
    /// Within crossfade distance of loop end.
    ApproachingEnd,
    /// At or past loop end; value is loop start position to wrap to.
    AtEnd(u64),
}

/// Loop playback configuration.
pub(crate) struct LoopConfig {
    pub(crate) range: (u64, u64),
    pub(crate) crossfade_samples: usize,
    /// Cached fadein samples from loop start; avoids re-reading on each loop.
    pub(crate) preloop_buffer: Option<Vec<(f32, f32)>>,
}

/// Active streaming connection for a channel.
///
/// Groups the ring buffer consumer, region identity, and optional loop config
/// into one value. Created by `start_streaming`, dropped by `stop_streaming`.
/// `region_id` and `read_position` are cached so the audio thread reads them
/// without locking the consumer.
pub(crate) struct Link {
    pub(crate) consumer: Arc<Mutex<RegionReader>>,
    pub(crate) region_id: RegionId,
    pub(crate) read_position: Arc<AtomicU64>,
    pub(crate) loop_config: Option<LoopConfig>,
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
    /// Samples to pre-roll for plugin delay compensation.
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
    pub fn start_streaming(&mut self, consumer: Arc<Mutex<RegionReader>>) {
        let (region_id, read_position) = {
            let guard = consumer.lock();
            (guard.region_id(), guard.read_position_shared())
        };
        self.link = Some(Link {
            consumer,
            region_id,
            read_position,
            loop_config: None,
        });
    }

    /// Fully reset the channel: drop the link (and its loop config), pdc
    /// preroll, and reset `rt_state` to defaults. The `rt_state` Arc itself
    /// stays alive because the audio thread may still hold a clone.
    pub fn stop_streaming(&mut self) {
        self.link = None;
        self.pdc_preroll = 0;
        self.rt_state.set_speed(1.0);
        self.rt_state.set_direction(Direction::Forward);
        self.rt_state.set_seeking(false);
        self.rt_state.set_src_ratio(1.0);
        self.rt_state.clear_loop_crossfade();
    }

    /// Clone of the RT-shared state handle for passing to the audio thread.
    pub fn rt_state(&self) -> Arc<RtState> {
        Arc::clone(&self.rt_state)
    }

    /// Clear ring buffer without busy-waiting. Bounded lock hold time.
    pub fn flush_buffer(&self) {
        if let Some(link) = self.link.as_ref() {
            link.consumer.lock().clear();
        }
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

        if loop_cfg.crossfade_samples > 0 {
            let crossfade_start = loop_end.saturating_sub(loop_cfg.crossfade_samples as u64);
            if read_pos >= crossfade_start {
                return LoopStatus::ApproachingEnd;
            }
        }

        LoopStatus::Normal
    }

    /// Mirrors to `rt_state` since it's behind an `Arc` with interior mutability.
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
        state.rt_state.set_speed(2.0);
        state.rt_state.set_reverse(true);

        state.stop_streaming();

        assert_eq!(state.pdc_preroll, 0);
        assert!(state.link.is_none());
        assert!(state.loop_config().is_none());
        assert_eq!(state.rt_state.speed(), tutti_core::Ratio::new(1.0));
        assert!(!state.rt_state.is_reverse());
    }
}
