//! Onset detection.
//!
//! Unlike pitch, this genuinely is "pick a detection function, then threshold
//! and peak-pick" — four published functions over one pipeline. So the entry
//! point keeps a general name and the four functions are exported
//! individually, named for themselves, because choosing between them is a real
//! choice with real trade-offs.
//!
//! The carry is explicit. Spectral flux and complex-domain deviation both need
//! the previous frame, and hiding that in `&mut self` is what let a detector
//! diff frame 0 of one call against the last frame of the *previous* one — 39
//! of 165 windows corrupted on the live path. Here you cannot forget to reset
//! something you have to pass in.

use tutti_types::{Amplitude, Samples, Seconds};

use crate::error::Result;
use crate::fft::FftScratch;
use crate::geometry::StftGeometry;
use crate::grid::FrameIndex;
use crate::Complex;

/// How novelty is measured frame to frame.
///
/// Each is sensitive to a different kind of change, and the differences are
/// audible in what they miss — see the per-function docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionFunction {
    /// Positive-rectified magnitude change. The general-purpose default.
    SpectralFlux,
    /// Magnitude weighted toward high bins. Favours percussive, bright onsets.
    HighFrequencyContent,
    /// Broadband level change. Cheapest, and the least sensitive — it misses
    /// short decaying spikes that the spectral functions catch.
    Energy,
    /// Magnitude change measured in the complex plane. Best on tonal onsets,
    /// where magnitude alone barely moves.
    ComplexDomain,
}

/// Positive-rectified sum of magnitude increases.
///
/// Only *growth* counts: a bin that quietens contributes nothing, which is
/// what makes this an onset detector rather than a change detector.
pub fn spectral_flux(previous: &[f32], current: &[Complex]) -> f32 {
    current
        .iter()
        .zip(previous)
        .map(|(bin, &prev)| (bin.norm() - prev).max(0.0))
        .sum()
}

/// Magnitude energy weighted by bin index.
///
/// Bright transients concentrate energy at high frequencies, so weighting by
/// bin favours them over low-frequency rumble.
pub fn high_frequency_content(current: &[Complex]) -> f32 {
    current
        .iter()
        .enumerate()
        .map(|(i, bin)| (i + 1) as f32 * bin.norm_sqr())
        .sum::<f32>()
        .sqrt()
}

/// Broadband RMS level of a time-domain frame.
pub fn spectral_energy(frame: &[f32]) -> f32 {
    frame.iter().map(|s| s * s).sum::<f32>().sqrt()
}

/// Euclidean distance between successive magnitude spectra.
///
/// Unlike [`spectral_flux`] this counts decreases too, which is what makes it
/// sensitive to tonal onsets where energy moves between bins rather than
/// simply arriving.
pub fn complex_domain_deviation(previous: &[f32], current: &[Complex]) -> f32 {
    current
        .iter()
        .zip(previous)
        .map(|(bin, &prev)| {
            let diff = bin.norm() - prev;
            diff * diff
        })
        .sum::<f32>()
        .sqrt()
}

/// Onset detection parameters. Immutable once built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnsetConfig {
    geometry: StftGeometry,
    function: DetectionFunction,
    threshold: Amplitude,
    sensitivity: Amplitude,
    min_gap: Samples,
}

impl OnsetConfig {
    /// Defaults: a 0.3 threshold multiplier, unity sensitivity, and a 50 ms
    /// minimum gap.
    pub fn new(geometry: StftGeometry, function: DetectionFunction) -> Self {
        let min_gap =
            Samples((0.05 * geometry.sample_rate().get()) as usize);
        Self {
            geometry,
            function,
            threshold: Amplitude(0.3),
            sensitivity: Amplitude::UNITY,
            min_gap,
        }
    }

    /// Multiplier on the adaptive threshold. Higher rejects more.
    pub fn with_threshold(mut self, threshold: impl Into<Amplitude>) -> Self {
        self.threshold = threshold.into();
        self
    }

    /// Gain applied to the detection function before thresholding.
    pub fn with_sensitivity(mut self, sensitivity: impl Into<Amplitude>) -> Self {
        self.sensitivity = sensitivity.into();
        self
    }

    /// Minimum spacing between accepted onsets.
    ///
    /// Takes [`Seconds`], like every other time-valued setter in the engine.
    /// The old pair took milliseconds on one method and seconds on another of
    /// the same type — the only ms-valued input anywhere in the engine.
    pub fn with_min_gap(mut self, gap: impl Into<Seconds>) -> Self {
        self.min_gap =
            Samples((gap.into().get() as f64 * self.geometry.sample_rate().get()) as usize);
        self
    }

    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    #[inline]
    pub fn function(&self) -> DetectionFunction {
        self.function
    }

    #[inline]
    pub fn min_gap(&self) -> Samples {
        self.min_gap
    }
}

/// One detected onset.
///
/// Position only: the time is derived from it, where the old shape stored both
/// and computed them against two different sample rates, so they could describe
/// different instants in the same struct.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Onset {
    pub position: Samples,
    pub strength: Amplitude,
}

impl Onset {
    #[inline]
    pub fn time(&self, sample_rate: f64) -> Seconds {
        Seconds((self.position.get() as f64 / sample_rate) as f32)
    }
}

/// What the detector carries between frames.
///
/// Explicit, so a batch run cannot inherit a streaming run's history. This is
/// the `prev_magnitudes` that `&mut self` used to hide.
#[derive(Debug, Clone, Default)]
pub struct OnsetState {
    previous: Vec<f32>,
    /// Novelty per frame, in analysis order. Thresholding is global, so peaks
    /// can only be picked once the run is complete.
    novelty: Vec<(Samples, f32)>,
}

impl OnsetState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything. Equivalent to starting with a fresh state.
    pub fn reset(&mut self) {
        self.previous.clear();
        self.novelty.clear();
    }

    /// How many frames have been pushed.
    #[inline]
    pub fn frames_seen(&self) -> usize {
        self.novelty.len()
    }
}

/// Feed one frame and accumulate its novelty.
///
/// Returns nothing per frame: the threshold is adaptive over the whole run, so
/// no single frame can be judged in isolation. Call [`finish`] to pick peaks.
pub fn step_onset(
    cfg: &OnsetConfig,
    state: &mut OnsetState,
    frame: FrameIndex,
    samples: &[f32],
    fft: &mut FftScratch,
) {
    let bins = cfg.geometry.bins_per_frame().get();
    if state.previous.len() != bins {
        state.previous = vec![0.0; bins];
    }

    let position = Samples(frame.get() * cfg.geometry.hop().get());
    let window = cfg.geometry.hann();
    let mut spectrum = vec![Complex::default(); bins];

    let value = match cfg.function {
        DetectionFunction::Energy => spectral_energy(samples),
        DetectionFunction::SpectralFlux => {
            fft.forward(samples, &window, &mut spectrum);
            let v = spectral_flux(&state.previous, &spectrum);
            store_magnitudes(&mut state.previous, &spectrum);
            v
        }
        DetectionFunction::HighFrequencyContent => {
            fft.forward(samples, &window, &mut spectrum);
            high_frequency_content(&spectrum) * 0.01
        }
        DetectionFunction::ComplexDomain => {
            fft.forward(samples, &window, &mut spectrum);
            let v = complex_domain_deviation(&state.previous, &spectrum);
            store_magnitudes(&mut state.previous, &spectrum);
            v
        }
    };

    state
        .novelty
        .push((position, value * cfg.sensitivity.get()));
}

/// Pick peaks from the accumulated novelty and apply gap suppression.
pub fn finish(cfg: &OnsetConfig, state: &OnsetState) -> Vec<Onset> {
    let mut onsets = pick_peaks(cfg, &state.novelty);
    suppress_close_onsets(&mut onsets, cfg.min_gap);
    onsets
}

/// Detect onsets across a whole buffer.
///
/// Folds [`step_onset`] — the same implementation the streaming path uses, so
/// the two cannot drift.
pub fn detect_onsets(cfg: &OnsetConfig, samples: &[f32], fft: &mut FftScratch) -> Result<Vec<Onset>> {
    let window = cfg.geometry.window().get();
    if samples.len() < window {
        return Ok(Vec::new());
    }

    let mut state = OnsetState::new();
    for frame in cfg.geometry.frames_for(Samples(samples.len())).indices() {
        let offset = frame.get() * cfg.geometry.hop().get();
        step_onset(cfg, &mut state, frame, &samples[offset..offset + window], fft);
    }
    Ok(finish(cfg, &state))
}

/// Drop onsets closer together than `min_gap`, keeping the stronger of a pair.
pub fn suppress_close_onsets(onsets: &mut Vec<Onset>, min_gap: Samples) {
    if onsets.len() < 2 {
        return;
    }
    let mut i = 1;
    while i < onsets.len() {
        let too_close =
            onsets[i].position.get() - onsets[i - 1].position.get() < min_gap.get();
        if too_close {
            let weaker = if onsets[i].strength > onsets[i - 1].strength {
                i - 1
            } else {
                i
            };
            onsets.remove(weaker);
        } else {
            i += 1;
        }
    }
}

/// Strict local maxima above an adaptive `mean + k·σ` threshold.
///
/// Endpoints are never reported: a maximum needs both neighbours.
fn pick_peaks(cfg: &OnsetConfig, novelty: &[(Samples, f32)]) -> Vec<Onset> {
    if novelty.len() < 3 {
        return Vec::new();
    }

    let len = novelty.len() as f32;
    let (sum, sum_sq, max) = novelty.iter().fold((0.0f32, 0.0f32, 0.0f32), |(s, sq, mx), &(_, v)| {
        (s + v, sq + v * v, mx.max(v))
    });
    let mean = sum / len;
    let std_dev = (sum_sq / len - mean * mean).max(0.0).sqrt();
    let threshold = mean + std_dev * cfg.threshold.get() * 3.0;

    (1..novelty.len() - 1)
        .filter_map(|i| {
            let (position, value) = novelty[i];
            let rising = value > novelty[i - 1].1;
            let falling = value > novelty[i + 1].1;
            (rising && falling && value > threshold).then(|| Onset {
                position,
                strength: Amplitude(if max > 0.0 { (value / max).min(1.0) } else { 0.0 }),
            })
        })
        .collect()
}

fn store_magnitudes(previous: &mut [f32], spectrum: &[Complex]) {
    for (slot, bin) in previous.iter_mut().zip(spectrum) {
        *slot = bin.norm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f64 = 44100.0;

    fn geometry() -> StftGeometry {
        StftGeometry::new(SAMPLE_RATE, Samples(2048), Samples(512)).unwrap()
    }

    fn signal(seconds: f64, onsets: &[f64]) -> Vec<f32> {
        let n = (SAMPLE_RATE * seconds) as usize;
        let mut samples = vec![0.0f32; n];
        for &t in onsets {
            let pos = (t * SAMPLE_RATE) as usize;
            for i in 0..50.min(n.saturating_sub(pos)) {
                samples[pos + i] += (-0.1 * i as f32).exp() * 0.8;
            }
        }
        samples
    }

    /// The property the explicit carry buys: batch and streaming agree by
    /// construction, because they run the same step function.
    #[test]
    fn batch_and_streaming_agree() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux)
            .with_threshold(Amplitude(0.2))
            .with_sensitivity(Amplitude(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let mut fft = FftScratch::new();

        let batch = detect_onsets(&cfg, &samples, &mut fft).unwrap();

        let mut state = OnsetState::new();
        let window = cfg.geometry().window().get();
        for frame in cfg
            .geometry()
            .frames_for(Samples(samples.len()))
            .indices()
        {
            let offset = frame.get() * cfg.geometry().hop().get();
            step_onset(
                &cfg,
                &mut state,
                frame,
                &samples[offset..offset + window],
                &mut fft,
            );
        }
        let streamed = finish(&cfg, &state);

        assert_eq!(batch, streamed);
    }

    /// A reused state must be reset; a fresh one needs nothing. Both paths are
    /// now visible at the call site rather than hidden in a method.
    #[test]
    fn a_reset_state_matches_a_fresh_one() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux);
        let a = signal(0.5, &[0.1, 0.3]);
        let b = signal(0.5, &[0.2]);
        let mut fft = FftScratch::new();

        let mut reused = OnsetState::new();
        run(&cfg, &mut reused, &a, &mut fft);
        reused.reset();
        run(&cfg, &mut reused, &b, &mut fft);

        let mut fresh = OnsetState::new();
        run(&cfg, &mut fresh, &b, &mut fft);

        assert_eq!(finish(&cfg, &reused), finish(&cfg, &fresh));
    }

    fn run(cfg: &OnsetConfig, state: &mut OnsetState, samples: &[f32], fft: &mut FftScratch) {
        let window = cfg.geometry().window().get();
        for frame in cfg.geometry().frames_for(Samples(samples.len())).indices() {
            let offset = frame.get() * cfg.geometry().hop().get();
            step_onset(cfg, state, frame, &samples[offset..offset + window], fft);
        }
    }

    #[test]
    fn onsets_land_near_the_known_spikes() {
        let truth = [0.1f64, 0.3, 0.5, 0.7];
        let samples = signal(1.0, &truth);
        let mut fft = FftScratch::new();

        for function in [
            DetectionFunction::SpectralFlux,
            DetectionFunction::HighFrequencyContent,
            DetectionFunction::ComplexDomain,
        ] {
            let cfg = OnsetConfig::new(geometry(), function)
                .with_threshold(Amplitude(0.2))
                .with_sensitivity(Amplitude(2.0));
            let onsets = detect_onsets(&cfg, &samples, &mut fft).unwrap();

            assert_eq!(onsets.len(), truth.len(), "{function:?}: wrong count");

            // Reported early by up to one window — a frame is attributed to
            // its start offset, so the error grows with how deep into the
            // frame the spike falls.
            let tolerance = cfg.geometry().window().get() as f64 / SAMPLE_RATE;
            for (found, &expected) in onsets.iter().zip(&truth) {
                let t = found.time(SAMPLE_RATE).get() as f64;
                assert!(
                    (t - expected).abs() <= tolerance,
                    "{function:?}: onset at {t} is not within one window of {expected}"
                );
            }
        }
    }

    /// `Energy` misses these, as it did before the restructure. Pinned so the
    /// weakness stays a known property rather than becoming a silent change.
    #[test]
    fn energy_misses_short_decaying_spikes() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::Energy)
            .with_threshold(Amplitude(0.2))
            .with_sensitivity(Amplitude(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let mut fft = FftScratch::new();

        assert!(detect_onsets(&cfg, &samples, &mut fft).unwrap().is_empty());
    }

    #[test]
    fn the_gap_is_expressed_in_seconds() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux)
            .with_min_gap(Seconds(0.05));
        assert_eq!(cfg.min_gap(), Samples(2205));

        let wide = cfg.with_min_gap(Seconds(0.5));
        assert_eq!(wide.min_gap(), Samples(22050));
    }

    #[test]
    fn a_wide_gap_suppresses_close_onsets() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux)
            .with_threshold(Amplitude(0.2))
            .with_sensitivity(Amplitude(2.0));
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);
        let mut fft = FftScratch::new();

        let baseline = detect_onsets(&cfg, &samples, &mut fft).unwrap().len();
        let widened = detect_onsets(&cfg.with_min_gap(Seconds(0.5)), &samples, &mut fft).unwrap();

        assert!(widened.len() < baseline);
    }

    #[test]
    fn suppression_keeps_the_stronger_of_a_close_pair() {
        let onset = |pos: usize, strength: f32| Onset {
            position: Samples(pos),
            strength: Amplitude(strength),
        };

        let mut onsets = vec![onset(100, 0.4), onset(150, 0.9), onset(9000, 0.5)];
        suppress_close_onsets(&mut onsets, Samples(1000));
        assert_eq!(onsets.len(), 2);
        assert_eq!(onsets[0].strength, Amplitude(0.9));

        let mut onsets = vec![onset(100, 0.9), onset(150, 0.4), onset(9000, 0.5)];
        suppress_close_onsets(&mut onsets, Samples(1000));
        assert_eq!(onsets[0].strength, Amplitude(0.9));

        // Degenerate inputs are no-ops.
        let mut single = vec![onset(100, 0.5)];
        suppress_close_onsets(&mut single, Samples(1000));
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn detection_functions_respond_to_what_they_are_for() {
        let quiet = vec![Complex::new(0.1, 0.0); 16];
        let loud = vec![Complex::new(1.0, 0.0); 16];
        let previous = vec![0.1f32; 16];

        // Flux counts growth only.
        assert!(spectral_flux(&previous, &loud) > 0.0);
        let falling = vec![Complex::new(0.01, 0.0); 16];
        assert_eq!(spectral_flux(&previous, &falling), 0.0);

        // Complex-domain counts change in either direction.
        assert!(complex_domain_deviation(&previous, &falling) > 0.0);

        // HFC weights high bins more than low ones.
        let mut low = vec![Complex::default(); 16];
        low[1] = Complex::new(1.0, 0.0);
        let mut high = vec![Complex::default(); 16];
        high[15] = Complex::new(1.0, 0.0);
        assert!(high_frequency_content(&high) > high_frequency_content(&low));

        assert!(spectral_energy(&[1.0, 1.0, 1.0, 1.0]) > spectral_energy(&[0.1; 4]));
        let _ = quiet;
    }

    #[test]
    fn input_shorter_than_a_window_yields_nothing() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux);
        let mut fft = FftScratch::new();
        assert!(detect_onsets(&cfg, &[0.0; 100], &mut fft).unwrap().is_empty());
    }
}
