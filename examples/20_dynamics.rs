//! # 20 - Dynamics Processing
//!
//! Sidechain compression and gating with runtime atomic control.
//!
//! **Concepts:** Compressor, Gate, atomic runtime control
//!
//! ```bash
//! cargo run --example 20_dynamics --features dsp
//! ```

use std::sync::atomic::Ordering;
use std::time::Duration;
use tutti::prelude::*;
use tutti::units::{Compressor, Gate};

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    // --- Sidechain Compressor ---
    let comp = Compressor::mono(-20.0, 4.0, 0.001, 0.05)
        .with_soft_knee(6.0)
        .with_makeup(3.0);

    // Atomic runtime control via shared handles
    let threshold = comp.threshold();
    let ratio = comp.ratio();

    println!("Sidechain Compressor:");
    println!("  Threshold: {:.1} dB", threshold.load(Ordering::Acquire));
    println!("  Ratio:     {:.1}:1", ratio.load(Ordering::Acquire));

    // --- Sidechain Gate ---
    let gate = Gate::mono(-40.0, 0.001, 0.01, 0.1);

    println!("\nSidechain Gate:");
    println!(
        "  Threshold: {:.1} dB",
        gate.threshold().load(Ordering::Acquire)
    );

    // --- Audio graph ---
    // Source: sustained pad (input 0 of compressor)
    // Sidechain: kick-like pulse (input 1 of compressor)
    let pad = sine_hz::<f32>(220.0) * 0.6;
    let kick = sine_hz::<f32>(60.0) * 0.8;

    let pad_id = engine.graph.add(pad);
    let kick_id = engine.graph.add(kick);
    let comp_id = engine.graph.add(comp);

    // Wire: pad → comp input 0, kick → comp input 1 (sidechain)
    engine.graph.connect(pad_id, 0, comp_id, 0);
    engine.graph.connect(kick_id, 0, comp_id, 1);
    engine.graph.pipe_output(comp_id);
    engine.graph.commit();

    engine.transport.play();
    println!("\nPlaying: pad through sidechain compressor (kicked by 60 Hz)");

    // Adjust parameters at runtime via atomics
    std::thread::sleep(Duration::from_secs(2));
    println!("→ Lowering threshold to -30 dB, increasing ratio to 8:1");
    threshold.store(-30.0, Ordering::Release);
    ratio.store(8.0, Ordering::Release);

    std::thread::sleep(Duration::from_secs(2));
    println!("→ Heavy compression: threshold -40 dB, ratio 20:1");
    threshold.store(-40.0, Ordering::Release);
    ratio.store(20.0, Ordering::Release);

    std::thread::sleep(Duration::from_secs(2));
    println!("Done.");

    let _ = gate;
    Ok(())
}
