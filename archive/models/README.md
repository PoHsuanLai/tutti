# Neural Models

This directory contains neural audio models for testing.

## Quick Start

```bash
# Create a test model
python create_test_model.py

# Convert to Burn format (if burn-import installed)
onnx2burn simple_synth.onnx simple_synth.mpk
```

## Requirements

- Python with PyTorch: `pip install torch onnxscript`
- burn-import (optional): `cargo install burn-import --features onnx`

## Test

```bash
cargo run --example neural_models --features neural
```
