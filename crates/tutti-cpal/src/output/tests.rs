use super::*;
use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_graph::{Editor, Prepare};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_transport::{MotionEvent, Transport};
use tutti_types::{Beat, BeatDuration};
use tutti_types::{Hz, Q};
use tutti_types::{SampleRate, Samples};

/// Build an engine + transport pair whose graph actually renders.
///
/// An empty graph renders silence without running a node — which would
/// leave the transport assertions below reading a playhead nothing
/// exercised. A sine through a filter into the output gives the render
/// path real nodes to run, buffers to hand between them, and a fold to
/// the device width. The playhead is the engine's own clock; there is no
/// clock node in the graph.
///
/// `tests/rt_no_alloc.rs` mirrors this fixture, because the allocation
/// gates need a `#[global_allocator]` that only a test binary root can
/// declare.
fn build_callback_state(sample_rate: f64) -> (Transport, AudioCallbackState) {
    let transport = Transport::new(SampleRate::new(sample_rate));

    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(sample_rate), Samples(512)));
    let (source, filter) = (NodeKey(1), NodeKey(2));
    ed.insert(source, "sine", Osc::sine(Hz(220.0)));
    ed.insert(
        filter,
        "filter",
        SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(2_000.0), Q(0.7)),
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
    // The filter's single output on both device channels.
    ed.spec_mut().topology.outputs = vec![
        Source::Node(OutPort {
            node: filter,
            port: 0,
        });
        2
    ];
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let state = AudioCallbackState::new(engine);
    (transport, state)
}

#[test]
fn test_transport_advances_with_graph() {
    let sample_rate = 44100.0;
    let (transport, state) = build_callback_state(sample_rate);

    transport.settings.set_tempo(tutti_types::Bpm::of(120.0));
    let _ = transport.motion.try_send(MotionEvent::Play);

    let frames = 256;
    let mut output = vec![0.0f32; frames * 2];
    process_audio(
        &state,
        &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO),
    );

    let expected_beat = Beat(256.0 * (120.0 / 60.0) / 44100.0);
    let actual_beat = transport.settings.beat();
    assert!(
        (actual_beat - expected_beat).abs() < BeatDuration(1e-6),
        "expected {expected_beat:?}, got {actual_beat:?}"
    );
}

#[test]
fn test_transport_loop_wrapping() {
    let sample_rate = 44100.0;
    let (transport, state) = build_callback_state(sample_rate);

    transport.settings.set_tempo(tutti_types::Bpm::of(120.0));
    transport
        .motion
        .schedule(
            tutti_types::At::NextBlock,
            tutti_transport::LoopEdit::over(
                tutti_types::LoopRange::new(Beat::new(0.0), Beat::new(4.0)).expect("ordered"),
            ),
        )
        .expect("room");
    let _ = transport.motion.try_send(MotionEvent::Play);
    let _ = transport.motion.try_send(MotionEvent::locate(Beat(3.99)));

    let frames = 1024;
    let mut output = vec![0.0f32; frames * 2];
    process_audio(
        &state,
        &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO),
    );

    // Seeked to 3.99 at the first frame, rolling at 2 beats a second: one
    // wrap past 4.0, so the beat is 3.99 plus the block, less the loop's
    // length. No seek would leave it at the block's own length, and a wrap
    // counted twice or not at all would miss it.
    let beat = transport.settings.beat();
    let expected = Beat(3.99 + 1024.0 * (120.0 / 60.0) / 44100.0 - 4.0);
    assert!(
        (beat - expected).abs() < BeatDuration(1e-6),
        "expected {expected:?} after one wrap, got {beat:?}"
    );
}

fn manual_spec() -> OutputSpec {
    OutputSpec::new(
        SampleRate(48_000.0),
        tutti_types::ChannelLayout::STEREO,
        cpal::SampleFormat::F32,
    )
}

fn driver_over(engine: AudioEngine) -> crate::TuttiDriver {
    let transport = Transport::new(SampleRate::new(48_000.0));
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    let graph = Engine::new(&transport, &mut ed, exec).expect("an empty graph");
    crate::TuttiDriver::new(engine, Arc::new(AudioCallbackState::new(graph)))
}

/// **Naming the opened device asks the host nothing, and tells a supplied
/// driver from a device that would not be named.** The engine here has no
/// host at all, so any look at it would be an error: `Ok(Some)` is the name
/// the open recorded, `Ok(None)` a driver the caller supplied (no device, by
/// design), `Err` a device the host could not name. A start on a supplied
/// driver ends in `Ok(None)`.
///
/// Mutations (run): `opened_name` mapping `Unnamed` to `Ok(None)` fails the
/// `Err` assertion; mapping `Supplied` to `Err` fails the first assertion;
/// `set_opened(Supplied)` left out of `TuttiDriver::restart_on` fails the
/// last.
#[test]
fn the_opened_name_tells_a_supplied_driver_from_an_unnamed_device() {
    let mut engine = AudioEngine::from_spec(manual_spec());
    assert_eq!(engine.opened_name().unwrap(), None, "a supplied driver");
    engine.opened = Opened::Named("Card".into());
    assert_eq!(engine.opened_name().unwrap().as_deref(), Some("Card"));
    engine.opened = Opened::Unnamed("no name".into());
    assert!(engine.opened_name().is_err(), "a device that would not say");
    engine.opened = Opened::Named("Card".into());

    let mut driver = driver_over(engine);
    let (manual, _stream) = crate::ManualStreamDriver::new();
    driver
        .restart_on(manual_spec(), manual, |_, _| Ok::<(), crate::Error>(()))
        .unwrap();
    assert_eq!(driver.engine().opened_name().unwrap(), None);
}

/// **A restart that fails leaves the name of the device still open.** The
/// hook refuses after the new spec is in, before any stream starts; the name
/// is recorded only once the stream runs.
///
/// Mutation (run): `set_opened` moved ahead of `rerate_or_restore` in
/// `TuttiDriver::restart_on` makes the name `None` and fails this.
#[test]
fn a_failed_restart_keeps_the_opened_name() {
    let mut engine = AudioEngine::from_spec(manual_spec());
    engine.opened = Opened::Named("Card".into());
    let mut driver = driver_over(engine);
    let (manual, _stream) = crate::ManualStreamDriver::new();
    driver
        .restart_on(manual_spec(), manual, |_, _| {
            Err(crate::Error::InvalidConfig("refused".into()))
        })
        .expect_err("the hook refuses");
    assert_eq!(
        driver.engine().opened_name().unwrap().as_deref(),
        Some("Card")
    );
}

/// **Starting an engine whose stream runs is an error, and the running
/// stream is untouched.** Through `start_with` over a `ManualStreamDriver`
/// (`start` resolves a real device first, so it needs hardware; its guard is
/// the same `is_running` check, ahead of any device call). After `stop` a
/// start succeeds again.
///
/// Mutation (run): `start_with` returning `Ok(())` for a running engine →
/// the `expect_err` fails; dropping the guard → the second driver opens
/// (`second.is_open()`) and replaces the first.
#[test]
fn starting_a_running_engine_is_an_error() {
    let (_transport, state) = build_callback_state(44_100.0);
    let state = Arc::new(state);
    let mut engine = AudioEngine::from_spec(OutputSpec::new(
        SampleRate(44_100.0),
        tutti_types::ChannelLayout::STEREO,
        cpal::SampleFormat::F32,
    ));
    let (first, first_stream) = crate::ManualStreamDriver::new();
    engine
        .start_with(Arc::clone(&state), first)
        .expect("starts");
    assert!(engine.is_running() && first_stream.is_open());

    let (second, second_stream) = crate::ManualStreamDriver::new();
    let err = engine
        .start_with(Arc::clone(&state), second)
        .expect_err("a running engine refuses a second start");
    assert!(matches!(err, Error::AlreadyRunning), "{err}");
    assert!(first_stream.is_open(), "the running stream is untouched");
    assert!(!second_stream.is_open(), "the refused driver never opened");

    engine.stop();
    let (third, third_stream) = crate::ManualStreamDriver::new();
    engine.start_with(state, third).expect("starts after stop");
    assert!(third_stream.is_open());
}
