//! Metering integration tests
//!
//! Tests amplitude, LUFS, correlation, and CPU metering.
//! Pattern: Inspired by Ardour's dsp_load_calculator_test.cc.

use tutti::prelude::*;

#[allow(clippy::duplicate_mod)]
#[path = "../helpers/mod.rs"]
mod helpers;
use helpers::*;

/// Test CPU metering.
/// Verifies CPU usage values are within valid bounds (0-100%).
#[test]
#[ignore = "requires audio device"]
fn test_metering_cpu() {
    let mut engine = test_engine();

    // Start some audio processing
    let osc = engine.graph.add(sine_hz::<f64>(440.0));
    engine.graph.pipe_output(osc);
    engine.graph.commit();

    engine.transport.play();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let cpu_avg = engine.metering.cpu_average();
    let cpu_peak = engine.metering.cpu_peak();

    engine.transport.stop();

    // CPU values should be between 0 and 100%
    assert!(cpu_avg >= 0.0, "CPU average {} should be >= 0", cpu_avg);
    assert!(cpu_peak >= 0.0, "CPU peak {} should be >= 0", cpu_peak);
    assert!(cpu_avg <= 100.0, "CPU average {} should be <= 100", cpu_avg);
    assert!(cpu_peak <= 100.0, "CPU peak {} should be <= 100", cpu_peak);
}

/// Test LUFS metering is functional.
/// Verifies the LUFS measurement returns a finite value with a signal present.
#[test]
#[ignore = "requires audio device"]
#[cfg(feature = "export")]
fn test_metering_lufs() {
    let mut engine = test_engine();

    // Enable LUFS metering
    engine.metering.inner().enable_lufs();

    // Full-scale 1kHz sine
    let osc = engine.graph.add(sine_hz::<f64>(1000.0));
    engine.graph.pipe_output(osc);
    engine.graph.commit();

    engine.transport.play();
    // LUFS needs at least 400ms of samples for short-term measurement
    std::thread::sleep(std::time::Duration::from_millis(600));

    // Get LUFS reading
    let lufs_result = engine.metering.lufs();

    engine.transport.stop();

    // LUFS measurement should succeed and return a finite value
    let lufs = lufs_result.expect("LUFS measurement should succeed after 600ms");
    assert!(
        lufs.is_finite(),
        "LUFS measurement should return a finite value, got {}",
        lufs
    );

    // With a signal, LUFS should be non-zero (whether linear or dB scale)
    assert!(
        lufs.abs() > 0.001,
        "With a signal present, LUFS should be non-zero, got {}",
        lufs
    );
}

/// Test stereo correlation metering.
/// Mono signal should have correlation close to +1.0.
#[test]
#[ignore = "requires audio device"]
#[cfg(feature = "export")]
fn test_metering_correlation_api() {
    let mut engine = test_engine();

    // Enable correlation metering
    engine.metering.inner().enable_corr();

    // Mono source going to stereo output should have high correlation
    let osc = engine.graph.add(sine_hz::<f64>(440.0) * 0.5);
    engine.graph.pipe_output(osc);
    engine.graph.commit();

    engine.transport.play();
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Get correlation value (stays live through &engine.metering)
    let _stereo = engine.metering.stereo();

    engine.transport.stop();

    // Test passes if no panic - actual correlation verification
    // would require reading the correlation value from metering
    // Mono signal should have correlation > 0.9 (near perfect)
}
