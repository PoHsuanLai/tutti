//! `Graph::open_inlet`: audio a thread of your own pushes into the graph, the
//! mirror of the tap.

use tutti_session::{GraphSource, InletError, Session};
use tutti_types::MaxBlock;
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

fn offline() -> Session {
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(1_024)))
        .expect("builds")
}

/// Frames pushed to an inlet come out of the graph's outputs in order, then
/// silence once the ring runs dry.
#[test]
fn pushed_frames_reach_the_outputs_in_order() {
    let mut s = offline();
    let (key, mut producer) = s
        .graph()
        .open_inlet(ChannelLayout::STEREO, Samples(256), RATE)
        .expect("the producer runs at the graph's rate");
    s.graph().set_output_source(0, GraphSource::Node(key, 0));
    s.graph().set_output_source(1, GraphSource::Node(key, 1));
    let mut tap = s
        .graph()
        .open_output_tap(Samples(1_024))
        .expect("the sources exist");
    s.update();

    producer.push_interleaved(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    let mut block = vec![0.0f32; 8 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));

    let mut got = vec![f32::NAN; 8 * 2];
    assert_eq!(tap.pop_into(&mut got), Samples(8));
    assert_eq!(
        got,
        [
            1.0, -1.0, 2.0, -2.0, 3.0, -3.0, //
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0
        ],
        "the three pushed frames, then silence for the frames the ring lacked"
    );
}

/// A producer at another rate is refused with a typed error carrying both
/// rates, and nothing is opened.
#[test]
fn a_producer_at_another_rate_is_refused() {
    let mut s = offline();
    s.update();
    let err = s
        .graph()
        .open_inlet(ChannelLayout::STEREO, Samples(256), SampleRate(44_100.0))
        .expect_err("an inlet does not resample");
    assert_eq!(
        err,
        InletError::RateMismatch {
            producer: SampleRate(44_100.0),
            graph: RATE
        }
    );
    assert!(
        !s.graph().is_edited(),
        "a refused inlet leaves the graph as it was"
    );
}
