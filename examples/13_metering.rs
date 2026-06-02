//! # 13 - Metering
//!
//! Monitor audio levels: peak, RMS, LUFS loudness, stereo correlation.
//!
//! **Concepts:** `engine.metering`, amplitude, LUFS, stereo analysis, CPU meter
//!
//! ```bash
//! cargo run --example 13_metering
//! ```

use std::time::Duration;
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    // Enable meters on the underlying manager. `MeteringHandle` fluent
    // setters consume `self`, so reach through `.inner()` when you want to
    // keep the handle in place for later reads.
    let m = engine.metering.inner();
    m.enable_amp();
    m.enable_lufs();
    m.enable_corr();
    engine.metering.inner().cpu().enable();

    // Create a test signal: stereo sine with slight detune (creates movement)
    let left = sine_hz::<f64>(440.0) * 0.5;
    let right = sine_hz::<f64>(442.0) * 0.5; // Slight detune
    engine.graph.master(left | right);
    engine.graph.commit();

    engine.transport.play();
    println!("Monitoring levels...");

    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(500));

        let m = &engine.metering;

        // Get amplitude (peak and RMS)
        let (l_peak, r_peak, l_rms, r_rms) = m.amplitude();

        // Get stereo analysis
        let stereo = m.stereo();

        // Get LUFS (may not be ready immediately)
        let lufs = m.lufs_short().unwrap_or(-100.0);

        // Get CPU load
        let cpu = m.cpu_average();

        println!(
            "[{:2}] Peak L/R: {:5.2}/{:5.2} | RMS: {:5.2}/{:5.2} | LUFS: {:6.1} | Corr: {:5.2} | CPU: {:4.1}%",
            i,
            l_peak,
            r_peak,
            l_rms,
            r_rms,
            lufs,
            stereo.correlation,
            cpu
        );
    }

    // Final loudness summary
    let m = &engine.metering;
    if let Ok(global) = m.lufs() {
        print!("\nIntegrated: {:.1} LUFS", global);
    }
    if let Ok(range) = m.lufs_range() {
        print!(" | Range: {:.1} LU", range);
    }
    println!();

    Ok(())
}
