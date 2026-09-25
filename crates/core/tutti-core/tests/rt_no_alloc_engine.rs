//! Regression gate for `Engine::process` with the engine's own nodes in the
//! graph.
//!
//! The existing `process_audio_is_allocation_free` test in `tutti/src/audio_io.rs`
//! only exercises the bare `TransportClock` — no signal nodes. This file adds
//! the metronome, whose meter read is the one non-scalar read on the audio
//! thread.
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
use parking_lot::Mutex;
use std::sync::Arc;
use tutti_core::AudioUnit;
use tutti_core::Engine;
use tutti_core::{
    dsp::Net, ChannelLayout, ClickNode, ClickSettings, InterleavedMut, MetronomeMode, SampleRate,
    Transport, TransportClock,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The metronome reads its meter through an [`RtPublish`], which is the one
/// non-scalar read on the audio thread. That read is a slot CAS, a fence and a
/// load, not an allocation — but that is a claim about another crate's
/// internals, so it gets pinned here rather than reasoned about.
///
/// [`RtPublish`]: tutti_types::RtPublish
///
/// The click must be *enabled and sounding*: a gated-off metronome returns before
/// touching the meter at all, which would make this test prove nothing.
#[test]
fn engine_process_with_metronome_is_allocation_free() {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);

    let mut net = Net::new(0, 2);
    let clock = TransportClock::new(transport.clock_links(), sample_rate);
    let clock_id = net.push(Box::new(clock));

    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), sample_rate);
    let click_id = net.push(Box::new(click));
    // The click reads the beat per sample off the clock's two ports.
    net.connect(clock_id, 0, click_id, 0);
    net.connect(clock_id, 1, click_id, 1);
    net.pipe_output(click_id);

    net.set_sample_rate(SampleRate(sample_rate));
    let backend = net.backend();
    let _keep: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

    let engine = Engine::new(transport.motion.clone(), backend);

    // Roll the transport so the click is actually generating, not gated off.
    let _ = transport.motion.try_send(tutti_core::MotionEvent::Play);
    transport.motion.drain();

    let mut output = vec![0.0f32; 512 * 2];
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            // The rolling clock moves the beat the click reads — ~21 beats over
            // these ~10.7 s — so the retrigger path runs inside the gate rather
            // than only the steady state.
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
}

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
                t.beat.get().fract() as f32
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
    use tutti_graph::{Editor, EventIn, EventKind, Prepare, Ump};
    use tutti_types::graph::{OutPort, Source};
    use tutti_types::NodeKey;

    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(sample_rate),
        tutti_core::Samples(512),
    ));
    ed.insert(NodeKey(1), "tone", TransportTone);
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::with_graph(&transport, exec);

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
