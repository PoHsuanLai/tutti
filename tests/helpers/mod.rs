//! Shared test helpers.
//!
//! Deliberately small — just the two primitives the integration tests need.

#![allow(dead_code)]

use tutti::prelude::*;

/// Construct a default test engine.
///
/// Opens the default CPAL device; tests that use this must be `#[ignore]`
/// in CI where no audio hardware is available.
pub fn test_engine() -> TuttiEngine {
    TuttiEngine::builder()
        .build()
        .expect("failed to create test engine (audio device required)")
}

/// Generate a sine wave: `frequency` Hz at `sample_rate` for `num_samples`.
pub fn generate_sine(frequency: f64, sample_rate: f64, num_samples: usize) -> Vec<f32> {
    (0..num_samples)
        .map(|i| {
            let t = i as f64 / sample_rate;
            (2.0 * std::f64::consts::PI * frequency * t).sin() as f32
        })
        .collect()
}
