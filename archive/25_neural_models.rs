//! # 25 - Neural Models
//!
//! Load an ONNX model from disk, probe it for shape + latency, and show the
//! results. This is the primary API for neural inference in tutti.
//!
//! **Concepts:** `Engine::load_model`, `LoadedModel`, `ProbeReport`
//!
//! The probe's latency is the value the graph's PDC uses to align other
//! nodes around the model's constant processing delay. There is no hard
//! "realtime-safe" gate — machine-dependent probe timing shouldn't decide
//! whether a model is usable. Use `Meter::record_inference` for live xrun
//! detection instead.
//!
//! ```bash
//! cargo run --example 25_neural_models --features neural,ort
//! ```

use std::path::PathBuf;
use tutti_neural::{engine, Config};

fn main() -> tutti::Result<()> {
    // Start the engine against the ORT backend. ORT handles `.onnx`.
    let factory = Box::new(tutti_ort::backend);
    let cfg = Config::default();
    let sample_rate = cfg.sample_rate;
    let neural = engine(cfg, factory).map_err(|e| tutti::Error::from(e))?;

    println!("Healthy: {}", neural.is_healthy());

    // Load the bundled identity test model. Engine picks the backend by
    // extension (`.onnx` → ORT) and runs the probe.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/tutti-ort/src/test_data/identity.onnx");
    let loaded = neural.load_model(&path)?;

    println!("Loaded: id={:?}", loaded.id);
    println!("  input shape:    {:?}", loaded.report.input_shape);
    println!("  output shape:   {:?}", loaded.report.output_shape);
    println!("  latency:        {:?}", loaded.report.latency);
    println!(
        "  latency (samples @ {} Hz): {}",
        sample_rate,
        loaded.report.latency_samples(sample_rate)
    );
    println!("  compatibility:  {}", loaded.report.compatibility);

    Ok(())
}
