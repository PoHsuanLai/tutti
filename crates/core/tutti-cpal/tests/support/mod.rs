//! Shared fixtures for this crate's integration tests.
//!
//! The rolling-graph fixture now exists in three places — `output.rs`'s
//! `build_callback_state`, `tests/rt_no_alloc.rs`'s `rolling_state`, and
//! whatever the next test binary needs. Each is a native graph (doc 013
//! Phase 3 PR 15: `Engine` renders nothing else); the nodes are `Legacy`
//! units, so the engine renders them chunk-major. Two copies were already acknowledged
//! in `rt_no_alloc.rs`'s header ("duplicated rather than shared because that
//! one is `#[cfg(test)]`-private"); three is the point at which the repo's own
//! escalation applies.
//!
//! A support module, not a dev-dependency crate: `tutti-fixture-resolve`
//! exists because *three separate crates* each carried a copy of the same
//! probe-path logic. Three copies inside one crate is a `tests/support/`.
//!
//! `rt_no_alloc.rs` still keeps its own copy, deliberately — it declares a
//! `#[global_allocator]`, and every line it runs before the gate has to be
//! auditable in one file.

#![allow(dead_code)]
// Each integration-test binary compiles this tree separately, and no single
// binary uses every item. Same reason, and same allow, as the plugin hosts'
// `tests/support/mod.rs`.

use std::sync::Arc;
use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{
    AudioTap, ChannelLayout, Engine, Hz, MasterMeter, NodeKey, SampleRate, Samples, Q,
};
use tutti_core::{MotionEvent, Transport};
use tutti_cpal::{AudioCallbackState, OutputSpec};
use tutti_graph::{Editor, Legacy, Prepare};
use tutti_nodes::testing::{Const, Osc};
use tutti_nodes::{SvfFilterNode, SvfType};

pub const SAMPLE_RATE: f64 = 48_000.0;

/// An engine over `transport` rendering the native graph `build` wires into
/// a fresh editor (prepared for 512-frame blocks at [`SAMPLE_RATE`]). The
/// editor is leaked: a test process is the whole lifetime, and nothing here
/// commits again.
pub fn graph_engine(transport: &Transport, build: impl FnOnce(&mut Editor)) -> Engine {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SAMPLE_RATE), Samples(512)));
    build(&mut ed);
    ed.commit().expect("commits");
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
    Box::leak(Box::new(ed));
    engine
}

/// Every one of `outputs` global outputs reads `node`'s output 0.
pub fn fan(ed: &mut Editor, node: NodeKey, outputs: usize) {
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node, port: 0 }); outputs];
}

/// A rolling transport driving a sine through a filter, at `outputs` width.
///
/// The graph has to actually render: a graph with nothing wired renders
/// silence, and any assertion over it is vacuous.
pub fn rolling_state(outputs: usize) -> (Transport, Arc<AudioCallbackState>) {
    let transport = Transport::new(SAMPLE_RATE);
    let engine = graph_engine(&transport, |ed| {
        let (source, filter) = (NodeKey(1), NodeKey(2));
        ed.insert(source, "sine", Legacy::new(Osc::sine(Hz(220.0))));
        ed.insert(
            filter,
            "filter",
            Legacy::new(SvfFilterNode::<f64>::new(
                SvfType::LowPass,
                Hz(2_000.0),
                Q(0.7),
            )),
        );
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: filter,
                port: 0,
            },
            Edge::Direct(Source::Node(OutPort {
                node: source,
                port: 0,
            })),
        );
        fan(ed, filter, outputs);
    });
    let state = AudioCallbackState::new(engine, MasterMeter::new(), AudioTap::new());

    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    transport.motion.drain();

    (transport, Arc::new(state))
}

/// A graph emitting a constant `level` on **every** output channel.
///
/// DC rather than a tone, for the reason `tutti-export`'s `dc_net` gives: every
/// frame carries the same known value, so an expected result can be computed
/// in closed form instead of sampled. That is what makes the sample-format
/// matrix assertable.
pub fn dc_state(level: f32, outputs: usize) -> Arc<AudioCallbackState> {
    let engine = graph_engine(&Transport::new(SAMPLE_RATE), |ed| {
        ed.insert(NodeKey(1), "dc", Legacy::new(Const::mono(level)));
        fan(ed, NodeKey(1), outputs);
    });
    Arc::new(AudioCallbackState::new(
        engine,
        MasterMeter::new(),
        AudioTap::new(),
    ))
}

/// A graph emitting `level` on exactly one output channel, silence on the
/// rest, with the master tap already open.
///
/// This is what distinguishes a *fold* from a truncation: put the signal on a
/// channel outside the front pair and a `[..2]` metering shortcut sees
/// silence while a real fold does not. A uniform-DC graph cannot tell the two
/// apart, because every channel carries the same value.
pub fn dc_on_channel(
    level: f32,
    channel: usize,
    outputs: usize,
) -> (Arc<AudioCallbackState>, tutti_core::TapCons) {
    let engine = graph_engine(&Transport::new(SAMPLE_RATE), |ed| {
        ed.insert(NodeKey(1), "dc", Legacy::new(Const::mono(level)));
        ed.spec_mut().topology.outputs = (0..outputs)
            .map(|ch| {
                if ch == channel {
                    Source::Node(OutPort {
                        node: NodeKey(1),
                        port: 0,
                    })
                } else {
                    Source::Zero
                }
            })
            .collect();
    });
    let tap = AudioTap::new();
    let cons = tap.open().expect("a fresh tap opens");
    let state = AudioCallbackState::new(engine, MasterMeter::new(), tap);
    (Arc::new(state), cons)
}

/// A spec with no device behind it, at `channels` width and `format`.
pub fn spec(channels: usize, format: cpal::SampleFormat) -> OutputSpec {
    OutputSpec::new(
        SampleRate(SAMPLE_RATE),
        ChannelLayout::from(channels),
        format,
    )
}
