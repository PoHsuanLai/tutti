//! Regression gate for `Engine::process` over a real chain of **this crate's**
//! nodes.
//!
//! `tutti-core`'s `rt_no_alloc_engine` gates the engine with only its own nodes
//! in the graph (the beat clock, the metronome). This file wires an
//! oscillator → bell EQ → mixer strip → lookahead limiter chain into the graph
//! so the gate covers the per-buffer hot path through the nodes the engine
//! actually ships: `EqBandNode` (over `SvfFilterNode`), `BusStripNode` and
//! `LimiterNode`. A regression in any of those — or in the executor's walk,
//! or `Legacy`'s adapter around each — shows up here as an allocation panic.
//!
//! It lived in `tutti-core` while that chain was fundsp's (`sine_hz`, `pan`,
//! `bell_hz`, `limiter_stereo`), which gated fundsp's filters rather than ours.
//! Using ours means depending on this crate, and `tutti-nodes → tutti-core`
//! makes that a cycle from `tutti-core`'s side — so the test moved to the crate
//! whose nodes it exercises (design doc 013, Phase 0b).
//!
//! The `rt_no_alloc*` files beside this one gate each node in isolation; this
//! one gates them *inside the engine*, where the executor drives them through
//! its own buffers and the transport advances on the same `process` call.
//! (Until doc 013 Phase 3 PR 15 the chain was a `Net` with a transport clock
//! node; the engine renders only the native graph now, and drives its clock
//! itself.)

use assert_no_alloc::AllocDisabler;
use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{ChannelLayout, Db, Engine, Hz, InterleavedMut, NodeKey, SampleRate, Samples, Q};
use tutti_core::{MotionEvent, Transport};
use tutti_graph::{Editor, Legacy, Prepare};
use tutti_nodes::testing::Osc;
use tutti_nodes::{BusStripNode, EqBandNode, LimiterNode, SvfType};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build an [`Engine`] whose graph is `sine → bell EQ → strip → limiter`,
/// with the transport rolling so its advancement runs through the same
/// `process` path.
fn build_engine_with_chain() -> Engine {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(sample_rate), Samples(512)));
    let [osc, eq, strip, limiter] = [1, 2, 3, 4].map(NodeKey);
    ed.insert(osc, "osc", Legacy::new(Osc::sine(Hz(440.0))));
    ed.insert(
        eq,
        "eq",
        Legacy::new(EqBandNode::<f64>::new(
            SvfType::Bell,
            Hz(1_000.0),
            Q(1.0),
            Db(6.0),
        )),
    );
    ed.insert(
        strip,
        "strip",
        Legacy::new(BusStripNode::with_channels(ChannelLayout::STEREO)),
    );
    // The limiter's lookahead ring is sized at the prepared rate, on the
    // control thread, when the commit prepares it: outside the gate.
    ed.insert(
        limiter,
        "limiter",
        Legacy::new(LimiterNode::new(Db(-1.0), Db(-0.3))),
    );
    let topology = &mut ed.spec_mut().topology;
    let mut wire = |node: NodeKey, port: u16, from: NodeKey, out: u16| {
        topology.edges.insert(
            InPort { node, port },
            Edge::Direct(Source::Node(OutPort {
                node: from,
                port: out,
            })),
        );
    };
    wire(eq, 0, osc, 0);
    // The mono EQ's one output on both of the strip's inputs, the width
    // change fundsp's `pan` made in the old chain.
    wire(strip, 0, eq, 0);
    wire(strip, 1, eq, 0);
    wire(limiter, 0, strip, 0);
    wire(limiter, 1, strip, 1);
    topology.outputs = (0..2)
        .map(|port| {
            Source::Node(OutPort {
                node: limiter,
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    // Every commit is sent; the editor only had to outlive the setup.
    drop(ed);
    let _ = transport.motion.try_send(MotionEvent::Play);
    engine
}

#[test]
fn engine_process_real_chain_is_allocation_free() {
    let engine = build_engine_with_chain();

    let mut output = vec![0.0f32; 512 * 2];

    // Warm up outside the gate — prime any first-call state on the
    // limiter / svf filters and the transport clock.
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    }
    // The chain must actually sound, or the gate below walks no DSP. A +6 dB
    // bell off-centre and a -0.3 dB ceiling leave a 440 Hz sine well audible.
    // Mutation: a silent oscillator (`with_amplitude(Amplitude(0.0))`) fails
    // here rather than passing the gate over a graph that did no work.
    assert!(
        output.iter().any(|s| s.abs() > 0.1),
        "the chain rendered silence; the gate would prove nothing"
    );

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}

#[test]
fn engine_process_real_chain_small_buffer_is_allocation_free() {
    // Small buffers stress the per-buffer setup overhead.
    let engine = build_engine_with_chain();

    let mut output = vec![0.0f32; 64 * 2];
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..5_000 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}
