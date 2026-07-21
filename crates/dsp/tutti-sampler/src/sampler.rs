//! The sampler streaming-engine handle. See [`Sampler`].

use crate::butler::{
    BufferConfig, ButlerCommand, ButlerThread, CaptureIdGen, LruCache, ChannelPlan,
};
use crate::error::Result;
use crate::{StreamingClipReader, StreamingSamplerUnit};
use arc_swap::ArcSwap;
#[cfg(feature = "bevy")]
use bevy_ecs::resource::Resource;
use dashmap::DashMap;
use smol::channel::Sender;
use std::path::PathBuf;
use std::sync::Arc;
use tutti_core::{BeatDuration, BeatPosition, PdcState, TransportReader};

/// The sampler subsystem handle, held as a Bevy [`Resource`].
///
/// Owns the butler thread (which drives all disk I/O) along with the
/// recording and audio-input managers. The engine builds one at startup with
/// [`new`](Self::new) and inserts it directly; the ECS layer reads it as
/// `Res<Sampler>` and reaches the subsystems through
/// [`recording`](Self::recording) / [`audio_input`](Self::audio_input), or
/// builds an [`Auditioner`](crate::Auditioner) via [`auditioner`](Self::auditioner).
///
/// Playback, recording, and preview are driven the idiomatic Bevy way — spawn
/// a [`PlayAudio`](crate::PlayAudio) entity, write a
/// [`StartRecording`](crate::StartRecording) message, or a
/// [`PreviewFile`](crate::PreviewFile) message — not through this handle.
///
/// # Example
///
/// ```no_run
/// use tutti_sampler::Sampler;
///
/// # fn main() -> tutti_sampler::Result<()> {
/// let sampler = Sampler::new(48_000.0, Default::default())?;
/// let _ = sampler.sample_rate();
/// # Ok(())
/// # }
/// ```
#[cfg_attr(feature = "bevy", derive(Resource))]
pub struct Sampler {
    butler_tx: Sender<ButlerCommand>,
    butler: ButlerThread,
    recording: Arc<crate::recording::capture::manager::Recorder>,
    audio_input: Arc<crate::input::manager::Manager>,
    sample_rate: f64,
}

impl Sampler {
    /// Build the system and spawn the butler thread.
    ///
    /// Configure with [`SamplerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Returns [`Err`] if any
    /// subsystem fails to initialize.
    pub fn new(sample_rate: f64, config: SamplerConfig) -> Result<Self> {
        let mut butler = ButlerThread::with_config(256, sample_rate, config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            butler = butler.with_pdc(Arc::clone(pdc));
        }

        let butler_tx = butler.command_sender();
        butler.start();

        let capture_ids = CaptureIdGen::new();
        let recording = Arc::new(crate::recording::capture::manager::Recorder::new(
            64,
            butler_tx.clone(),
            sample_rate,
            capture_ids,
        ));
        let audio_input = Arc::new(crate::input::manager::Manager::new(sample_rate as u32));

        Ok(Sampler {
            butler_tx,
            butler,
            recording,
            audio_input,
            sample_rate,
        })
    }

    /// Sample rate the system was built with.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Recording-session bookkeeper for MIDI / audio / automation captures.
    pub fn recording(&self) -> &crate::recording::capture::manager::Recorder {
        &self.recording
    }

    /// Hardware audio-input manager (cpal capture stream + MPMC channel).
    pub fn audio_input(&self) -> &crate::input::manager::Manager {
        &self.audio_input
    }

    /// Register a disk-streaming source for a timeline clip on `channel_index`.
    ///
    /// Sends the butler a [`StreamAudioFile`](ButlerCommand::StreamAudioFile)
    /// command; the butler probes the file, allocates a ring, and installs the
    /// [`ChannelPlan`] link asynchronously. Pair with
    /// [`take_clip_reader`](Self::take_clip_reader) — called on a later frame —
    /// to build the [`StreamingClipReader`] once the link exists.
    ///
    /// `channel_index` must be unique per streaming clip; the caller (dawai-model)
    /// derives it from the clip's `SlotId`.
    pub fn stream_clip(&self, channel_index: usize, file_path: PathBuf, offset_samples: usize) {
        let _ = self.butler_tx.send_blocking(ButlerCommand::StreamAudioFile {
            channel_index,
            file_path,
            offset_samples,
        });
    }

    /// Build a [`StreamingClipReader`] for a channel whose butler stream is
    /// ready, binding it to the timeline placement gate.
    ///
    /// Pulls the ring consumer + shared `RtState` out of the channel's
    /// [`ChannelPlan`] link — the same handles [`Auditioner::streaming_unit`]
    /// wires — and wraps them in a placement-gated reader. Returns `None` while
    /// the butler hasn't installed the link yet (the caller retries next frame).
    ///
    /// [`Auditioner::streaming_unit`]: crate::Auditioner::streaming_unit
    pub fn take_clip_reader(
        &self,
        channel_index: usize,
        transport: Arc<dyn TransportReader>,
        start_beat: BeatPosition,
        duration: Option<BeatDuration>,
    ) -> Option<StreamingClipReader> {
        let plans = self.butler_plans();
        let plan = plans.get(&channel_index)?;
        let link = plan.link.as_ref()?;
        let consumer = link.consumer.clone();
        let rt_state = plan.rt_state();

        // file_sr / session_sr is the src_ratio the butler set on the plan; the
        // reader's placement gate converts transport seconds → file samples with
        // the file's own rate, so recover it from that ratio.
        let file_sample_rate = self.sample_rate * rt_state.src_ratio().get() as f64;

        let inner = StreamingSamplerUnit::new(consumer, Arc::clone(&rt_state));
        Some(StreamingClipReader::new(
            inner,
            rt_state,
            transport,
            start_beat,
            duration,
            file_sample_rate,
        ))
    }

    /// Enable/replace looping on a streaming clip's channel.
    ///
    /// `range` is `(loop_start, loop_end)` in file samples. Forwards
    /// [`SetStreamLoop`](ButlerCommand::SetStreamLoop) to the butler, which
    /// builds the loop config the refill/wrap loop respects.
    pub fn set_clip_stream_loop(
        &self,
        channel_index: usize,
        range: (u64, u64),
        crossfade_samples: usize,
    ) {
        let _ = self.butler_tx.send_blocking(ButlerCommand::SetStreamLoop {
            channel_index,
            range,
            crossfade_samples,
        });
    }

    /// Disable looping on a streaming clip's channel.
    pub fn clear_clip_stream_loop(&self, channel_index: usize) {
        let _ = self
            .butler_tx
            .send_blocking(ButlerCommand::ClearStreamLoop { channel_index });
    }

    /// Reposition a streaming clip's channel to an absolute file sample offset
    /// (timeline seek). Forwards [`SeekStream`](ButlerCommand::SeekStream); the
    /// butler applies the channel's PDC preroll and repositions the live stream
    /// click-free (flush + seek + crossfade).
    pub fn seek_clip_stream(&self, channel_index: usize, file_position: u64) {
        let _ = self.butler_tx.send_blocking(ButlerCommand::SeekStream {
            channel_index,
            file_position,
        });
    }

    /// Set varispeed (playback speed + direction) on a streaming clip's channel.
    /// `speed = 1.0` is normal; `reverse` flips playback direction. Forwards
    /// [`SetVarispeed`](ButlerCommand::SetVarispeed).
    pub fn set_clip_stream_speed(&self, channel_index: usize, speed: f32, reverse: bool) {
        let direction = if reverse {
            crate::butler::PlayDirection::Reverse
        } else {
            crate::butler::PlayDirection::Forward
        };
        let _ = self.butler_tx.send_blocking(ButlerCommand::SetVarispeed {
            channel_index,
            direction,
            speed,
        });
    }

    /// Stop a streaming clip's channel — drops its ring + link.
    pub fn stop_clip_stream(&self, channel_index: usize) {
        let _ = self
            .butler_tx
            .send_blocking(ButlerCommand::StopStreaming { channel_index });
    }

    /// Build a low-latency [`Auditioner`](crate::Auditioner) for previewing
    /// files. The auditioner uses a reserved internal channel for streaming
    /// and the LRU cache for instant replay of recently-accessed files.
    pub fn auditioner(&self) -> crate::Auditioner {
        crate::Auditioner::new(self)
    }

    /// Clone of the butler command channel, for handles (auditioner) that
    /// drive the butler without a back-reference to the whole `Sampler`.
    pub(crate) fn butler_sender(&self) -> Sender<ButlerCommand> {
        self.butler_tx.clone()
    }

    pub(crate) fn butler_plans(&self) -> Arc<DashMap<usize, ChannelPlan>> {
        self.butler.plans()
    }

    pub(crate) fn butler_cache(&self) -> Arc<LruCache> {
        self.butler.cache()
    }
}

// `butler` has its own `Drop` impl; auto-drop handles cleanup.

/// Configuration for [`Sampler::new`]. `Default` + struct-update.
#[derive(Default)]
pub struct SamplerConfig {
    /// Butler buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
    /// PDC snapshot subscription for automatic plugin-delay compensation.
    ///
    /// While set, butler pre-rolls each stream by the channel's latency so
    /// downstream effects stay sample-aligned. Typically obtained from
    /// `AudioGraph::pdc_snapshot()`.
    pub pdc: Option<Arc<ArcSwap<PdcState>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_metrics_zeroed_on_fresh_system() {
        let sampler = Sampler::new(44100.0, Default::default()).unwrap();
        // A fresh butler has read nothing and an empty cache.
        let plans = sampler.butler_plans();
        assert!(plans.is_empty());
        assert_eq!(sampler.sample_rate(), 44100.0);
    }

    #[test]
    fn test_pdc_passthrough() {
        use tutti_core::PdcManager;
        let pdc = PdcManager::new(4, 2);
        pdc.set_channel_latency(0, 100);
        pdc.set_channel_latency(1, 200);
        let sampler = Sampler::new(
            44100.0,
            SamplerConfig {
                pdc: Some(pdc.snapshot_arc()),
                ..Default::default()
            },
        )
        .unwrap();
        // Sampler doesn't expose PDC state — caller keeps the manager.
        let _ = sampler;
        assert_eq!(pdc.max_latency(), 200);
        assert_eq!(pdc.get_channel_compensation(0), 100);
        assert_eq!(pdc.get_channel_compensation(1), 0);
    }
}
