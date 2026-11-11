//! Regression gate for `Engine::process` with the engine's own nodes in the
//! graph: timed transport commands, and the metronome, whose meter read is
//! the one non-scalar read on the audio thread.
//!
//! The metronome gate — the click beside a node walking its `Env`'s beat,
//! with timed seeks, tempo and loop changes on top — is
//! `graph_engine_with_env_clock_and_metronome_is_allocation_free`.
//!
//! The gate over a chain of real DSP nodes (EQ, strip, limiter) is
//! `tutti-nodes`' `tests/rt_no_alloc_engine.rs`: running it on the nodes the
//! engine ships means depending on `tutti-nodes`, which depends on this crate,
//! so it lives there rather than create a cycle.
//!
//! What this file deliberately does *not* test: that the audio thread never
//! *frees* a retired non-scalar value (a `MeterMap`, a routing table). That
//! property is carried by the type — `Reader::lend` returns a `Loan`, a
//! `!Send` borrow tied to the reader's lifetime, so there is no owning handle for
//! the callback to be left holding — and by `ArcLend`'s reclamation protocol,
//! whose reader side has no path to a destructor. It cannot be pinned by a
//! no-alloc gate here: the hazard is a race, and a sampling schedule cannot
//! exhaust one; a test that tried would pass while asserting a property it
//! structurally could not observe. The race is covered where it can be: the
//! loom model `arc-lend/tests/loom.rs` (against the shipped code; nine models:
//! eight exhaustive, one bounded at 4 in CI and 5 under `cargo xtask loom-full`)
//! and miri over `arc-lend`'s stress test.

mod support;

use assert_no_alloc::AllocDisabler;
use std::sync::Arc;
use tutti_engine::Engine;
use tutti_nodes::{ClickNode, ClickSettings, MetronomeMode};
use tutti_transport::{LoopEdit, Transport};
use tutti_types::LoopRange;
use tutti_types::{Amplitude, ChannelLayout, InterleavedMut, SampleRate};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A stereo source that reads the transport per frame from its `Env`, so
/// the gated loop below exercises `transport_at` and the executor's change
/// list, not only a render that ignores time.
struct TransportTone;

impl tutti_graph::Node for TransportTone {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_discrete(ChannelLayout::STEREO)
            .with_events(1, 0)
            .with_tail(tutti_types::Tail::Unbounded)
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

/// `Engine::process` over the graph, with timestamped transport
/// commands (play, seek, tempo, loop, a declick stop) scheduled and landing
/// inside the gate, and graph notes at beats landing in blocks with transport
/// changes: the walk, the change list, the executor and the fold never
/// allocate. Device blocks of 1 024 frames against a 512-frame `MaxBlock`,
/// so the split into graph blocks runs too.
///
/// Mutation (run): allocate a `Vec` at the top of `Engine::walk` → the gate
/// panics → fails; box a value in `LoopOwner::apply` (the loop edits publish
/// a snapshot) → fails.
#[test]
fn graph_engine_with_timed_transport_is_allocation_free() {
    use tutti_fixtures::midi::m1_note_on;
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, EventIn, EventKind, Prepare, Unforkable};
    use tutti_graph::{OutPort, Source};
    use tutti_transport::{MotionEvent, TransportCommand};
    use tutti_types::{At, Beat, Frame};
    use tutti_ump::{MidiChannel, NoteNumber, U7};

    let sample_rate = 48_000.0;
    let transport = Transport::new(SampleRate::new(sample_rate));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(sample_rate),
        tutti_types::Samples(512),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
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
            EventKind::Midi(m1_note_on(
                MidiChannel::FIRST,
                NoteNumber::of(60),
                U7::of(100),
            )),
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
                At::Frame(Frame(base + 2100)),
                TransportCommand::Loop(LoopEdit::over(
                    LoopRange::new(Beat::new(0.0), Beat::new(4.0)).expect("ordered"),
                )),
            )
            .expect("room");
            m.schedule(At::Beat(Beat(0.5)), MotionEvent::locate(Beat(0.0)))
                .expect("room");
            m.schedule(At::Frame(Frame(base + 6000)), MotionEvent::stop())
                .expect("room");
            m.schedule(
                At::Frame(Frame(base + 7000)),
                TransportCommand::Loop(LoopEdit::armed(false)),
            )
            .expect("room");
            // The snapshot publish: a bounds edit and a re-arm.
            m.schedule(
                At::Frame(Frame(base + 7500)),
                LoopEdit::bounds(LoopRange::new(Beat::new(1.0), Beat::new(3.0)).expect("ordered")),
            )
            .expect("room");
            m.schedule(At::Frame(Frame(base + 7600)), LoopEdit::armed(true))
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

/// A tempo map published between blocks reaches the engine's clock without
/// the audio thread allocating or freeing: the block lends the cell once, and
/// the maps it replaced are freed by the control side's sweep. The publish and
/// the sweep run outside the gate (they allocate and free, as they may); the
/// blocks run inside it, and the clock really takes each tempo (a lend that
/// never happened would pass by measuring nothing).
///
/// Mutation (run): a `Box::new(0u8)` in `TransportAudio::tempo_for_block`'s
/// lend branch -> the gate panics -> fails; the clock not taking the map (the
/// lend removed) -> `tempo_in_force` stays 120 -> the assertion fails.
#[test]
fn a_published_tempo_map_is_taken_without_allocating() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{OutPort, Source};
    use tutti_types::Bpm;

    let transport = Transport::new(SampleRate::new(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(48_000.0),
        tutti_types::Samples(512),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let mut output = vec![0.0f32; 1024 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    for round in 0..20u32 {
        let bpm = Bpm::of(80.0 + f64::from(round));
        transport.settings.set_tempo(bpm);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
            }
        });
        assert_eq!(transport.settings.tempo_in_force(), bpm, "round {round}");
        transport.settings.reclaim_tempo();
    }
}

/// A tempo map of steps followed on the audio thread: the walk lends the cell
/// per step (never held across a call), the look ahead and the cut blocks
/// (41 steps 24 frames apart, so a 512-frame graph block carries more than the
/// 8 changes it holds), loop wraps that re-seat on a step, a seek back across
/// the steps and a map swap all run inside the gate. The publishes run outside
/// it; the clock really takes the steps (`tempo_in_force` reaches the last
/// step's tempo), so a walk that never ran would not pass.
///
/// Mutation (run): a `Vec::with_capacity(1)` in `TempoWalk::roll` -> the gate
/// panics -> fails; a `Vec::with_capacity(1)` in `TransportAudio::tempo_ahead`
/// -> fails; the walk not running (`stepped` left false in `tempo_for_block`) -> the tempo stays
/// 120 -> the assertion fails.
///
/// The ramp case (doc 020, stage 4) runs the same calls over a ramp, whose
/// cells are a change each: the graph takes 4 096-frame blocks, so a call
/// crosses 32 cell edges and the engine cuts it at every 8th change; loop
/// wraps inside the ramp re-seat on a cell, and a seek back and a map swap
/// land in the middle of it. Its clock really takes the cells
/// (`tempo_in_force` is the chord of a ramp cell, strictly between the ends).
///
/// Mutation (run): a `Vec::with_capacity(1)` in `chord_cell_with` (reached
/// through `TempoWalk::frames_to_edge`) -> the gate panics -> fails; the ramp
/// not followed (`stepped` left false) -> `tempo_in_force` stays at the
/// scalar 120 -> the assertion fails.
#[test]
fn a_step_map_is_followed_without_allocating() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{OutPort, Source};
    use tutti_tempo::{Shape, TempoMap, TempoPoint};
    use tutti_transport::MotionEvent;
    use tutti_types::{At, Beat, Bpm};

    let dense = |last: f64| {
        let mut points: Vec<TempoPoint> = (0..41)
            .map(|i| TempoPoint {
                beat: Beat(f64::from(i) * 0.001),
                tempo: Bpm::of(120.0 + f64::from(i)),
                shape: Shape::Step,
            })
            .collect();
        points.push(TempoPoint {
            beat: Beat(1.0),
            tempo: Bpm::of(last),
            shape: Shape::Step,
        });
        Arc::new(TempoMap::new(points).expect("a map"))
    };
    let transport = Transport::new(SampleRate::new(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(48_000.0),
        tutti_types::Samples(512),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings.set_tempo_map(dense(90.0));
    transport
        .motion
        .try_send(MotionEvent::play_from(Beat(0.0)))
        .expect("room");
    let mut output = vec![0.0f32; 1024 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    // 1 024 frames: past all 41 steps (the last at frame 960).
    assert_eq!(transport.settings.tempo_in_force(), Bpm::of(160.0));
    for round in 0..10u32 {
        // Control side: a new map, a loop over the steps, a seek back.
        transport
            .settings
            .set_tempo_map(dense(80.0 + f64::from(round)));
        let m = &transport.motion;
        m.schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(0.0), Beat::new(0.02)).expect("ordered")),
        )
        .expect("room");
        m.try_send(MotionEvent::play_from(Beat(0.0))).expect("room");
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
            }
        });
        // Looping over beats 0 to 0.02 keeps the tempo among the first 21.
        let tempo = transport.settings.tempo_in_force().get();
        assert!((120.0..=140.0).contains(&tempo), "round {round}: {tempo}");
        transport.settings.reclaim_tempo();
    }

    // The ramp case: 120 to `end` BPM over 2 beats, linear in time.
    let ramp = |end: f64, shape: Shape| {
        Arc::new(
            TempoMap::new([
                TempoPoint {
                    beat: Beat(0.0),
                    tempo: Bpm::of(120.0),
                    shape,
                },
                TempoPoint {
                    beat: Beat(2.0),
                    tempo: Bpm::of(end),
                    shape: Shape::Step,
                },
            ])
            .expect("a map"),
        )
    };
    let transport = Transport::new(SampleRate::new(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(48_000.0),
        tutti_types::Samples(4_096),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport
        .settings
        .set_tempo_map(ramp(60.0, Shape::RampTime));
    transport
        .motion
        .try_send(MotionEvent::play_from(Beat(0.0)))
        .expect("room");
    let mut output = vec![0.0f32; 4_096 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    let slowed = transport.settings.tempo_in_force().get();
    assert!(
        (60.0..119.0).contains(&slowed),
        "a ramp cell's chord: {slowed}"
    );
    for round in 0..10u32 {
        let shape = if round % 2 == 0 {
            Shape::RampTime
        } else {
            Shape::RampBeats
        };
        transport
            .settings
            .set_tempo_map(ramp(50.0 + f64::from(round), shape));
        let m = &transport.motion;
        m.schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(0.1), Beat::new(0.3)).expect("ordered")),
        )
        .expect("room");
        m.try_send(MotionEvent::play_from(Beat(0.2))).expect("room");
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
            }
        });
        // Looping over beats 0.1 to 0.3 of a ramp that starts at 120 keeps the
        // chord between the ramp's tempo there and its start.
        let tempo = transport.settings.tempo_in_force().get();
        assert!((90.0..120.0).contains(&tempo), "round {round}: {tempo}");
        transport.settings.reclaim_tempo();
    }
}

/// The master meter on the audio thread: the meter op over a stereo and a
/// wide root never allocates, and the meter really reads (a gated-off meter
/// would pass by measuring nothing).
///
/// Mutation (run, recorded here): `Vec::with_capacity(1)` in `meter_op`
/// -> the gate panics.
#[test]
fn the_master_meter_is_allocation_free() {
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{MeterId, MeterSpec, NodeKey};
    use tutti_graph::{OutPort, Source};
    use tutti_transport::MotionEvent;

    for width in [2usize, 6] {
        let transport = Transport::new(SampleRate::new(48_000.0));
        let (mut ed, exec) = Editor::new(Prepare::new(
            SampleRate(48_000.0),
            tutti_types::Samples(512),
        ));
        ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
        ed.spec_mut().topology.set_outputs((0..2).map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        }));
        let mut meter = ed
            .open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
        ed.commit().expect("commits");
        let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
        meter.enable();
        let _ = transport.motion.try_send(MotionEvent::Play);

        let mut output = vec![0.0f32; 1024 * width];
        let layout = ChannelLayout::try_from(width).expect("a test width is small");
        engine.process(&mut InterleavedMut::new(&mut output, layout));
        ed.collect();
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..50 {
                engine.process(&mut InterleavedMut::new(&mut output, layout));
            }
        });
        assert!(
            meter.read()[0].peak.get() > 0.0,
            "width {width}: the meter read nothing"
        );
    }
}

/// The tap after the fader on the audio thread: writing (rings that fill and
/// count what they drop), taking an install, replacing the writer and taking a
/// close never allocate, and the writer a replace or close retires is sent
/// back, not freed (the engine enters `RtScope`, where a `TapWriter` drop is a
/// debug panic). The installs and closes are queued before each gate; the
/// engine takes them inside it.
///
/// Mutation (run, recorded here): `Vec::with_capacity(1)` in
/// `TapWriter::push_interleaved` -> the gate panics; dropping the replaced
/// writer in `Hearing::swap` -> the debug panic.
#[test]
fn the_heard_tap_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Editor, Prepare, Unforkable};
    use tutti_graph::{OutPort, Source};
    use tutti_transport::MotionEvent;

    let transport = Transport::new(SampleRate::new(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(48_000.0),
        tutti_types::Samples(512),
    ));
    ed.insert(NodeKey(1), "tone", Unforkable(TransportTone));
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let _ = transport.motion.try_send(MotionEvent::Play);
    let taps = engine.heard_taps();
    let mut output = vec![0.0f32; 1024 * 2];
    let layout = ChannelLayout::STEREO;
    engine.process(&mut InterleavedMut::new(&mut output, layout));
    ed.collect();

    // 16 frames of ring against 1024-frame blocks: every block overflows.
    let first = taps.open(layout, tutti_types::Samples(16)).expect("opens");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            engine.process(&mut InterleavedMut::new(&mut output, layout));
        }
    });
    drop(first);
    taps.reclaim();
    // The reclaim queued a close; a new open queues a replacement behind it.
    let second = taps
        .open(layout, tutti_types::Samples(4096))
        .expect("reopens");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            engine.process(&mut InterleavedMut::new(&mut output, layout));
        }
    });
    taps.close();
    assert_no_alloc::assert_no_alloc(|| {
        engine.process(&mut InterleavedMut::new(&mut output, layout));
    });
    taps.reclaim();
    assert_eq!(
        second.available().get(),
        4096,
        "the second tap filled its ring"
    );
    assert!(
        second.take_lost().get() > 0,
        "and counted what it could not hold"
    );
}

/// The metronome on the graph: `ClickNode` and a node walking its
/// block's beat (`Env::for_each_beat`, what `EnvClock` did: whole beats and
/// fraction to outputs 2 and 3) side by side, with timestamped seeks, tempo and loop changes landing
/// inside blocks, so both walk several segments per block. Neither segment
/// walk nor the click's meter read allocates.
///
/// The meter read goes through an [`ArcLend`]: a slot CAS, a fence and a
/// load, not an allocation — but that is a claim about another crate's
/// internals, so it is pinned here rather than reasoned about. The click
/// must be *sounding* for it to count: a gated-off metronome returns before
/// touching the meter, hence the `clicked` check.
///
/// [`ArcLend`]: tutti_rt::ArcLend
///
/// Mutations (run): collect `env.segments()` into a `Vec` in
/// `EnvClock::process` (the walk now in `Env::for_each_beat`), or in
/// `ClickNode::render` → the gate panics → fails.
#[test]
fn graph_engine_with_env_clock_and_metronome_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Unforkable};
    use tutti_graph::{OutPort, Source};
    use tutti_transport::{MotionEvent, TransportCommand};
    use tutti_types::Tail;
    use tutti_types::{At, Beat, Frame};

    /// The beat of every frame, whole beats then the fraction.
    struct BeatWalk;
    impl Node for BeatWalk {
        fn shape(&self) -> Shape {
            Shape::source_discrete(ChannelLayout::STEREO).with_tail(Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            cx.env.for_each_beat(|i, beat| {
                io.output(0)[i] = beat.floor().get() as f32;
                io.output(1)[i] = beat.fract().get() as f32;
            });
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    let sample_rate = 48_000.0;
    let transport = Transport::new(SampleRate::new(sample_rate));
    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(Amplitude::new(1.0));
    let click = ClickNode::new(&transport, Arc::clone(&settings));

    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(sample_rate),
        tutti_types::Samples(512),
    ));
    let (clock, sink) = (NodeKey(1), NodeKey(2));
    ed.insert(clock, "clock", Unforkable(BeatWalk));
    let _ = ed.insert(sink, "click", click);
    let topology = &mut ed.spec_mut().topology;
    topology.set_outputs(
        [(sink, 0), (sink, 1), (clock, 0), (clock, 1)]
            .into_iter()
            .map(|(node, port)| Source::Node(OutPort { node, port })),
    );
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    let four = ChannelLayout::from_count(4);
    let mut output = vec![0.0f32; 1024 * 4];
    // Applying the commit allocates; that is the control side's price.
    engine.process(&mut InterleavedMut::new(&mut output, four));
    ed.collect();
    let _ = transport.motion.try_send(MotionEvent::Play);

    let m = &transport.motion;
    let mut clicked = false;
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..20u64 {
            let base = 1024 * (1 + 8 * round);
            m.schedule(
                At::Frame(Frame(base + 1500)),
                TransportCommand::Loop(LoopEdit::over(
                    LoopRange::new(Beat::new(0.0), Beat::new(2.0)).expect("ordered"),
                )),
            )
            .expect("room");
            m.schedule(At::Beat(Beat(1.75)), MotionEvent::locate(Beat(0.9)))
                .expect("room");
            m.schedule(
                At::Frame(Frame(base + 7000)),
                TransportCommand::Loop(LoopEdit::armed(false)),
            )
            .expect("room");
            for _ in 0..8 {
                engine.process(&mut InterleavedMut::new(&mut output, four));
                // Channel 0 of each frame: the click's left.
                clicked |= output.iter().step_by(4).any(|&s| s != 0.0);
            }
            m.cancel_scheduled();
        }
    });
    // Not vacuous: the click sounded inside the gate.
    assert!(clicked, "the metronome clicked");
}

/// A [`WorkerPool`](tutti_graph::WorkerPool) whose every participant runs its
/// share of a job inside `assert_no_alloc` — the helper threads too, which a
/// gate around the callback alone would not see (`assert_no_alloc` is per
/// thread).
struct GatedPool(tutti_graph::WorkerPool);

impl tutti_graph::Pool for GatedPool {
    fn participants(&self) -> usize {
        self.0.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.0
            .run(wake, &|i| assert_no_alloc::assert_no_alloc(|| job(i)));
    }
    fn waits(&self) -> tutti_rt::BlockWaits {
        self.0.waits()
    }
}

/// The engine on the parallel executor, on the engine's own pool: a block's
/// tasks spread over four participants, the callback wakes parked helpers
/// (the pool's one system call), and neither the callback nor any helper
/// allocates or frees while a block runs. The helpers' idle loop between
/// blocks (spin, yield, park) runs outside the job and is not gated; it
/// touches only atomics and `Thread::park`.
///
/// Mutation: in `WorkerPool::run`, collect the sleepers to wake into a `Vec`
/// → the callback's gate aborts.
#[test]
fn graph_engine_on_a_worker_pool_is_allocation_free() {
    use tutti_graph::NodeKey;
    use tutti_graph::{Edge, InPort, OutPort, Source};
    use tutti_graph::{Editor, ForkByClone, Prepare};

    let sample_rate = 48_000.0;
    let transport = Transport::new(SampleRate::new(sample_rate));
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate(sample_rate),
        tutti_types::Samples(512),
    ));
    // Parks quickly, so the gated blocks below wake parked helpers.
    let pool = tutti_graph::WorkerPool::builder()
        .num_threads(4)
        .spin_budget(std::time::Duration::from_micros(1))
        .idle(std::time::Duration::from_micros(1))
        .build()
        .expect("a worker pool");
    exec.set_pool(Some(Arc::new(GatedPool(pool))));
    for k in 0..6u64 {
        ed.insert(
            NodeKey(k + 1),
            "sine",
            ForkByClone(support::Sine::new(tutti_types::Hz(220.0 * (k + 1) as f32))),
        );
        ed.insert(
            NodeKey(k + 11),
            "gain",
            ForkByClone(support::Gain(0.1 * (k + 1) as f32)),
        );
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(k + 11),
                port: 0,
            },
            vec![Edge::Direct(Source::Node(OutPort {
                node: NodeKey(k + 1),
                port: 0,
            }))],
        );
    }
    ed.spec_mut().topology.set_outputs(
        [11, 12]
            .map(|k| {
                Source::Node(OutPort {
                    node: NodeKey(k),
                    port: 0,
                })
            })
            .to_vec(),
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(exec.is_parallel(), "the plan spreads over the pool");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let mut output = vec![0.0f32; 512 * 2];
    engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
    ed.collect();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
    // Idle long enough for every helper to park, then render again: the
    // wake is gated too.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..20 {
            engine.process(&mut InterleavedMut::new(&mut output, ChannelLayout::STEREO));
        }
    });
    assert!(output.iter().any(|&x| x != 0.0), "the graph rendered");
}
