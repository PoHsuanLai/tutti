//! High-level facade for the sampler crate. See [`Sampler`].

#[allow(unused_imports)] // used in doc links
use super::builders::CaptureSession;
use super::builders::RecordBuilder;
use super::channel::Channel;
use super::metrics::View;
use crate::butler::{
    BufferConfig, ButlerCommand, ButlerThread, CaptureIdGen, LruCache, TransportBridge,
};
use crate::error::Result;
use arc_swap::ArcSwap;
use smol::channel::Sender;
use dashmap::DashMap;
use std::sync::Arc;
use tutti_core::{PdcState, TransportManager};

/// The sampler subsystem facade.
///
/// Owns the butler thread (which drives all disk I/O) along with the
/// recording, automation, and audio-input managers. Build one at startup
/// with [`builder`](Self::builder) and keep it alive for the lifetime of
/// the audio engine.
///
/// # API shape
///
/// `Sampler` is intentionally narrow. Most operations are reached
/// through one of three handle accessors:
///
/// - [`channel(n)`](Self::channel) → [`Channel`] for playback ops on
///   a single channel
/// - [`record(path)`](Self::record) → [`RecordBuilder`] which yields a
///   [`CaptureSession`] when started
/// - [`metrics()`](Self::metrics) → [`View`] for I/O counters,
///   cache stats, and per-channel buffer health
///
/// Plus four subsystem getters — [`recording`](Self::recording),
/// [`automation`](Self::automation), [`audio_input`](Self::audio_input), and
/// [`auditioner`](Self::auditioner) — for cases the high-level API doesn't
/// cover.
///
/// # Example
///
/// ```no_run
/// use tutti_sampler::Sampler;
///
/// # fn main() -> tutti_sampler::Result<()> {
/// let sampler = Sampler::builder(48_000.0).build()?;
///
/// sampler.channel(0).play("clip.wav").start();
/// sampler.channel(0).speed(1.25);
///
/// let session = sampler.record("out.wav").channels(2).start();
/// session.stop();
/// # Ok(())
/// # }
/// ```
pub struct Sampler {
    butler_tx: Sender<ButlerCommand>,
    butler: ButlerThread,
    recording: Arc<crate::capture_impl::manager::Recorder>,
    automation: Arc<crate::capture_impl::automation_manager::Manager<crate::capture_impl::automation_target::AutomationTarget>>,
    audio_input: Arc<crate::input_impl::manager::Manager>,
    sample_rate: f64,
    transport_bridge: Option<TransportBridge>,
    capture_ids: CaptureIdGen,
}

impl Sampler {
    /// Start a builder for a new system. Call [`SamplerBuilder::build`]
    /// to spawn the butler thread and obtain a `Sampler`.
    pub fn builder(sample_rate: f64) -> SamplerBuilder {
        SamplerBuilder {
            sample_rate,
            buffer_config: BufferConfig::default(),
            pdc: None,
        }
    }

    /// Sample rate the system was built with.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Pause the butler thread's processing loop.
    ///
    /// Fire-and-forget: dropped if the butler command queue is full.
    /// Safe to call from any thread, including the audio thread.
    pub fn pause(&self) {
        let _ = self.butler_tx.try_send(ButlerCommand::Pause);
    }

    /// Resume butler processing after a [`pause`](Self::pause).
    ///
    /// **Not needed on a fresh system** — the butler is already running
    /// after [`build`](SamplerBuilder::build). Only call this to
    /// undo a previous `pause()`.
    ///
    /// Fire-and-forget: dropped if the butler command queue is full.
    /// Safe to call from any thread, including the audio thread.
    pub fn resume(&self) {
        let _ = self.butler_tx.try_send(ButlerCommand::Run);
    }

    /// Block until the butler has drained its command queue.
    ///
    /// Call from a UI or worker thread only — this can block briefly.
    pub fn wait_for_completion(&self) {
        self.send(ButlerCommand::WaitForCompletion);
    }

    /// Send a shutdown command to the butler.
    ///
    /// The thread also stops automatically when the system is dropped, so
    /// this is only needed if you want explicit ordering.
    pub fn shutdown(&self) {
        self.send(ButlerCommand::Shutdown);
    }

    /// Get a per-channel handle for playback operations.
    ///
    /// See [`Channel`] for the available methods (`play`, `stop`,
    /// `seek`, `speed`, looping, …).
    ///
    /// ```no_run
    /// # use tutti_sampler::Sampler;
    /// # let sampler = Sampler::builder(48_000.0).build().unwrap();
    /// sampler.channel(0).play("clip.wav").start();
    /// sampler.channel(0).speed(0.5);
    /// sampler.channel(0).stop();
    /// ```
    pub fn channel(&self, index: usize) -> Channel<'_> {
        Channel::new(self, index)
    }

    /// Begin a capture-session configuration.
    ///
    /// See [`RecordBuilder`] for tunable parameters; call
    /// [`RecordBuilder::start`] to obtain a live [`CaptureSession`].
    pub fn record(&self, file_path: impl Into<std::path::PathBuf>) -> RecordBuilder<'_> {
        RecordBuilder::new(self, file_path)
    }

    /// Diagnostics view: I/O counters, cache stats, per-channel buffer fill,
    /// underrun counts. See [`View`].
    pub fn metrics(&self) -> View<'_> {
        View::new(self)
    }

    /// Bind the sampler to a [`TransportManager`].
    ///
    /// While bound, the internal transport bridge polls transport state and
    /// translates play / stop / locate / loop into butler commands that are
    /// broadcast to all active streaming channels. Call
    /// [`unbind_transport`](Self::unbind_transport) to detach.
    pub fn bind_transport(&mut self, transport: Arc<TransportManager>) {
        self.transport_bridge = Some(TransportBridge::new(
            transport,
            self.butler_tx.clone(),
            self.butler.plans(),
            self.sample_rate,
        ));
    }

    /// Detach from any previously-bound transport.
    pub fn unbind_transport(&mut self) {
        self.transport_bridge = None;
    }

    /// Recording-session bookkeeper for MIDI / audio / automation captures.
    pub fn recording(&self) -> &crate::capture_impl::manager::Recorder {
        &self.recording
    }

    /// Automation-lane manager for write/touch/latch parameter recording.
    pub fn automation(&self) -> &crate::capture_impl::automation_manager::Manager<crate::capture_impl::automation_target::AutomationTarget> {
        &self.automation
    }

    /// Hardware audio-input manager (cpal capture stream + MPMC channel).
    pub fn audio_input(&self) -> &crate::input_impl::manager::Manager {
        &self.audio_input
    }

    /// Build a low-latency [`Auditioner`](super::auditioner::Auditioner)
    /// for previewing files.
    ///
    /// The auditioner uses a reserved internal channel for streaming and
    /// the LRU cache for instant replay of recently-accessed files. Only
    /// one preview can play at a time — starting a new preview stops the
    /// previous one.
    pub fn auditioner(self: &Arc<Self>) -> super::auditioner::Auditioner {
        super::auditioner::Auditioner::new(Arc::clone(self))
    }

    pub(crate) fn send(&self, cmd: ButlerCommand) {
        let _ = self.butler_tx.send_blocking(cmd);
    }

    pub(crate) fn mint_capture_id(&self) -> crate::butler::CaptureId {
        self.capture_ids.mint()
    }

    pub(crate) fn butler_plans(&self) -> Arc<DashMap<usize, crate::butler::ChannelPlan>> {
        self.butler.plans()
    }

    pub(crate) fn butler_metrics(&self) -> Arc<crate::butler::Metrics> {
        self.butler.metrics()
    }

    pub(crate) fn butler_cache(&self) -> Arc<LruCache> {
        self.butler.cache()
    }
}

// `butler` and `transport_bridge` have their own `Drop` impls; auto-drop
// handles cleanup. No `impl Drop for Sampler` needed.

/// Builder for [`Sampler`]. Returned by
/// [`Sampler::builder`].
pub struct SamplerBuilder {
    sample_rate: f64,
    buffer_config: BufferConfig,
    pdc: Option<Arc<ArcSwap<PdcState>>>,
}

impl SamplerBuilder {
    /// Override the butler's buffer / cache configuration. Defaults are
    /// tuned for 64-channel streaming on a typical desktop.
    pub fn buffer_config(mut self, config: BufferConfig) -> Self {
        self.buffer_config = config;
        self
    }

    /// Subscribe to PDC snapshots for automatic plugin-delay compensation.
    ///
    /// While set, butler pre-rolls each stream by the channel's latency
    /// so that downstream effects stay sample-aligned. The subscription is
    /// typically obtained from `TuttiGraph::pdc_snapshot()`.
    pub fn pdc(mut self, snapshot: Arc<ArcSwap<PdcState>>) -> Self {
        self.pdc = Some(snapshot);
        self
    }

    /// Build the system and spawn the butler thread.
    ///
    /// Returns [`Err`] if any subsystem fails to initialize.
    pub fn build(self) -> Result<Sampler> {
        let mut butler = ButlerThread::with_config(256, self.sample_rate, self.buffer_config);

        if let Some(ref pdc) = self.pdc {
            butler = butler.with_pdc(Arc::clone(pdc));
        }

        let butler_tx = butler.command_sender();
        butler.start();

        let capture_ids = CaptureIdGen::new();

        let recording = Arc::new(crate::capture_impl::manager::Recorder::new(
            64,
            butler_tx.clone(),
            self.sample_rate,
            capture_ids.clone(),
        ));
        let automation = Arc::new(crate::capture_impl::automation_manager::Manager::new());
        let audio_input = Arc::new(crate::input_impl::manager::Manager::new(
            self.sample_rate as u32,
        ));

        Ok(Sampler {
            butler_tx,
            butler,
            recording,
            automation,
            audio_input,
            sample_rate: self.sample_rate,
            transport_bridge: None,
            capture_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_lifecycle() {
        let sampler = Sampler::builder(44100.0).build().unwrap();
        let session = sampler.record("/tmp/export.wav").channels(2).start();
        let _ = session.id;
        session.stop();
        sampler.wait_for_completion();
    }

    #[test]
    fn test_io_metrics() {
        let sampler = Sampler::builder(44100.0).build().unwrap();
        let snapshot = sampler.metrics().io();
        assert_eq!(snapshot.bytes_read, 0);
        assert!((snapshot.cache_hit_rate() - 1.0).abs() < 0.001);
        sampler.metrics().reset_io();
    }

    #[test]
    fn test_cache_stats() {
        let sampler = Sampler::builder(44100.0).build().unwrap();
        let stats = sampler.metrics().cache();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.max_entries, 64);
    }

    #[test]
    fn test_underrun_monitoring() {
        let sampler = Sampler::builder(44100.0).build().unwrap();
        assert_eq!(sampler.metrics().take_underruns(0), 0);
        assert_eq!(sampler.metrics().take_total_underruns(), 0);
    }

    #[test]
    fn test_pdc_passthrough() {
        use tutti_core::PdcManager;
        let pdc = PdcManager::new(4, 2);
        pdc.set_channel_latency(0, 100);
        pdc.set_channel_latency(1, 200);
        let sampler = Sampler::builder(44100.0)
            .pdc(pdc.snapshot_arc())
            .build()
            .unwrap();
        // Sampler doesn't expose PDC state — caller keeps the manager.
        let _ = sampler;
        assert_eq!(pdc.max_latency(), 200);
        assert_eq!(pdc.get_channel_compensation(0), 100);
        assert_eq!(pdc.get_channel_compensation(1), 0);
    }
}
