//! # 26 - Neural Workflow
//!
//! Recipe for going from a trained PyTorch model to a Tutti-hosted neural
//! effect. The tutti side of the workflow is path-based: export to `.onnx`
//! and hand the path to `engine.load_model`.
//!
//! ```bash
//! cargo run --example 26_neural_workflow --features neural,ort
//! ```

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Engine: ORT handles ONNX.
    let _engine = tutti_neural::engine(
        tutti_neural::Config::default(),
        Box::new(tutti_ort::backend),
    )?;

    println!("Neural workflow:");
    println!("  1. Train model (PyTorch).");
    println!("  2. Export: torch.onnx.export(model, input, \"model.onnx\")");
    println!("  3. Load: engine.load_model(Path::new(\"model.onnx\"))?");
    println!("  4. Inspect: loaded.report.{{latency, compatibility}}");
    println!("  5. Build node: engine.effect(loaded.id, 2, 512, latency_samples)");

    Ok(())
}
