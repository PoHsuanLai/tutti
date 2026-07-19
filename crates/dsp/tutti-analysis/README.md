# Tutti Analysis

Audio analysis: waveforms, transients, pitch, correlation.

## What this is

Analysis algorithms for DAW waveform displays and audio processing. Waveform thumbnails (multi-resolution min/max/RMS), transient/onset detection (spectral flux), pitch detection (YIN algorithm), and stereo correlation (phase, width, balance).

Operates on raw `&[f32]` buffers. Uses [rustfft](https://crates.io/crates/rustfft) for FFT operations.

## Quick Start

```rust
use tutti_analysis::{
    waveform::compute_summary,
    TransientDetector,
    PitchDetector,
    CorrelationMeter,
};

let samples: Vec<f32> = vec![0.0; 44100]; // 1 second of audio
let sample_rate = 44100.0;

// Waveform thumbnail
let summary = compute_summary(&samples, 1, 512);

// Transient detection
let mut detector = TransientDetector::new(sample_rate);
let transients = detector.detect(&samples);

// Pitch detection
let mut pitch_detector = PitchDetector::new(sample_rate);
let pitch = pitch_detector.detect(&samples);

// Stereo correlation
let left = &samples[..];
let right = &samples[..];
let mut meter = CorrelationMeter::new(sample_rate);
let analysis = meter.process(left, right);
```

## Features

- `cache` - LRU cache for waveform thumbnails
- `serialization` - Serde support for analysis results

## License

MIT OR Apache-2.0
