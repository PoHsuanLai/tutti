# Test Reference Files

This directory contains reference audio files for regression testing.

## Philosophy

We use a **hybrid approach**:
- **Simple signals** (sine, impulse, staircase, silence) are generated programmatically in test code
- **Complex DSP chain references** are stored as committed `.wav` files for regression testing

## Directory Structure

```
data/
├── README.md           # This file
└── regression/         # Complex DSP chain reference files
    └── *.wav           # Version-tagged regression files
```

## Regenerating Reference Files

If DSP algorithms intentionally change, regenerate reference files:

```rust
use helpers::{save_reference_wav, generate_sine};

// In a test or script:
let engine = test_engine();
engine.graph_mut(|net| {
    // Your DSP chain here
});

let (left, right, sr) = engine.export()
    .duration_seconds(1.0)
    .render()
    .unwrap();

save_reference_wav("regression/your_test_v0_2_0.wav", &left, &right, sr as u32).unwrap();
```

## Tolerance Levels

When comparing against reference files, use appropriate tolerances:

| Tolerance | Value | Use Case |
|-----------|-------|----------|
| `FLOAT_EPSILON` | 1e-6 | Exact operations (passthrough) |
| `DSP_EPSILON` | 1e-4 | DSP processing (filters, oscillators) |
| `PERCEPTUAL_EPSILON` | 0.001 | Perceptual equivalence |
| `SILENCE_THRESHOLD` | 0.0001 | Silence detection |
