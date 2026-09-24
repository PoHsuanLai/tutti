//! Regression gate for `Engine::process` over a real chain of **this crate's**
//! nodes.
//!
//! `tutti-core`'s `rt_no_alloc_engine` gates the engine with only its own nodes
//! in the graph (the transport clock, the metronome). This file pushes an
//! oscillator → bell EQ → mixer strip → lookahead limiter chain into the net so
//! the gate covers the per-buffer hot path through the nodes the engine
//! actually ships: `EqBandNode` (over `SvfFilterNode`), `BusStripNode` and
//! `LimiterNode`. A regression in any of those — or in `Net`'s vertex
//! iteration — shows up here as an allocation panic.
//!
//! It lived in `tutti-core` while that chain was fundsp's (`sine_hz`, `pan`,
//! `bell_hz`, `limiter_stereo`), which gated fundsp's filters rather than ours.
//! Using ours means depending on this crate, and `tutti-nodes → tutti-core`
//! makes that a cycle from `tutti-core`'s side — so the test moved to the crate
//! whose nodes it exercises (design doc 013, Phase 0b).
//!
//! The `rt_no_alloc*` files beside this one gate each node in isolation; this
//! one gates them *inside the engine*, where `Net` drives them through its own
//! buffers and the transport advances on the same `process` call.

use assert_no_alloc::AllocDisabler;
use tutti_core::dsp::Net;
use tutti_core::{AudioUnit, ChannelLayout, Db, Engine, Hz, InterleavedMut, SampleRate, Q};
use tutti_core::{Transport, TransportClock};
use tutti_nodes::testing::Osc;
use tutti_nodes::{BusStripNode, EqBandNode, LimiterNode, SvfType};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build an [`Engine`] whose net is `sine → bell EQ → strip → limiter`.
/// The transport-clock node is also pushed so transport advancement runs
/// through the same `process` path.
fn build_engine_with_chain() -> Engine {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);

    let mut net = Net::new(0, 2);

    // Transport clock — matches what every real Engine sees.
    let clock = TransportClock::new(transport.clock_links(), sample_rate);
    net.push(Box::new(clock));

    // `chain` feeds each node from the previous one's outputs. The mono EQ's
    // one output is fanned to both of the strip's inputs, the width change
    // fundsp's `pan` made in the old chain.
    net.chain(Box::new(Osc::sine(Hz(440.0))));
    net.chain(Box::new(EqBandNode::<f64>::new(
        SvfType::Bell,
        Hz(1_000.0),
        Q(1.0),
        Db(6.0),
    )));
    net.chain(Box::new(BusStripNode::with_channels(ChannelLayout::STEREO)));
    net.chain(Box::new(LimiterNode::new(Db(-1.0), Db(-0.3))));

    // Outside the gate: the limiter's lookahead ring is sized at the placeholder
    // rate and reallocated here (see the crate docs on placeholder rates).
    net.set_sample_rate(SampleRate(sample_rate));
    let backend = net.backend();

    // The backend holds a pointer back into the net; keep it alive.
    let _keep: &'static Net = Box::leak(Box::new(net));

    Engine::new(transport.motion.clone(), backend)
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
