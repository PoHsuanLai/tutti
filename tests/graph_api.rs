//! `TuttiGraph` typed-API tests.
//!
//! Covers the introspection surface (`ids`, `len`, `inputs`, `outputs`,
//! `source`, `output_source`, `dot`) and the structural-edit surface
//! (`add`, `master`, `connect`, `pipe_all`, `pipe_output`, `remove`,
//! `commit`). Exercises the graph shape — no audio device needed since
//! `TuttiEngineBuilder::build()` opens CPAL, we gate these tests behind
//! `#[ignore]` for CI while still documenting intended behavior.

#![cfg(feature = "std")]

use tutti::core::Source;
use tutti::prelude::*;

fn engine() -> TuttiEngine {
    TuttiEngine::builder()
        .outputs(2)
        .build()
        .expect("engine build (needs audio device)")
}

#[test]
#[ignore = "requires audio device"]
fn empty_graph_has_builder_nodes_only() {
    let engine = engine();

    // Builder inserts the transport clock + the metronome click node, so the
    // graph is never truly "empty" post-build. Just sanity-check the count is
    // small and the introspection methods don't panic.
    let n = engine.graph.len();
    assert!(n >= 1, "builder should have inserted at least the clock");
    assert_eq!(engine.graph.len(), engine.graph.ids().count());
}

#[test]
#[ignore = "requires audio device"]
fn add_returns_unique_ids() {
    let mut engine = engine();

    let a = engine.graph.add(sine_hz::<f32>(440.0));
    let b = engine.graph.add(sine_hz::<f32>(880.0));
    let c = engine.graph.add(lowpass_hz::<f32>(1200.0, 1.0));
    engine.graph.commit();

    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
    assert!(engine.graph.contains(a));
    assert!(engine.graph.contains(b));
    assert!(engine.graph.contains(c));
}

#[test]
#[ignore = "requires audio device"]
fn inputs_outputs_reflect_node_arity() {
    let mut engine = engine();

    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    let filt = engine.graph.add(lowpass_hz::<f32>(1000.0, 1.0));

    assert_eq!(engine.graph.inputs(osc), 0, "sine has no inputs");
    assert_eq!(engine.graph.outputs(osc), 1, "sine is mono out");
    assert_eq!(engine.graph.inputs(filt), 1, "lowpass is mono in");
    assert_eq!(engine.graph.outputs(filt), 1, "lowpass is mono out");
}

#[test]
#[ignore = "requires audio device"]
fn connect_wires_a_source_into_a_dest_port() {
    let mut engine = engine();

    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    let filt = engine.graph.add(lowpass_hz::<f32>(1000.0, 1.0));

    engine.graph.connect(osc, 0, filt, 0);
    engine.graph.commit();

    match engine.graph.source(filt, 0) {
        Source::Local(src, src_port) => {
            assert_eq!(src, osc);
            assert_eq!(src_port, 0);
        }
        other => panic!("expected Local source, got {:?}", other),
    }
}

#[test]
#[ignore = "requires audio device"]
fn pipe_output_sets_global_output_source() {
    let mut engine = engine();
    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    engine.graph.pipe_output(osc);
    engine.graph.commit();

    for ch in 0..engine.graph.channels() {
        match engine.graph.output_source(ch) {
            Source::Local(src, _) => assert_eq!(src, osc),
            other => panic!("channel {ch}: expected Local source, got {other:?}"),
        }
    }
}

#[test]
#[ignore = "requires audio device"]
fn master_adds_and_pipes_in_one_call() {
    let mut engine = engine();
    let id = engine.graph.master(sine_hz::<f32>(440.0) * 0.5);
    engine.graph.commit();

    assert!(engine.graph.contains(id));
    for ch in 0..engine.graph.channels() {
        match engine.graph.output_source(ch) {
            Source::Local(src, _) => assert_eq!(src, id),
            other => panic!("channel {ch}: expected Local source, got {other:?}"),
        }
    }
}

#[test]
#[ignore = "requires audio device"]
fn remove_drops_node_from_graph() {
    let mut engine = engine();

    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    assert!(engine.graph.contains(osc));

    let _unit = engine.graph.remove(osc);
    engine.graph.commit();

    assert!(!engine.graph.contains(osc));
}

#[test]
#[ignore = "requires audio device"]
fn dot_output_is_valid_graphviz() {
    let mut engine = engine();
    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    let filt = engine.graph.add(lowpass_hz::<f32>(1000.0, 1.0));
    engine.graph.connect(osc, 0, filt, 0);
    engine.graph.pipe_output(filt);
    engine.graph.commit();

    let dot = engine.graph.dot().to_string();
    assert!(dot.starts_with("digraph tutti {"));
    assert!(dot.trim_end().ends_with('}'));
    assert!(dot.contains(&format!("n{}", osc.value())));
    assert!(dot.contains(&format!("n{}", filt.value())));
}

#[test]
#[ignore = "requires audio device"]
fn commit_returns_latency_informational() {
    let mut engine = engine();
    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    engine.graph.pipe_output(osc);

    // commit() returns total graph latency in samples. For a plain sine →
    // output with no PDC-declaring plugins there's no latency to compensate.
    let latency = engine.graph.commit();
    assert_eq!(latency, 0);
}
