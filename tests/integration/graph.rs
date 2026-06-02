//! Audio graph integration tests
//!
//! Tests DSP graph construction, node routing, and signal flow.
//! Pattern: Inspired by Ardour's audiographer tests and Zrythm's graph tests.

use tutti::prelude::*;

#[allow(clippy::duplicate_mod)]
#[path = "../helpers/mod.rs"]
mod helpers;
use helpers::*;

/// Test node creation directly in graph.
/// Verifies that each created node gets a unique ID.
#[test]
#[ignore = "requires audio device"]
fn test_graph_direct_nodes() {
    let mut engine = test_engine();

    // Create nodes directly in graph
    let osc1 = engine.graph.add(sine_hz::<f64>(220.0));
    let osc2 = engine.graph.add(sine_hz::<f64>(440.0));
    let filter = engine.graph.add(lowpole_hz(800.0));
    engine.graph.commit();

    // Verify different instances get unique IDs
    assert_ne!(osc1, osc2);
    assert_ne!(osc1, filter);
}

// Previously this test covered `NodeRegistry` + `engine.register(...)`, which
// has been removed in the flat-bundle API. Nodes are now instantiated directly
// and added via `engine.graph.add(...)`. The unique-id assertion is
// already covered by `test_graph_direct_nodes`, so the registry test is
// dropped entirely.

/// Test DSP nodes created directly.
/// Verifies LFO parameters can be set and instances are unique.
#[test]
#[ignore = "requires audio device"]
fn test_graph_dsp_lfo() {
    let mut engine = test_engine();

    use tutti::units::{LfoNode, LfoShape};

    let lfo1 = LfoNode::new(LfoShape::Sine).with_frequency(5.0);
    lfo1.set_depth(0.5);

    let lfo2 = LfoNode::new(LfoShape::Sine).with_frequency(5.0);
    lfo2.set_depth(0.8);

    let lfo1_id = engine.graph.add(lfo1);
    let lfo2_id = engine.graph.add(lfo2);
    engine.graph.commit();

    assert_ne!(lfo1_id, lfo2_id);
}

/// Test stereo split node routing.
#[test]
#[ignore = "requires audio device"]
fn test_graph_stereo_split() {
    let mut engine = test_engine();

    let mono = engine.graph.add(sine_hz::<f64>(440.0));
    // Reproduce `net.add_split()` via the fundsp split node (1 input, 2 outputs).
    let split = engine.graph.add(split::<U2>());
    let reverb = engine.graph.add(reverb_stereo(10.0, 2.0, 0.5));

    engine.graph.pipe_all(mono, split);
    engine.graph.pipe_all(split, reverb);
    engine.graph.pipe_output(reverb);
    engine.graph.commit();

    assert!(engine.driver.is_running());
}
