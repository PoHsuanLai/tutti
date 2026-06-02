//! # 21 - Spatial Audio
//!
//! Compare two spatial panning methods:
//! 1. **VBAP** (Vector Base Amplitude Panning) — amplitude-only, clean panning
//! 2. **Binaural** (ITD/ILD) — time + level differences, more 3D on headphones
//!
//! Each method does: hard left → hard right → center → continuous rotation.
//!
//! **Concepts:** `SpatialPannerNode`, `BinauralPannerNode`, VBAP, ITD/ILD, Arc-shared atomics
//!
//! ```bash
//! cargo run --example 21_spatial_audio --features dsp
//! ```

use std::time::Duration;
use tutti::prelude::*;
use tutti::units::{BinauralPannerNode, SpatialPannerNode};

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().outputs(2).build()?;

    println!("Put on headphones for best effect!\n");

    // ── Part 1: VBAP ──
    println!("=== VBAP (amplitude panning) ===\n");

    let vbap = SpatialPannerNode::stereo()?;
    let vbap_handle = vbap.clone();
    let source = sine_hz::<f32>(440.0) * 0.5;

    let src_id = engine.graph.add(source);
    let pan_id = engine.graph.add(vbap);
    engine.graph.connect(src_id, 0, pan_id, 0);
    engine.graph.connect(src_id, 0, pan_id, 1);
    engine.graph.pipe_output(pan_id);
    engine.graph.commit();

    engine.transport.play();
    run_spatial_test(&vbap_handle);

    // ── Part 2: Binaural ──
    // Replace the graph with a binaural panner.
    println!("\n=== Binaural (ITD/ILD) ===\n");

    let binaural = BinauralPannerNode::new(48000.0);
    let binaural_handle = binaural.clone();
    let source2 = sine_hz::<f32>(440.0) * 0.5;

    engine.graph.net_mut().reset();
    let src_id = engine.graph.add(source2);
    let pan_id = engine.graph.add(binaural);
    engine.graph.connect(src_id, 0, pan_id, 0);
    engine.graph.connect(src_id, 0, pan_id, 1);
    engine.graph.pipe_output(pan_id);
    engine.graph.commit();

    run_spatial_test(&binaural_handle);

    println!("Done.");
    Ok(())
}

trait SpatialHandle {
    fn set_position(&self, azimuth: f32, elevation: f32);
}

impl SpatialHandle for SpatialPannerNode {
    fn set_position(&self, azimuth: f32, elevation: f32) {
        SpatialPannerNode::set_position(self, azimuth, elevation);
    }
}

impl SpatialHandle for BinauralPannerNode {
    fn set_position(&self, azimuth: f32, elevation: f32) {
        BinauralPannerNode::set_position(self, azimuth, elevation);
    }
}

fn run_spatial_test(handle: &dyn SpatialHandle) {
    println!("Hard LEFT for 2 seconds...");
    handle.set_position(90.0, 0.0);
    std::thread::sleep(Duration::from_secs(2));

    println!("Hard RIGHT for 2 seconds...");
    handle.set_position(-90.0, 0.0);
    std::thread::sleep(Duration::from_secs(2));

    println!("CENTER for 2 seconds...");
    handle.set_position(0.0, 0.0);
    std::thread::sleep(Duration::from_secs(2));

    println!("Rotating (5 seconds)...");
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let t = start.elapsed().as_secs_f32();
        let azimuth = (t * 360.0) % 360.0 - 180.0;
        handle.set_position(azimuth, 0.0);
        std::thread::sleep(Duration::from_millis(20));
    }
}
