//! Regression gate for `Engine::process` with the engine's own nodes in the
//! graph: timed transport commands, and the metronome, whose meter read is
//! the one non-scalar read on the audio thread.
//!
//! The metronome gate ran on a `Net` (a `TransportClock` feeding the click)
//! until doc 013 Phase 3 PR 15 removed the engine's `Net` backend; its graph
//! form, an `EnvClock` feeding the click with timed seeks, tempo and loop
//! changes on top, is `graph_engine_with_env_clock_and_metronome_is_allocation_free`.
//!
//! The gate over a chain of real DSP nodes (EQ, strip, limiter) is
//! `tutti-nodes`' `tests/rt_no_alloc_engine.rs`. It used to live here, built
//! from fundsp's filters; running it on the nodes the engine ships means
//! depending on `tutti-nodes`, which depends on this crate, so it moved there
//! rather than create a cycle.
//!
//! What this file deliberately does *not* test: that the audio thread never
//! *frees* a retired non-scalar value (a `MeterMap`, a routing table). That
//! property is carried by the type — `RtPublish::read` returns an `RtRef`, a
//! `!Send` borrow tied to the cell's lifetime, so there is no owning handle for
//! the callback to be left holding — and by `RtPublish`'s reclamation protocol,
//! whose reader side has no path to a destructor. It cannot be pinned by a
//! no-alloc gate here: the hazard is a race, and a sampling schedule cannot
//! exhaust one. A test that tried anyway lived here until it was removed — it
//! passed while asserting a property it structurally could not observe, which
//! reads as coverage and is worse than nothing. The race is covered where it
//! can be: the loom model `tutti-types/tests/rt_publish_loom.rs` (against the
//! shipped code; bounded in CI, exhaustive under `just loom-full`) and miri
//! over `rt::publish`'s stress test. (#34's
//! residual case — the old `ArcSwap` guard degrading into an owning reference —
//! is gone with the `ArcSwap`.)

use assert_no_alloc::AllocDisabler;
use std::sync::Arc;
use tutti_core::Engine;
use tutti_core::{
    ChannelLayout, ClickNode, ClickSettings, InterleavedMut, MetronomeMode, SampleRate, Transport,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A stereo source that reads the transport per frame from its `Env`, so
/// the gated loop below exercises `transport_at` and the executor's change
/// list, not only a render that ignores time.
struct TransportTone;

impl tutti_graph::Node for TransportTone {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_tail(tutti_core::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let env = *cx.env;
        let bumps = io.events(0).len() as f32;
        for k in env.offsets() {
            let t = env.transport_at(k);
            let v = if t.playing {
                t.beat().get().fract() as f32
            } else {
                0.0
            } + bumps;
            io.output(0)[k.index()] = v;
            io.output(1)[k.index()] = -v;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// `Engine::process` over the native graph, with timestamped transport
/// commands (play, seek, tempo, loop, a declick stop) scheduled and landing
/// inside the gate, and graph notes at beats landing in blocks with transport
/// changes: the walk, the change list, the executor and the fold never
/// allocate. Device blocks of 1 024 frames against a 512-frame `MaxBlock`,
/// so the split into graph blocks runs too.
///
/// Mutation (run): allocate a `Vec` at the top of `Engine::walk` → the gate
/// panics → fails.
#[test]
fn graph_engine_with_timed_transport_is_allocation_free() {
    use tutti_core::{At, Beat, Bpm, Frame, MotionEvent, TransportCommand};
    use tutti_graph::{Editor, EventIn, EventKind, Prepare, Ump, Unforkable};
    use tutti_types::graph::{OutPort, Source};
    use tutti_types::NodeKey;

    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(sample_rate),
        tutti_core::Samples(512),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    let mut output = vec![0.0f32; 1024 * 2];
    // Applying the commit allocates; that is the control side's price and
    // happens before the gate.
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    ed.collect();
    let to = EventIn {
        node: NodeKey(1),
        port: 0,
    };
    for b in 0..8 {
        ed.schedule(
            At::Beat(Beat(0.25 * b as f64)),
            to,
            EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0])),
        )
        .expect("room");
    }

    let m = &transport.motion;
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..20u64 {
            let base = 1024 * (1 + 8 * round);
            m.schedule(At::Frame(Frame(base + 300)), MotionEvent::Play)
                .expect("room");
            m.schedule(
                At::Frame(Frame(base + 1500)),
                TransportCommand::Tempo(Bpm(90.0 + round as f64)),
            )
            .expect("room");
            m.schedule(
                At::Frame(Frame(base + 2100)),
                TransportCommand::Loop(tutti_core::LoopRange::new(0.0, 4.0)),
            )
            .expect("room");
            m.schedule(At::Beat(Beat(0.5)), MotionEvent::locate(Beat(0.0)))
                .expect("room");
            m.schedule(At::Frame(Frame(base + 6000)), MotionEvent::stop())
                .expect("room");
            m.schedule(At::Frame(Frame(base + 7000)), TransportCommand::Loop(None))
                .expect("room");
            for _ in 0..8 {
                engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
            }
            m.cancel_scheduled();
        }
    });
    // The engine really ran every block, through its own clock.
    assert_eq!(transport.settings.steady_time(), 1024 * (1 + 20 * 8));
}

/// The metronome on the native graph: an `EnvClock` feeding `ClickNode`
/// (through `Legacy`), with timestamped seeks, tempo and loop changes
/// landing inside blocks, so `EnvClock` walks several segments per block.
/// Neither the clock's segment walk nor the click's meter read allocates.
///
/// The meter read goes through an [`RtPublish`]: a slot CAS, a fence and a
/// load, not an allocation — but that is a claim about another crate's
/// internals, so it is pinned here rather than reasoned about. The click
/// must be *sounding* for it to count: a gated-off metronome returns before
/// touching the meter, hence the `clicked` check.
///
/// [`RtPublish`]: tutti_types::RtPublish
///
/// Mutation (run): collect `env.segments()` into a `Vec` in
/// `EnvClock::process` → the gate panics → fails.
#[test]
fn graph_engine_with_env_clock_and_metronome_is_allocation_free() {
    use tutti_core::{At, Beat, Bpm, EnvClock, Frame, MotionEvent, TransportCommand};
    use tutti_graph::{Editor, Legacy, Prepare, Unforkable};
    use tutti_types::graph::{Edge, InPort, OutPort, Source};
    use tutti_types::NodeKey;

    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);
    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), sample_rate);

    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(sample_rate),
        tutti_core::Samples(512),
    ));
    let (clock, sink) = (NodeKey(1), NodeKey(2));
    ed.insert(clock, "clock", Unforkable(EnvClock::new()));
    ed.insert(sink, "click", Legacy::new(click));
    let topology = &mut ed.spec_mut().topology;
    for port in 0..2 {
        topology.edges.insert(
            InPort { node: sink, port },
            Edge::Direct(Source::Node(OutPort { node: clock, port })),
        );
    }
    topology.outputs = (0..2)
        .map(|port| Source::Node(OutPort { node: sink, port }))
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    let mut output = vec![0.0f32; 1024 * 2];
    // Applying the commit allocates; that is the control side's price.
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    ed.collect();
    let _ = transport.motion.try_send(MotionEvent::Play);

    let m = &transport.motion;
    let mut clicked = false;
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..20u64 {
            let base = 1024 * (1 + 8 * round);
            m.schedule(
                At::Frame(Frame(base + 300)),
                TransportCommand::Tempo(Bpm(110.0 + round as f64)),
            )
            .expect("room");
            m.schedule(
                At::Frame(Frame(base + 1500)),
                TransportCommand::Loop(tutti_core::LoopRange::new(0.0, 2.0)),
            )
            .expect("room");
            m.schedule(At::Beat(Beat(1.75)), MotionEvent::locate(Beat(0.9)))
                .expect("room");
            m.schedule(At::Frame(Frame(base + 7000)), TransportCommand::Loop(None))
                .expect("room");
            for _ in 0..8 {
                engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
                clicked |= output.iter().any(|&s| s != 0.0);
            }
            m.cancel_scheduled();
        }
    });
    // Not vacuous: the click sounded inside the gate.
    assert!(clicked, "the metronome clicked");
}
