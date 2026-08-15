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

use tutti_core::SampleRate;
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
        let min_gap = Seconds(0.05).to_samples(geometry.sample_rate().get());
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
    /// Takes [`Seconds`], like every other time-valued setter in the engine —
    /// milliseconds on one setter and seconds on another of the same type is
    /// how a gap ends up a thousand times off.
    pub fn with_min_gap(mut self, gap: impl Into<Seconds>) -> Self {
        self.min_gap = gap.into().to_samples(self.geometry.sample_rate().get());
        self
    }

    /// The window, hop and sample rate onsets are detected on.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// Which detection function measures novelty between frames.
    #[inline]
    pub fn function(&self) -> DetectionFunction {
        self.function
    }

    /// Minimum spacing between accepted onsets, in [`Samples`] — resolved from
    /// the [`Seconds`] the setter takes, against this config's sample rate.
    #[inline]
    pub fn min_gap(&self) -> Samples {
        self.min_gap
    }
}

/// One detected onset.
///
/// Position only, with the time derived from it on demand. Storing both invites
/// computing them against two different sample rates, so one struct describes
/// two different instants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Onset {
    /// Where the onset sits in the analyzed buffer, in [`Samples`].
    pub position: Samples,
    /// Novelty at that frame as an [`Amplitude`] — how far the detection
    /// function rose above its adaptive threshold.
    pub strength: Amplitude,
}

impl Onset {
    /// The onset's position in [`Seconds`], resolved against `sample_rate`.
    ///
    /// Derived rather than stored, so it cannot disagree with
    /// [`position`](Self::position).
    #[inline]
    pub fn time(&self, sample_rate: impl Into<SampleRate>) -> Seconds {
        Seconds((self.position.get() as f64 / sample_rate.into().get()) as f32)
    }
}

/// What the detector carries between frames.
///
/// Explicit, so a batch run cannot inherit a streaming run's history — the
/// previous frame's magnitudes are a value the caller holds, not state hidden
/// behind `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct OnsetState {
    /// The config this carry belongs to.
    ///
    /// A carry is only meaningful for the config that produced it, and
    /// `OnsetConfig` is `Copy` and passed per call — so nothing stops a caller
    /// varying it mid-run. Recording it here lets [`step_onset`] reset instead
    /// of measuring novelty against a reference from a different detection
    /// function or a different time grid.
    config: Option<OnsetConfig>,
    previous: Vec<f32>,
    /// The current frame's bins, reused across frames.
    ///
    /// Lives here rather than in [`FftScratch`] because `FftScratch::forward`
    /// takes its output slice from the caller — the buffer is this state's to
    /// own, and it sits beside `previous` because the two are resized by the
    /// same bin count on the same line.
    spectrum: Vec<Complex>,
    /// Novelty per frame, in analysis order. Thresholding is global, so peaks
    /// can only be picked once the run is complete.
    novelty: Vec<(Samples, f32)>,
}

impl OnsetState {
    /// An empty carry: no previous frame, no accumulated novelty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything. Equivalent to starting with a fresh state.
    pub fn reset(&mut self) {
        self.config = None;
        self.previous.clear();
        self.spectrum.clear();
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
    // The carry is only meaningful for the config that produced it. Comparing
    // the whole config — not just the bin count — is what makes this correct:
    // two geometries can share a window (and so a bin count) while differing
    // in hop, which puts the carried frame on a different time grid.
    if state.config != Some(*cfg) {
        state.reset();
        state.config = Some(*cfg);
    }

    let bins = cfg.geometry.bins_per_frame().get();
    if state.previous.len() != bins {
        state.previous = vec![0.0; bins];
    }
    // Resized, not rebuilt: `forward` overwrites every bin it is handed, so the
    // contents carry nothing between frames — only the allocation does.
    if state.spectrum.len() != bins {
        state.spectrum.resize(bins, Complex::default());
    }

    let position = Samples(frame.get() * cfg.geometry.hop().get());
    let window = cfg.geometry.window_coefficients();

    // Every spectral branch stores its magnitudes, including the ones that do
    // not read them. Skipping the store leaves `previous` stale or zeroed, so
    // the first frame after a switch measured novelty against the wrong
    // reference — overstating flux by ~12x and complex-domain deviation by
    // ~15,000x in measurement, which then dominates the adaptive threshold and
    // the strength normalization for the *whole* run.
    let OnsetState {
        previous, spectrum, ..
    } = state;
    let value = match cfg.function {
        DetectionFunction::Energy => spectral_energy(samples),
        DetectionFunction::SpectralFlux => {
            fft.forward(samples, &window, spectrum);
            let v = spectral_flux(previous, spectrum);
            store_magnitudes(previous, spectrum);
            v
        }
        DetectionFunction::HighFrequencyContent => {
            fft.forward(samples, &window, spectrum);
            let v = high_frequency_content(spectrum) * 0.01;
            store_magnitudes(previous, spectrum);
            v
        }
        DetectionFunction::ComplexDomain => {
            fft.forward(samples, &window, spectrum);
            let v = complex_domain_deviation(previous, spectrum);
            store_magnitudes(previous, spectrum);
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
pub fn detect_onsets(
    cfg: &OnsetConfig,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<Vec<Onset>> {
    let window = cfg.geometry.window().get();
    if samples.len() < window {
        return Ok(Vec::new());
    }

    let mut state = OnsetState::new();
    for frame in cfg.geometry.frames_for(Samples(samples.len())).indices() {
        let offset = frame.get() * cfg.geometry.hop().get();
        step_onset(
            cfg,
            &mut state,
            frame,
            &samples[offset..offset + window],
            fft,
        );
    }
    Ok(finish(cfg, &state))
}

/// Drop onsets closer together than `min_gap`, keeping the stronger of a pair.
///
/// Order-independent: the gap is a distance, so an unsorted list is compared
/// correctly rather than underflowing. `pick_peaks` emits ascending positions,
/// but this is public and takes a plain `Vec`, so it cannot assume that.
pub fn suppress_close_onsets(onsets: &mut Vec<Onset>, min_gap: Samples) {
    if onsets.len() < 2 {
        return;
    }
    let mut i = 1;
    while i < onsets.len() {
        let too_close = onsets[i]
            .position
            .get()
            .abs_diff(onsets[i - 1].position.get())
            < min_gap.get();
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
    let (sum, sum_sq, max) = novelty
        .iter()
        .fold((0.0f32, 0.0f32, 0.0f32), |(s, sq, mx), &(_, v)| {
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
                strength: Amplitude(if max > 0.0 {
                    (value / max).min(1.0)
                } else {
                    0.0
                }),
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
        for frame in cfg.geometry().frames_for(Samples(samples.len())).indices() {
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
    /// visible at the call site rather than hidden in a method.
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

    /// The per-frame spectrum buffer is allocated once, not per frame.
    ///
    /// `detect_onsets` folds `step_onset` over every frame of the buffer, so a
    /// fresh `vec![Complex; bins]` there cost one allocation per frame — tens
    /// of thousands for a few minutes of audio.
    ///
    /// Asserted by **writing a sentinel into the buffer and seeing it survive**
    /// the resize check on the next frame. Two weaker observables were tried
    /// and rejected: `capacity()` is identical either way (a fresh
    /// `vec![_; bins]` allocates exactly `bins`), and this crate deliberately
    /// carries no `assert_no_alloc` dev-dependency — see its Cargo.toml, which
    /// records that everything here is cold-path batch analysis that
    /// legitimately allocates its output. A buffer that is *replaced* loses the
    /// sentinel; one that is refilled in place keeps the allocation and only
    /// has its bins overwritten by `forward`.
    #[test]
    fn the_spectrum_buffer_is_reused_across_frames() {
        let cfg = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux);
        let mut fft = FftScratch::new();
        let mut state = OnsetState::new();
        let samples = signal(1.0, &[0.1, 0.3, 0.5, 0.7]);

        // First frame sizes the buffer.
        let window = cfg.geometry().window().get();
        step_onset(
            &cfg,
            &mut state,
            FrameIndex(0),
            &samples[..window],
            &mut fft,
        );
        let bins = state.spectrum.len();
        assert!(bins > 0, "the first frame must have sized the buffer");

        // Reserve well past what a frame needs. `resize` back down to `bins`
        // keeps the larger allocation; `vec![_; bins]` requests a fresh one
        // sized exactly `bins`, so the spare capacity is what separates them.
        state.spectrum.reserve_exact(bins * 4);
        let reserved = state.spectrum.capacity();
        assert!(reserved > bins, "the reserve must actually over-allocate");

        run(&cfg, &mut state, &samples, &mut fft);

        assert!(state.frames_seen() > 8, "the run must cover many frames");
        assert_eq!(
            state.spectrum.len(),
            bins,
            "the buffer is exactly one frame of bins"
        );
        assert_eq!(
            state.spectrum.capacity(),
            reserved,
            "every frame must refill the same allocation — a rebuilt \
             `vec![_; bins]` would drop this spare capacity"
        );
    }

    fn run(cfg: &OnsetConfig, state: &mut OnsetState, samples: &[f32], fft: &mut FftScratch) {
        let window = cfg.geometry().window().get();
        for frame in cfg.geometry().frames_for(Samples(samples.len())).indices() {
            let offset = frame.get() * cfg.geometry().hop().get();
            step_onset(cfg, state, frame, &samples[offset..offset + window], fft);
        }
    }

    /// Changing the config mid-run invalidates the carry.
    ///
    /// `OnsetConfig` is `Copy` and passed per call, so a caller can vary it
    /// between frames. Before this was handled, the first frame after a switch
    /// diffed against a reference from the previous function — measured at
    /// ~12x overstatement for flux and ~15,000x for complex-domain — which
    /// then dominated the adaptive threshold and strength normalization for
    /// the whole run.
    #[test]
    fn switching_config_mid_run_resets_the_carry() {
        let samples = signal(0.5, &[0.1, 0.25]);
        let mut fft = FftScratch::new();

        let hfc = OnsetConfig::new(geometry(), DetectionFunction::HighFrequencyContent);
        let flux = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux);

        // Run HFC frames, then switch to flux on the same state.
        let mut switched = OnsetState::new();
        run(&hfc, &mut switched, &samples, &mut fft);
        run(&flux, &mut switched, &samples, &mut fft);

        // A state that only ever saw flux.
        let mut clean = OnsetState::new();
        run(&flux, &mut clean, &samples, &mut fft);

        assert_eq!(
            finish(&flux, &switched),
            finish(&flux, &clean),
            "a config switch must discard the previous function's carry"
        );
    }

    /// Same window, different hop: the bin count is identical, so a
    /// bins-only guard would not notice the time grid changed.
    #[test]
    fn a_hop_change_alone_resets_the_carry() {
        let samples = signal(0.5, &[0.1, 0.25]);
        let mut fft = FftScratch::new();

        let coarse = OnsetConfig::new(
            StftGeometry::new(SAMPLE_RATE, Samples(2048), Samples(1024)).unwrap(),
            DetectionFunction::SpectralFlux,
        );
        let fine = OnsetConfig::new(geometry(), DetectionFunction::SpectralFlux);
        assert_eq!(
            coarse.geometry().bins_per_frame(),
            fine.geometry().bins_per_frame(),
            "the two geometries must share a bin count for this test to bite"
        );

        let mut switched = OnsetState::new();
        run(&coarse, &mut switched, &samples, &mut fft);
        run(&fine, &mut switched, &samples, &mut fft);

        let mut clean = OnsetState::new();
        run(&fine, &mut clean, &samples, &mut fft);

        assert_eq!(finish(&fine, &switched), finish(&fine, &clean));
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

    /// Unsorted input must not underflow.
    ///
    /// `pick_peaks` emits ascending positions, so the in-crate path never hit
    /// this — but the function is public and takes a plain `Vec`, and
    /// subtracting `usize` positions panicked in debug and wrapped to a huge
    /// value in release, where the pair would silently never be suppressed.
    #[test]
    fn suppression_handles_unsorted_input() {
        let onset = |pos: usize, strength: f32| Onset {
            position: Samples(pos),
            strength: Amplitude(strength),
        };

        let mut descending = vec![onset(9000, 0.4), onset(100, 0.9)];
        suppress_close_onsets(&mut descending, Samples(1000));
        assert_eq!(descending.len(), 2, "far apart in either direction");

        let mut close = vec![onset(150, 0.4), onset(100, 0.9)];
        suppress_close_onsets(&mut close, Samples(1000));
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].strength, Amplitude(0.9));
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
        assert!(detect_onsets(&cfg, &[0.0; 100], &mut fft)
            .unwrap()
            .is_empty());
    }
}
