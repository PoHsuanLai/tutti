//! The sampler streaming-engine handle. See [`Sampler`].

use crate::butler::{BufferConfig, ButlerCommand, ButlerThread, CaptureIdGen, LruCache};
use crate::error::Result;
use crate::ports::{Commands, Status};
use arc_swap::ArcSwap;
#[cfg(feature = "bevy")]
use bevy_ecs::resource::Resource;
use smol::channel::Sender;
use std::sync::Arc;
use tutti_core::PdcState;

/// The sampler subsystem handle, held as a Bevy [`Resource`].
///
/// Owns the butler thread (which drives all disk I/O) along with the
/// recording and audio-input managers. The engine builds one at startup with
/// [`new`](Self::new) and inserts it directly; the ECS layer reads it as
/// `Res<Sampler>` and reaches the subsystems through
/// [`recording`](Self::recording) / [`audio_input`](Self::audio_input), or
/// builds an [`Auditioner`](crate::Auditioner) via [`auditioner`](Self::auditioner).
///
/// Stream control is split MIDI-device-style into two cloneable ports: the
/// WRITE port [`commands`](Self::commands) (a [`Commands`] over the butler
/// command channel) and the READ port [`status`](Self::status) (a [`Status`]
/// carrying the sample rate + the reader-factory).
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
/// let _ = sampler.status().sample_rate();
/// # Ok(())
/// # }
/// ```
#[cfg_attr(feature = "bevy", derive(Resource))]
pub struct Sampler {
    butler_tx: Sender<ButlerCommand>,
    butler: ButlerThread,
    recording: Arc<crate::recording::capture::manager::Recorder>,
    audio_input: Arc<crate::input::manager::InputEngine>,
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
        let audio_input = Arc::new(crate::input::manager::InputEngine::new(sample_rate as u32));

        Ok(Sampler {
            butler_tx,
            butler,
            recording,
            audio_input,
            sample_rate,
        })
    }

    /// WRITE port: a cloneable [`Commands`] handle over the butler command
    /// channel. Drive streaming with `commands().send(Command::…)`.
    pub fn commands(&self) -> Commands {
        Commands::new(self.butler_tx.clone(), self.butler.plans())
    }

    /// READ port: a cloneable [`Status`] snapshot carrying the sample rate and
    /// the channel-plan map (the reader-factory).
    pub fn status(&self) -> Status {
        Status::new(self.sample_rate, self.butler.plans())
    }

    /// Recording-session bookkeeper for MIDI / audio / automation captures.
    pub fn recording(&self) -> &crate::recording::capture::manager::Recorder {
        &self.recording
    }

    /// Hardware audio-input manager (cpal capture stream + MPMC channel).
    pub fn audio_input(&self) -> &crate::input::manager::InputEngine {
        &self.audio_input
    }

    /// Build a low-latency [`Auditioner`](crate::Auditioner) for previewing
    /// files. The auditioner uses a reserved internal channel for streaming
    /// and the LRU cache for instant replay of recently-accessed files.
    pub fn auditioner(&self) -> crate::Auditioner {
        crate::Auditioner::new(self)
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
        let plans = sampler.butler.plans();
        assert!(plans.is_empty());
        assert_eq!(sampler.status().sample_rate(), 44100.0);
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
