//! # 28 - ONNX Runtime (Load at Runtime)
//!
//! Load a pre-trained `.onnx` model at runtime via ONNX Runtime + the
//! unified `Engine::load_model` API. Benchmarks throughput through the
//! engine's event channel.
//!
//! ```bash
//! cargo run --example 28_onnx_runtime --no-default-features --features "std,neural,ort"
//! ```

use std::sync::Arc;
use std::time::Instant;
use tutti_neural::{engine, Config, Request, Response, Shape};

fn main() -> tutti::Result<()> {
    println!("=== ONNX Runtime Model Loading ===\n");

    let cfg = Config::default();
    let sample_rate = cfg.sample_rate;
    let neural = engine(cfg, Box::new(tutti_ort::backend)).map_err(|e| tutti::Error::from(e))?;

    let onnx_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates/tutti-burn/src/model/tiny_effect.onnx");

    // Load + probe in one call.
    let loaded = neural.load_model(&onnx_path)?;
    println!("Loaded: id={:?}", loaded.id);
    println!("  latency:       {:?}", loaded.report.latency);
    println!(
        "  latency samples @ {} Hz: {}",
        sample_rate,
        loaded.report.latency_samples(sample_rate)
    );

    let input: Vec<f32> = (0..128).map(|i| (i as f32 / 128.0).sin()).collect();

    // Throughput test via the engine event channel.
    let iterations = 1000;
    let start = Instant::now();
    for _ in 0..iterations {
        let (r, w) = tutti_neural::slot(1, input.len());
        let _ = r; // keep reader alive through the request
        let req = Request {
            id: loaded.id,
            input: Arc::from(input.as_slice()),
            shape: Shape::new(1, input.len()),
            resp: Response::Audio(w),
        };
        let _ = tutti_neural::submit(&neural.event_sender(), req);
    }
    let total = start.elapsed();
    println!(
        "\nSubmitted {} requests in {:.1}ms ({:.1}µs/call submission path)",
        iterations,
        total.as_millis(),
        total.as_micros() as f64 / iterations as f64,
    );

    println!("\nDone.");
    Ok(())
}
