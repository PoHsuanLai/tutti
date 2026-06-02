//! Engine lifecycle integration tests
//!
//! Tests engine creation, configuration, subsystem initialization, and cleanup.
//! Pattern: Inspired by Ardour's session_test.h and Zrythm's ZrythmFixture.

use tutti::prelude::*;

#[allow(clippy::duplicate_mod)]
#[path = "../helpers/mod.rs"]
mod helpers;
use helpers::*;

/// Test engine creation with custom sample rate.
/// Note: The actual sample rate may differ from requested if the audio device
/// doesn't support it. This test verifies the engine reports a valid rate.
#[test]
#[ignore = "requires audio device"]
fn test_engine_custom_sample_rate() {
    let engine = TuttiEngine::builder().build().unwrap();

    // Engine should report a valid sample rate (common rates: 44100, 48000, 96000)
    let rate = engine.sample_rate;
    assert!(
        (8000.0..=192000.0).contains(&rate),
        "Sample rate {} is outside valid range",
        rate
    );
}

/// Test that multiple engines can be created sequentially.
/// (One at a time - audio devices are exclusive)
#[test]
#[ignore = "requires audio device"]
fn test_engine_sequential_creation() {
    for _i in 0..3 {
        let engine = TuttiEngine::builder().build().unwrap();

        assert!(engine.driver.is_running());
        // Engine is dropped here, releasing audio device
    }
}

/// Test that nodes can be created directly in the graph.
#[test]
#[ignore = "requires audio device"]
fn test_graph_node_creation() {
    let mut engine = test_engine();

    // Create nodes directly in graph
    let node1 = engine.graph.add(sine_hz::<f32>(440.0));
    let node2 = engine.graph.add(sine_hz::<f32>(880.0));
    engine.graph.commit();

    // Both should succeed and be different
    assert_ne!(node1, node2);
}

/// Test that graph() works properly.
#[test]
#[ignore = "requires audio device"]
fn test_graph_operations() {
    let mut engine = test_engine();

    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    engine.graph.pipe_output(osc);
    engine.graph.commit();

    assert!(engine.driver.is_running());
}
