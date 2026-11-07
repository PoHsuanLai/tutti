use super::*;
use tutti_engine::Engine;
use tutti_graph::{Editor, Prepare};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_transport::{MotionEvent, Transport};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, BeatDuration};
use tutti_types::{Hz, Q};
use tutti_types::{NodeKey, SampleRate, Samples};

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
    let transport = Transport::new(sample_rate);

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
    let state = AudioCallbackState::new(engine, MasterMeter::pair().0, AudioTap::new());
    (transport, state)
}

#[test]
fn test_transport_advances_with_graph() {
    let sample_rate = 44100.0;
    let (transport, state) = build_callback_state(sample_rate);

    transport.settings.set_tempo(120.0);
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

    transport.settings.set_tempo(120.0);
    transport
        .motion
        .schedule(
            tutti_types::At::NextBlock,
            tutti_transport::LoopEdit::over(
                tutti_types::LoopRange::new(0.0, 4.0).expect("ordered"),
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
