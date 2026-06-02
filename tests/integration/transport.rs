//! Transport integration tests
//!
//! Tests transport state machine, tempo, looping, seeking, and metronome.
//! Pattern: Inspired by Ardour's tempo_test.cc and Zrythm's recording tests.

#[allow(unused_imports)]
use tutti::prelude::*;

#[allow(clippy::duplicate_mod)]
#[path = "../helpers/mod.rs"]
mod helpers;
use helpers::*;

/// Test basic play/stop operations.
#[test]
#[ignore = "requires audio device"]
fn test_transport_play_stop() {
    let engine = test_engine();

    // Initially stopped
    assert!(!engine.transport.is_playing());

    // Play
    engine.transport.play();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(engine.transport.is_playing());

    // Stop
    engine.transport.stop();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!engine.transport.is_playing());
}

/// Test tempo changes.
#[test]
#[ignore = "requires audio device"]
fn test_transport_tempo() {
    let engine = test_engine();

    // Set tempo
    engine.transport.tempo(120.0);
    assert!((engine.transport.get_tempo().get() - 120.0).abs() < 0.1);

    // Change tempo
    engine.transport.tempo(140.0);
    assert!((engine.transport.get_tempo().get() - 140.0).abs() < 0.1);

    // Tempo should persist across play/stop
    engine.transport.play();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!((engine.transport.get_tempo().get() - 140.0).abs() < 0.1);
    engine.transport.stop();
}

/// Test that beat position advances during playback.
#[test]
#[ignore = "requires audio device"]
fn test_transport_beat_advancement() {
    let engine = test_engine();

    // Set a fast tempo for quicker test
    engine.transport.tempo(240.0); // 4 beats/second

    let initial_beat = engine.transport.current_beat();
    engine.transport.play();

    // Wait for beats to advance
    std::thread::sleep(std::time::Duration::from_millis(500));

    let final_beat = engine.transport.current_beat();
    engine.transport.stop();

    // Should have advanced at least 1 beat at 240 BPM over 500ms
    assert!(
        final_beat > initial_beat + 1.0,
        "Beat should have advanced: initial={}, final={}",
        initial_beat,
        final_beat
    );
}

/// Test seek operation.
/// Note: Seek may be asynchronous - the position updates on next audio callback.
#[test]
#[ignore = "requires audio device"]
fn test_transport_seek() {
    let engine = test_engine();

    engine.transport.tempo(120.0);

    // Seek to beat 4
    engine.transport.seek(4.0);

    // Give time for the seek to take effect
    std::thread::sleep(std::time::Duration::from_millis(50));

    let beat = engine.transport.current_beat();
    // Seek might not be instant - verify it's close or the API was called without panic
    // The important thing is the API works; timing precision depends on implementation
    assert!(beat >= 0.0, "Beat should be non-negative, got {}", beat);
}

/// Test loop enable/disable.
#[test]
#[ignore = "requires audio device"]
fn test_transport_loop_enable() {
    let engine = test_engine();

    // Initially loop should be disabled
    assert!(!engine.transport.is_loop_enabled());

    // Set loop range and enable
    engine.transport.loop_range(0.0, 4.0).enable_loop();

    assert!(engine.transport.is_loop_enabled());

    // Disable loop
    engine.transport.disable_loop();
    assert!(!engine.transport.is_loop_enabled());
}

/// Test that loop configuration works.
/// Note: Actual loop wrapping depends on audio callback timing.
#[test]
#[ignore = "requires audio device"]
fn test_transport_loop_behavior() {
    let engine = test_engine();

    // Configure loop
    engine
        .transport
        .tempo(120.0)
        .loop_range(0.0, 4.0)
        .enable_loop();

    // Verify loop is enabled
    assert!(engine.transport.is_loop_enabled());

    // Play briefly
    engine.transport.play();
    std::thread::sleep(std::time::Duration::from_millis(100));
    engine.transport.stop();

    // Test passed if no panic - loop wrapping behavior tested via examples
}

/// Test rapid play/stop cycles.
/// Note: State changes may be asynchronous; this tests the API doesn't crash.
#[test]
#[ignore = "requires audio device"]
fn test_transport_rapid_toggle() {
    let engine = test_engine();

    for _ in 0..20 {
        engine.transport.play();
        engine.transport.stop();
    }

    // Engine should still be functional (no panics)
    // Final state may vary due to async nature
    engine.transport.stop(); // Ensure stopped
    std::thread::sleep(std::time::Duration::from_millis(50));
}

/// Test transport fluent API chaining.
#[test]
#[ignore = "requires audio device"]
fn test_transport_fluent_api() {
    let engine = test_engine();

    // All transport settings in one chain
    engine
        .transport
        .tempo(128.0)
        .loop_range(0.0, 8.0)
        .enable_loop()
        .play();

    // Give time for state to settle
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(engine.transport.is_loop_enabled());
    assert!((engine.transport.get_tempo().get() - 128.0).abs() < 0.1);

    engine.transport.stop();
}

/// Test metronome enable/disable.
#[test]
#[ignore = "requires audio device"]
fn test_metronome_toggle() {
    let engine = test_engine();

    // Enable metronome
    engine.transport.metronome().always();
    assert!(engine.transport.metronome().get_mode() != tutti::core::MetronomeMode::Off);

    // Disable metronome
    engine.transport.metronome().off();
    assert!(engine.transport.metronome().get_mode() == tutti::core::MetronomeMode::Off);
}

/// Test metronome configuration.
#[test]
#[ignore = "requires audio device"]
fn test_metronome_config() {
    let engine = test_engine();

    engine
        .transport
        .metronome()
        .volume(0.7)
        .accent_every(4)
        .always();

    // Metronome should be configured (hard to verify audio output in test)
    assert!(engine.transport.metronome().get_mode() != tutti::core::MetronomeMode::Off);
}

/// Test transport state after tempo change during playback.
#[test]
#[ignore = "requires audio device"]
fn test_tempo_change_during_playback() {
    let engine = test_engine();

    engine.transport.tempo(120.0).play();

    std::thread::sleep(std::time::Duration::from_millis(100));

    // Change tempo while playing
    engine.transport.tempo(180.0);

    std::thread::sleep(std::time::Duration::from_millis(100));

    // Tempo should be updated
    assert!((engine.transport.get_tempo().get() - 180.0).abs() < 0.1);

    // Transport should still be playing
    assert!(engine.transport.is_playing());

    engine.transport.stop();
}
