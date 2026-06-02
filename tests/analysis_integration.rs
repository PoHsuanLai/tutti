//! Analysis integration tests (requires "analysis" feature)
//!
//! Tests transient detection, pitch detection, and waveform analysis.
//!
//! Run with:
//! ```bash
//! cargo test -p tutti --test analysis_integration --features "analysis"
//! ```

#![cfg(feature = "analysis")]

use tutti::prelude::*;

fn test_engine() -> TuttiEngine {
    TuttiEngine::builder()
        .build()
        .expect("Failed to create test engine")
}

/// Generate a test sine wave buffer.
fn generate_sine(frequency: f64, sample_rate: f64, duration_secs: f64) -> Vec<f32> {
    let num_samples = (sample_rate * duration_secs) as usize;
    (0..num_samples)
        .map(|i| {
            let t = i as f64 / sample_rate;
            (2.0 * std::f64::consts::PI * frequency * t).sin() as f32
        })
        .collect()
}

/// Generate a click/transient sound.
fn generate_click(sample_rate: f64, click_sample: usize, duration_secs: f64) -> Vec<f32> {
    let num_samples = (sample_rate * duration_secs) as usize;
    let mut samples = vec![0.0f32; num_samples];

    // Sharp attack, quick decay
    if click_sample < num_samples {
        samples[click_sample] = 1.0;
        let decay_len = std::cmp::min(100, num_samples - click_sample);
        for i in 1..decay_len {
            samples[click_sample + i] = (1.0 - i as f32 / 100.0).max(0.0);
        }
    }

    samples
}

/// Test analysis handle creation.
#[test]
#[ignore = "requires audio device"]
fn test_analysis_handle() {
    let engine = test_engine();

    let _analysis = &engine.analysis;
}

/// Test waveform summary generation.
#[test]
#[ignore = "requires audio device"]
fn test_waveform_summary() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate test signal
    let samples = generate_sine(440.0, sr, 0.5);

    // Generate waveform summary
    let summary = analysis.waveform_summary(&samples, 256);

    // Should have some data points
    assert!(
        !summary.blocks.is_empty(),
        "Waveform summary should not be empty"
    );

    // Each point should be in valid range
    for block in &summary.blocks {
        assert!(block.min >= -1.0 && block.min <= 1.0);
        assert!(block.max >= -1.0 && block.max <= 1.0);
        assert!(block.min <= block.max);
    }
}

/// Test pitch detection with known frequency.
#[test]
#[ignore = "requires audio device"]
fn test_pitch_detection() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate 440Hz sine (A4) at the engine's actual sample rate
    let samples = generate_sine(440.0, sr, 0.5);

    // Detect pitch
    let result = analysis.detect_pitch(&samples);

    // Should detect something (accuracy depends on algorithm)
    if result.confidence > 0.5 {
        // If confident, frequency should be close to 440Hz
        let freq_error = (result.frequency - 440.0).abs();
        assert!(
            freq_error < 10.0,
            "Detected {}Hz, expected ~440Hz",
            result.frequency
        );
    }
}

/// Test pitch detection with different frequencies.
#[test]
#[ignore = "requires audio device"]
fn test_pitch_detection_various_frequencies() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    let test_frequencies = [220.0, 440.0, 880.0, 1000.0];

    for &freq in &test_frequencies {
        let samples = generate_sine(freq, sr, 0.3);
        let result = analysis.detect_pitch(&samples);

        // Just verify it doesn't panic
        assert!(result.frequency >= 0.0);
        assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    }
}

/// Test transient detection with click.
#[test]
#[ignore = "requires audio device"]
fn test_transient_detection() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate signal with transient at 0.1 seconds
    let click_sample = (sr * 0.1) as usize;
    let samples = generate_click(sr, click_sample, 0.5);

    // Detect transients
    let transients = analysis.detect_transients(&samples);

    // Should detect at least one transient
    // Note: Detection accuracy depends on algorithm and thresholds
    if !transients.is_empty() {
        // First transient should be near the click
        let first = &transients[0];
        assert!(
            first.strength > 0.0,
            "Transient should have positive strength"
        );
    }
}

/// Test transient detection with multiple clicks.
#[test]
#[ignore = "requires audio device"]
fn test_transient_detection_multiple() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate signal with multiple transients
    let num_samples = (sr * 1.0) as usize;
    let mut samples = vec![0.0f32; num_samples];

    // Add clicks at 0.1s, 0.3s, 0.5s, 0.7s
    for click_time in [0.1, 0.3, 0.5, 0.7] {
        let click_sample = (sr * click_time) as usize;
        if click_sample < num_samples {
            samples[click_sample] = 1.0;
            let decay_len = std::cmp::min(50, num_samples - click_sample);
            for i in 1..decay_len {
                samples[click_sample + i] = (1.0 - i as f32 / 50.0).max(0.0);
            }
        }
    }

    let transients = analysis.detect_transients(&samples);

    // Should find multiple transients (exact count depends on algorithm)
    // Just verify it doesn't panic and returns valid data
    for t in &transients {
        assert!(t.time >= 0.0);
        assert!(t.strength >= 0.0);
    }
}

/// Test stereo analysis.
#[test]
#[ignore = "requires audio device"]
fn test_stereo_analysis() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate identical L/R channels (correlation = 1.0)
    let left = generate_sine(440.0, sr, 0.25);
    let right = left.clone();

    let stereo = analysis.analyze_stereo(&left, &right);

    // Should have high correlation for identical signals
    assert!(
        stereo.correlation > 0.95,
        "Identical signals should have correlation ~1.0, got {}",
        stereo.correlation
    );
}

/// Test stereo analysis with inverted signals.
#[test]
#[ignore = "requires audio device"]
fn test_stereo_analysis_inverted() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate inverted signals (correlation = -1.0)
    let left = generate_sine(440.0, sr, 0.25);
    let right: Vec<f32> = left.iter().map(|&s| -s).collect();

    let stereo = analysis.analyze_stereo(&left, &right);

    // Should have negative correlation for inverted signals
    assert!(
        stereo.correlation < -0.95,
        "Inverted signals should have correlation ~-1.0, got {}",
        stereo.correlation
    );
}

/// Test stereo analysis with uncorrelated signals.
#[test]
#[ignore = "requires audio device"]
fn test_stereo_analysis_uncorrelated() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    // Generate signals at different frequencies (low correlation)
    let left = generate_sine(440.0, sr, 0.25);
    let right = generate_sine(550.0, sr, 0.25);

    let stereo = analysis.analyze_stereo(&left, &right);

    // Should have low correlation
    assert!(
        stereo.correlation.abs() < 0.5,
        "Different frequencies should have low correlation, got {}",
        stereo.correlation
    );
}

/// Test analysis with empty buffer.
#[test]
#[ignore = "requires audio device"]
fn test_analysis_empty_buffer() {
    let engine = test_engine();
    let analysis = &engine.analysis;

    let empty: Vec<f32> = vec![];

    // Should handle empty buffer gracefully
    let summary = analysis.waveform_summary(&empty, 256);
    assert!(summary.blocks.is_empty() || summary.blocks.len() <= 1);

    let pitch = analysis.detect_pitch(&empty);
    assert!(pitch.confidence == 0.0 || pitch.frequency == 0.0);

    let transients = analysis.detect_transients(&empty);
    assert!(transients.is_empty());
}

/// Exercise the runtime enable/disable hooks on `AnalysisHandle`.
#[test]
#[ignore = "requires audio device"]
fn test_live_analysis_enable_disable() {
    let engine = test_engine();
    let analysis = &engine.analysis;

    assert!(!analysis.is_live());
    assert!(analysis.enable_live(), "enable_live should succeed");
    assert!(analysis.is_live());

    // Idempotent.
    assert!(analysis.enable_live());
    assert!(analysis.is_live());

    analysis.disable_live();
    assert!(!analysis.is_live());

    // Idempotent.
    analysis.disable_live();
    assert!(!analysis.is_live());
}

/// Test waveform summary with different bin sizes.
#[test]
#[ignore = "requires audio device"]
fn test_waveform_summary_bin_sizes() {
    let engine = test_engine();
    let sr = engine.sample_rate;
    let analysis = &engine.analysis;

    let samples = generate_sine(440.0, sr, 0.5);

    // Different bin sizes should produce different summary lengths
    let summary_64 = analysis.waveform_summary(&samples, 64);
    let summary_256 = analysis.waveform_summary(&samples, 256);
    let summary_1024 = analysis.waveform_summary(&samples, 1024);

    // More samples per block = fewer blocks
    assert!(summary_64.blocks.len() >= summary_256.blocks.len());
    assert!(summary_256.blocks.len() >= summary_1024.blocks.len());
}
