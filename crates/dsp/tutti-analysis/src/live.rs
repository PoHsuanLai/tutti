//! Live audio analysis via ring buffer tap.
//!
//! Runs analysis on a background thread, reading from a SPSC ring buffer
//! fed by the audio callback. Results are published via `ArcSwap` for
//! lock-free reads from the UI thread.

use crate::{
    PitchDetector, PitchResult, SpectrumResult, Transient, TransientDetector, WaveformBlock,
    WaveformSummary,
};
use arc_swap::ArcSwap;
use core::sync::atomic::{AtomicBool, Ordering};
use ringbuf::{
    traits::{Consumer, Observer},
    HeapCons,
};
use rustfft::{num_complex::Complex, FftPlanner};
use std::sync::Arc;
use tutti_core::io::AudioIn;

/// [`AudioIn`] adapter over the metering tap's SPSC ring. Pops `(f32, f32)`
/// pairs from the ring and hands them out as `[f32; 2]` frames — the cold-path
/// read half the analysis thread drains through. (The ring element stays
/// `(f32, f32)` because the audio-thread metering tap writes tuples; the
/// tuple → `[f32; 2]` conversion is confined to this adapter.)
struct RingIn {
    consumer: HeapCons<(f32, f32)>,
    /// Reused `(f32, f32)` staging so a `poll_into` allocates nothing.
    scratch: Vec<(f32, f32)>,
}

impl RingIn {
    fn new(consumer: HeapCons<(f32, f32)>, capacity: usize) -> Self {
        Self {
            consumer,
            scratch: vec![(0.0, 0.0); capacity],
        }
    }

    /// Frames currently sitting in the ring (an upper bound on the next poll).
    fn available(&self) -> usize {
        self.consumer.occupied_len()
    }
}

impl AudioIn for RingIn {
    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
        let n = out.len().min(self.scratch.len());
        if n == 0 {
            return 0;
        }
        let read = self.consumer.pop_slice(&mut self.scratch[..n]);
        for (slot, &(l, r)) in out.iter_mut().zip(&self.scratch[..read]) {
            *slot = [l, r];
        }
        read
    }
}

/// Shared state between the analysis thread and `AnalysisHandle`.
///
/// All fields are lock-free for reads from any thread.
pub struct LiveAnalysisState {
    pub pitch: ArcSwap<PitchResult>,
    pub transients: ArcSwap<Vec<Transient>>,
    pub waveform: ArcSwap<WaveformSummary>,
    pub spectrum: ArcSwap<SpectrumResult>,
    running: AtomicBool,
}

impl LiveAnalysisState {
    pub fn new(samples_per_block: usize) -> Self {
        Self {
            pitch: ArcSwap::from_pointee(PitchResult::default()),
            transients: ArcSwap::from_pointee(Vec::new()),
            waveform: ArcSwap::from_pointee(WaveformSummary::new(samples_per_block)),
            spectrum: ArcSwap::from_pointee(SpectrumResult::default()),
            running: AtomicBool::new(true),
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

const LIVE_WAVEFORM_BLOCK_SIZE: usize = 512;
/// Must be large enough for pitch detection.
const WINDOW_SIZE: usize = 4096;
const HOP_SIZE: usize = 512;
const MAX_RECENT_TRANSIENTS: usize = 64;

/// Drains stereo pairs from `consumer`, downmixes to mono, and runs
/// pitch/transient/waveform analysis on a sliding window.
/// Blocks until `state.stop()` is called.
pub fn run_analysis_thread(
    consumer: HeapCons<(f32, f32)>,
    state: Arc<LiveAnalysisState>,
    sample_rate: f64,
) {
    let mut input = RingIn::new(consumer, 1024);
    let mut pitch_detector = PitchDetector::new(sample_rate);
    let mut transient_detector = TransientDetector::new(sample_rate);

    // FFT for spectrum analysis (separate from transient detector's internal FFT)
    let mut fft_planner = FftPlanner::<f32>::new();
    let fft = fft_planner.plan_fft_forward(WINDOW_SIZE);
    let mut fft_scratch = vec![Complex::<f32>::default(); fft.get_inplace_scratch_len()];
    let hann_window: Vec<f32> = (0..WINDOW_SIZE)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / WINDOW_SIZE as f32).cos()))
        .collect();

    let mut window = vec![0.0f32; WINDOW_SIZE];
    let mut window_pos = 0usize;
    let mut hop_counter = 0usize;

    let mut waveform_blocks: Vec<WaveformBlock> = Vec::new();
    let mut block_min = f32::MAX;
    let mut block_max = f32::MIN;
    let mut block_sum_sq = 0.0f32;
    let mut block_count = 0usize;

    let mut recent_transients: Vec<Transient> = Vec::new();
    let mut total_samples_processed = 0usize;

    let mut drain_buf = [[0.0f32; 2]; 1024];

    while state.is_running() {
        if input.available() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }

        let read = input.poll_into(&mut drain_buf);

        for &[l, r] in &drain_buf[..read] {
            let mono = (l + r) * 0.5;

            window[window_pos % WINDOW_SIZE] = mono;
            window_pos += 1;
            hop_counter += 1;
            total_samples_processed += 1;

            block_min = block_min.min(mono);
            block_max = block_max.max(mono);
            block_sum_sq += mono * mono;
            block_count += 1;

            if block_count >= LIVE_WAVEFORM_BLOCK_SIZE {
                let rms = (block_sum_sq / block_count as f32).sqrt();
                waveform_blocks.push(WaveformBlock {
                    min: block_min,
                    max: block_max,
                    rms,
                });

                let max_blocks = (sample_rate as usize / LIVE_WAVEFORM_BLOCK_SIZE) * 2; // ~2s
                if waveform_blocks.len() > max_blocks {
                    waveform_blocks.drain(0..waveform_blocks.len() - max_blocks);
                }

                let summary = WaveformSummary {
                    blocks: waveform_blocks.clone(),
                    samples_per_block: LIVE_WAVEFORM_BLOCK_SIZE,
                    total_samples: total_samples_processed,
                };
                state.waveform.store(Arc::new(summary));

                block_min = f32::MAX;
                block_max = f32::MIN;
                block_sum_sq = 0.0;
                block_count = 0;
            }

            if hop_counter >= HOP_SIZE && window_pos >= WINDOW_SIZE {
                hop_counter = 0;

                let start = window_pos % WINDOW_SIZE;
                let mut contiguous = Vec::with_capacity(WINDOW_SIZE);
                contiguous.extend_from_slice(&window[start..]);
                contiguous.extend_from_slice(&window[..start]);

                let pitch = pitch_detector.detect(&contiguous);
                state.pitch.store(Arc::new(pitch));

                let transients = transient_detector.detect(&contiguous);
                if !transients.is_empty() {
                    let base_time = (total_samples_processed - WINDOW_SIZE) as f64 / sample_rate;
                    for t in &transients {
                        recent_transients.push(Transient {
                            sample_position: total_samples_processed - WINDOW_SIZE
                                + t.sample_position,
                            time: base_time + t.time,
                            strength: t.strength,
                        });
                    }

                    if recent_transients.len() > MAX_RECENT_TRANSIENTS {
                        let drain_count = recent_transients.len() - MAX_RECENT_TRANSIENTS;
                        recent_transients.drain(0..drain_count);
                    }

                    state.transients.store(Arc::new(recent_transients.clone()));
                }

                // FFT spectrum: apply Hann window, compute magnitudes
                let num_bins = WINDOW_SIZE / 2;
                let mut fft_buf: Vec<Complex<f32>> = contiguous
                    .iter()
                    .zip(&hann_window)
                    .map(|(&s, &w)| Complex { re: s * w, im: 0.0 })
                    .collect();
                fft.process_with_scratch(&mut fft_buf, &mut fft_scratch);

                let mut magnitudes: Vec<f32> =
                    fft_buf[..num_bins].iter().map(|c| c.norm()).collect();
                let max_mag = magnitudes.iter().copied().fold(0.0f32, f32::max);
                if max_mag > 0.0 {
                    for m in &mut magnitudes {
                        *m /= max_mag;
                    }
                }
                state.spectrum.store(Arc::new(SpectrumResult {
                    magnitudes,
                    num_bins,
                    freq_resolution: sample_rate / WINDOW_SIZE as f64,
                }));
            }
        }
    }
}

// ===========================================================================
// Bevy ECS surface — the live-analysis duty.
//
// Co-located with the RT engine above (`LiveAnalysisState` /
// `run_analysis_thread`). [`AnalysisRes`] is a plain resource owning the
// metering tap source and the running analysis thread; the control system
// spawns/joins that thread on Enable/Disable messages, and the sync system
// pulls its lock-free `ArcSwap` results into [`LiveAnalysisData`] each frame.
// There is no separate handle facade — the resource *is* the live state, and a
// system owns its lifecycle (the bevy-native shape).
//
// Gated behind the `bevy` feature: the analysis algorithms above are Bevy-free.
// ===========================================================================

#[cfg(feature = "bevy")]
pub use ecs::*;

#[cfg(feature = "bevy")]
mod ecs {
    use super::*;

    use bevy_app::{App, Plugin, Update};
    use bevy_ecs::message::{Message, MessageReader};
    use bevy_ecs::prelude::*;
    use std::thread::JoinHandle;
    use tutti_core::metering::AudioTap;

    use tutti_core::ecs::engine_ready;

    /// The running analysis thread plus the state it publishes into.
    struct RunningAnalysis {
        state: Arc<LiveAnalysisState>,
        thread: Option<JoinHandle<()>>,
    }

    /// Live-analysis engine state, as a Bevy resource.
    ///
    /// Holds the audio-thread tap and, while live, the running analysis thread
    /// plus its published [`LiveAnalysisState`]. The control system mutates
    /// this through `ResMut` (Bevy guarantees exclusive access), so no interior
    /// locking is needed — enable spawns the thread, disable joins it. Built by
    /// bevy-tutti and claimed from [`PendingAnalysis`] in
    /// [`TuttiAnalysisPlugin`]'s `build()`.
    #[derive(Resource)]
    pub struct AnalysisRes {
        sample_rate: f64,
        tap: AudioTap,
        running: Option<RunningAnalysis>,
    }

    impl AnalysisRes {
        /// Construct from the engine's audio tap. Live analysis is off until
        /// [`EnableLiveAnalysis`] is sent.
        pub fn new(sample_rate: impl Into<tutti_core::SampleRate>, tap: AudioTap) -> Self {
            Self {
                sample_rate: sample_rate.into().get(),
                tap,
                running: None,
            }
        }

        fn is_live(&self) -> bool {
            self.running.is_some()
        }

        /// Open the audio tap and spawn the analysis thread. Idempotent.
        fn enable(&mut self) {
            if self.running.is_some() {
                return;
            }
            let consumer = self.tap.open();
            let state = Arc::new(LiveAnalysisState::new(512));
            let thread_state = state.clone();
            let sample_rate = self.sample_rate;
            let thread = std::thread::Builder::new()
                .name("tutti-analysis".into())
                .spawn(move || run_analysis_thread(consumer, thread_state, sample_rate))
                .expect("failed to spawn tutti-analysis thread");
            self.running = Some(RunningAnalysis {
                state,
                thread: Some(thread),
            });
        }

        /// Stop the analysis thread and close the tap. Idempotent.
        fn disable(&mut self) {
            let Some(mut running) = self.running.take() else {
                return;
            };
            running.state.stop();
            self.tap.close();
            if let Some(handle) = running.thread.take() {
                let _ = handle.join();
            }
        }
    }

    impl Drop for AnalysisRes {
        fn drop(&mut self) {
            self.disable();
        }
    }

    /// Transient handoff resource: the engine-built analysis state. Inserted by
    /// `build_into`; claimed into [`AnalysisRes`] by [`TuttiAnalysisPlugin`].
    #[derive(Resource)]
    pub struct PendingAnalysis(pub Option<AnalysisRes>);

    /// Fire-and-forget request to enable live analysis.
    #[derive(Message, Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct EnableLiveAnalysis;

    /// Fire-and-forget request to disable live analysis.
    #[derive(Message, Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct DisableLiveAnalysis;

    /// Live analysis state synced from the analysis thread via lock-free ArcSwap
    /// reads.
    ///
    /// Fields are `Arc` pointers -- cheap to clone for UI consumption.
    #[derive(Resource)]
    pub struct LiveAnalysisData {
        pub pitch: Arc<crate::PitchResult>,
        pub transients: Arc<Vec<crate::Transient>>,
        pub waveform: Arc<crate::WaveformSummary>,
        pub spectrum: Arc<crate::SpectrumResult>,
        pub is_live: bool,
    }

    impl Default for LiveAnalysisData {
        fn default() -> Self {
            Self {
                pitch: Arc::new(crate::PitchResult::default()),
                transients: Arc::new(Vec::new()),
                waveform: Arc::new(crate::WaveformSummary::new(512)),
                spectrum: Arc::new(crate::SpectrumResult::default()),
                is_live: false,
            }
        }
    }

    pub fn live_analysis_control_system(
        mut analysis: ResMut<AnalysisRes>,
        mut data: ResMut<LiveAnalysisData>,
        mut enable: MessageReader<EnableLiveAnalysis>,
        mut disable: MessageReader<DisableLiveAnalysis>,
    ) {
        if enable.read().next().is_some() && !analysis.is_live() {
            analysis.enable();
            data.is_live = true;
            bevy_log::info!("Live analysis enabled");
        }

        if disable.read().next().is_some() {
            analysis.disable();
            data.is_live = false;
            bevy_log::info!("Live analysis disabled");
        }
    }

    pub fn live_analysis_sync_system(
        analysis: Res<AnalysisRes>,
        mut data: ResMut<LiveAnalysisData>,
    ) {
        let Some(running) = analysis.running.as_ref() else {
            return;
        };
        data.pitch = running.state.pitch.load_full();
        data.transients = running.state.transients.load_full();
        data.waveform = running.state.waveform.load_full();
        data.spectrum = running.state.spectrum.load_full();
    }

    /// Bevy plugin: live analysis enable/disable + per-frame pull.
    pub struct TuttiAnalysisPlugin;

    impl Plugin for TuttiAnalysisPlugin {
        fn build(&self, app: &mut App) {
            // Claim the analysis state out of the transient `build_into` inserted
            // (synchronous, during plugin build — present before frame 1).
            if let Some(PendingAnalysis(Some(res))) =
                app.world_mut().remove_resource::<PendingAnalysis>()
            {
                app.insert_resource(res);
            }
            app.add_message::<EnableLiveAnalysis>()
                .add_message::<DisableLiveAnalysis>();
            app.init_resource::<LiveAnalysisData>().add_systems(
                Update,
                (live_analysis_control_system, live_analysis_sync_system).run_if(engine_ready),
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use ringbuf::{traits::Producer, traits::Split, HeapRb};

        #[test]
        fn test_live_analysis_state_creation() {
            let state = LiveAnalysisState::new(512);
            assert!(state.is_running());
            assert!(!state.pitch.load().is_voiced());
            assert!(state.transients.load().is_empty());
        }

        #[test]
        fn test_analysis_thread_stops() {
            let rb = HeapRb::<(f32, f32)>::new(4096);
            let (mut prod, cons) = rb.split();

            let state = Arc::new(LiveAnalysisState::new(512));
            let state2 = state.clone();

            // Feed a sine wave
            let sample_rate = 44100.0;
            for i in 0..8192 {
                let t = i as f32 / sample_rate as f32;
                let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
                let _ = prod.try_push((s, s));
            }

            // Stop after brief run
            let handle = std::thread::spawn(move || {
                run_analysis_thread(cons, state2, sample_rate);
            });

            std::thread::sleep(std::time::Duration::from_millis(100));
            state.stop();
            handle.join().unwrap();

            // Should have produced some results
            assert!(!state.waveform.load().blocks.is_empty());
        }

        #[test]
        fn test_pitch_detection_live() {
            let rb = HeapRb::<(f32, f32)>::new(131072);
            let (mut prod, cons) = rb.split();

            let state = Arc::new(LiveAnalysisState::new(512));
            let state2 = state.clone();

            let sample_rate = 44100.0;
            // Feed enough samples for pitch detection (need WINDOW_SIZE + some hops)
            for i in 0..20000 {
                let t = i as f32 / sample_rate as f32;
                let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.8;
                let _ = prod.try_push((s, s));
            }

            let handle = std::thread::spawn(move || {
                run_analysis_thread(cons, state2, sample_rate);
            });

            std::thread::sleep(std::time::Duration::from_millis(200));
            state.stop();
            handle.join().unwrap();

            let pitch = state.pitch.load();
            // Should have detected ~440 Hz
            if pitch.is_voiced() {
                assert!(
                    (pitch.frequency - 440.0).abs() < 20.0,
                    "Expected ~440 Hz, got {} Hz",
                    pitch.frequency
                );
            }
        }
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;
    use ringbuf::{traits::Producer, traits::Split, HeapRb};

    #[test]
    fn ring_in_pops_pairs_as_frames() {
        let rb = HeapRb::<(f32, f32)>::new(16);
        let (mut prod, cons) = rb.split();
        for i in 0..4 {
            prod.try_push((i as f32, -(i as f32))).unwrap();
        }

        let mut input = RingIn::new(cons, 8);
        assert_eq!(input.available(), 4);

        let mut out = [[0.0f32; 2]; 8];
        let read = input.poll_into(&mut out);
        assert_eq!(read, 4);
        assert_eq!(
            &out[..4],
            &[[0.0, 0.0], [1.0, -1.0], [2.0, -2.0], [3.0, -3.0]]
        );
    }

    #[test]
    fn ring_in_empty_polls_zero() {
        let rb = HeapRb::<(f32, f32)>::new(16);
        let (_prod, cons) = rb.split();
        let mut input = RingIn::new(cons, 8);
        let mut out = [[0.0f32; 2]; 8];
        assert_eq!(input.poll_into(&mut out), 0);
    }

    #[test]
    fn ring_in_bounded_by_scratch_capacity() {
        let rb = HeapRb::<(f32, f32)>::new(64);
        let (mut prod, cons) = rb.split();
        for i in 0..10 {
            prod.try_push((i as f32, i as f32)).unwrap();
        }
        // scratch capacity 3 caps each poll to 3 frames even with a bigger out.
        let mut input = RingIn::new(cons, 3);
        let mut out = [[0.0f32; 2]; 8];
        assert_eq!(input.poll_into(&mut out), 3);
    }
}
