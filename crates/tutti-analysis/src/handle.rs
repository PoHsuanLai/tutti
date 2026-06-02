use crate::live::LiveAnalysisState;
use crate::ThumbnailCache;
use crate::{
    CorrelationMeter, DetectionMethod, PitchDetector, PitchResult, SpectrumResult, StereoAnalysis,
    StereoWaveformSummary, Transient, TransientDetector, WaveformSummary,
};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tutti_core::metering::MeteringManager;

/// Live analysis lifecycle owned by [`AnalysisHandle`].
///
/// Held inside a `Mutex` so enable/disable can swap the running state
/// atomically. `state` is published outside the mutex for lock-free reads.
struct LiveSlot {
    state: Arc<LiveAnalysisState>,
    thread: Option<JoinHandle<()>>,
}

/// Read-side handle into the analysis subsystem.
///
/// Constructed with [`AnalysisHandle::with_metering`] so that
/// [`AnalysisHandle::enable_live`] can wire the metering tap and spawn the
/// analysis thread on demand. Without the metering manager (constructed via
/// [`AnalysisHandle::new`]) only the offline helpers (`detect_*`,
/// `waveform_summary`, …) are usable.
pub struct AnalysisHandle {
    sample_rate: f64,
    metering: Option<Arc<MeteringManager>>,
    /// Latest published live state. `None` when disabled. Cheap to clone for
    /// `live_*()` reads — guarded by a short `Mutex` lock that never blocks
    /// the audio thread.
    live: Mutex<Option<Arc<LiveAnalysisState>>>,
    /// Running thread metadata, separate from `live` so readers don't touch
    /// the join handle. `take()`-and-join on disable.
    slot: Mutex<Option<LiveSlot>>,
    thumbnail_cache: Arc<Mutex<ThumbnailCache>>,
}

impl AnalysisHandle {
    /// Construct without a metering manager. `enable_live` will fail to wire
    /// the tap; offline helpers still work.
    pub fn new(sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        Self {
            sample_rate: sample_rate.into().get(),
            metering: None,
            live: Mutex::new(None),
            slot: Mutex::new(None),
            thumbnail_cache: Arc::new(Mutex::new(ThumbnailCache::new(1024))),
        }
    }

    /// Construct with a metering manager so [`enable_live`] can wire the tap.
    pub fn with_metering(
        sample_rate: impl Into<tutti_core::SampleRate>,
        metering: Arc<MeteringManager>,
    ) -> Self {
        Self {
            sample_rate: sample_rate.into().get(),
            metering: Some(metering),
            live: Mutex::new(None),
            slot: Mutex::new(None),
            thumbnail_cache: Arc::new(Mutex::new(ThumbnailCache::new(1024))),
        }
    }

    /// Enable live analysis: enable the metering tap and spawn the analysis
    /// thread. Idempotent. Returns false if no metering manager is wired.
    pub fn enable_live(&self) -> bool {
        let Some(metering) = self.metering.as_ref() else {
            return false;
        };
        let mut slot_guard = self.slot.lock().unwrap();
        if slot_guard.is_some() {
            return true;
        }

        let consumer = metering.enable_tap();
        let state = Arc::new(LiveAnalysisState::new(512));
        let thread_state = state.clone();
        let sample_rate = self.sample_rate;
        let thread = std::thread::Builder::new()
            .name("tutti-analysis".into())
            .spawn(move || {
                crate::live::run_analysis_thread(consumer, thread_state, sample_rate);
            })
            .expect("failed to spawn tutti-analysis thread");

        *self.live.lock().unwrap() = Some(state.clone());
        *slot_guard = Some(LiveSlot {
            state,
            thread: Some(thread),
        });
        true
    }

    /// Disable live analysis: stop the thread, disable the tap. Idempotent.
    pub fn disable_live(&self) {
        let mut slot_guard = self.slot.lock().unwrap();
        let Some(mut slot) = slot_guard.take() else {
            return;
        };
        slot.state.stop();
        if let Some(metering) = self.metering.as_ref() {
            metering.disable_tap();
        }
        // Drop the published Arc before joining so readers releasing their
        // clones drop the state promptly.
        *self.live.lock().unwrap() = None;
        if let Some(handle) = slot.thread.take() {
            let _ = handle.join();
        }
    }

    pub fn is_live(&self) -> bool {
        self.live.lock().unwrap().is_some()
    }

    fn live_state(&self) -> Option<Arc<LiveAnalysisState>> {
        self.live.lock().unwrap().clone()
    }

    pub fn live_pitch(&self) -> Arc<PitchResult> {
        self.live_state().map_or_else(
            || Arc::new(PitchResult::default()),
            |state| state.pitch.load_full(),
        )
    }

    pub fn live_transients(&self) -> Arc<Vec<Transient>> {
        self.live_state().map_or_else(
            || Arc::new(Vec::new()),
            |state| state.transients.load_full(),
        )
    }

    /// Returns the last ~2 seconds of waveform blocks.
    pub fn live_waveform(&self) -> Arc<WaveformSummary> {
        self.live_state().map_or_else(
            || Arc::new(WaveformSummary::new(512)),
            |state| state.waveform.load_full(),
        )
    }

    /// Returns the most recent FFT magnitude spectrum.
    pub fn live_spectrum(&self) -> Arc<SpectrumResult> {
        self.live_state().map_or_else(
            || Arc::new(SpectrumResult::default()),
            |state| state.spectrum.load_full(),
        )
    }

    pub fn detect_transients(&self, samples: &[f32]) -> Vec<Transient> {
        let mut detector = TransientDetector::new(self.sample_rate);
        detector.detect(samples)
    }

    pub fn detect_transients_with_method(
        &self,
        samples: &[f32],
        method: DetectionMethod,
    ) -> Vec<Transient> {
        let mut detector = TransientDetector::new(self.sample_rate);
        detector.set_method(method);
        detector.detect(samples)
    }

    pub fn detect_pitch(&self, samples: &[f32]) -> PitchResult {
        let mut detector = PitchDetector::new(self.sample_rate);
        detector.detect(samples)
    }

    pub fn detect_pitch_with_confidence(
        &self,
        samples: &[f32],
        min_confidence: f32,
    ) -> Option<PitchResult> {
        let result = self.detect_pitch(samples);
        if result.confidence >= min_confidence {
            Some(result)
        } else {
            None
        }
    }

    pub fn analyze_stereo(&self, left: &[f32], right: &[f32]) -> StereoAnalysis {
        let mut meter = CorrelationMeter::new(self.sample_rate);
        meter.process(left, right)
    }

    pub fn waveform_summary(&self, samples: &[f32], samples_per_block: usize) -> WaveformSummary {
        crate::waveform::compute_summary(samples, 1, samples_per_block)
    }

    /// Input: interleaved stereo `[L0, R0, L1, R1, ...]`.
    pub fn stereo_waveform_summary(
        &self,
        interleaved_samples: &[f32],
        samples_per_block: usize,
    ) -> StereoWaveformSummary {
        crate::waveform::compute_stereo_summary(interleaved_samples, samples_per_block)
    }

    /// 8 zoom levels starting at 512 samples/block; results are cached.
    pub fn cached_multi_resolution_summary(
        &self,
        audio_id: u64,
        samples: &[f32],
    ) -> crate::MultiResolutionSummary {
        let mut cache = self.thumbnail_cache.lock().unwrap();
        cache
            .get_or_compute(audio_id, || {
                crate::MultiResolutionSummary::from_samples(samples, 1, 512, 8)
            })
            .clone()
    }

    pub fn clear_cache(&self) {
        self.thumbnail_cache.lock().unwrap().clear();
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

impl Drop for AnalysisHandle {
    fn drop(&mut self) {
        self.disable_live();
    }
}
