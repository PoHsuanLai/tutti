//! # 10 - Export
//!
//! Render audio to file (WAV, FLAC) and in-memory buffers, with optional
//! loudness normalization and a progress callback.
//!
//! **Concepts:** `Export`, `AudioFormat`, `Normalize`, progress callback
//!
//! ```bash
//! cargo run --example 10_export --features export
//! ```

use tutti::export::{Export, Normalize, Phase};
use tutti::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = TuttiEngine::builder().build()?;

    let c = sine_hz::<f64>(261.63) * 0.2;
    let e = sine_hz::<f64>(329.63) * 0.2;
    let g = sine_hz::<f64>(392.00) * 0.2;
    engine.graph.master((c + e + g) >> split::<U2>());
    engine.graph.commit();

    let sample_rate = engine.sample_rate;

    // WAV - format inferred from .wav extension.
    Export::graph(engine.graph.clone_net(), sample_rate)
        .duration_seconds(3.0)
        .to_file("/tmp/tutti_export_demo.wav")
        .run()?;
    println!("Exported: /tmp/tutti_export_demo.wav");

    // FLAC with loudness normalization.
    Export::graph(engine.graph.clone_net(), sample_rate)
        .duration_seconds(3.0)
        .normalize(Normalize::lufs(-14.0))
        .to_file("/tmp/tutti_export_demo.flac")
        .run()?;
    println!("Exported: /tmp/tutti_export_demo.flac (-14 LUFS)");

    // With progress callback.
    Export::graph(engine.graph.clone_net(), sample_rate)
        .duration_seconds(5.0)
        .compensate_latency(true)
        .to_file("/tmp/tutti_export_progress.wav")
        .run_with(|phase, progress| {
            let label = match phase {
                Phase::Render => "Render",
                Phase::Process => "Process",
                Phase::Encode => "Encode",
            };
            print!("\r{label} {:.0}%", progress * 100.0);
        })?;
    println!("\nExported: /tmp/tutti_export_progress.wav");

    // Render to memory.
    let rendered = Export::graph(engine.graph.clone_net(), sample_rate)
        .duration_seconds(1.0)
        .to_buffers()
        .run()?;
    println!(
        "Rendered: {} samples @ {} Hz",
        rendered.left.len(),
        rendered.sample_rate
    );

    Ok(())
}
