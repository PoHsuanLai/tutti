//! # 27 - ONNX Model (Runtime Load)
//!
//! Load a `.onnx` model from disk via ORT. This replaces the older
//! compile-time `burn-import` codegen flow with the generic
//! `Engine::load_model` entry point.
//!
//! ```bash
//! cargo run --example 27_onnx_model --features neural,ort
//! ```

use std::path::PathBuf;
use tutti_neural::{engine, Config};

fn main() -> tutti::Result<()> {
    println!("=== ONNX Model Integration (Runtime Load) ===\n");

    let neural = engine(Config::default(), Box::new(tutti_ort::backend))
        .map_err(|e| tutti::Error::from(e))?;

    println!("Healthy: {}", neural.is_healthy());

    // Same tiny_effect.onnx file that burn-import used at build time.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/tutti-burn/src/model/tiny_effect.onnx");
    let loaded = neural.load_model(&path)?;

    println!("Loaded: id={:?}", loaded.id);
    println!("  latency:        {:?}", loaded.report.latency);
    println!("  compatibility:  {}", loaded.report.compatibility);

    println!("\nDone.");
    Ok(())
}
