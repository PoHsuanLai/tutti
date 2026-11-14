//! `Graph::open_inlet`: audio a thread of your own pushes into the graph, the
//! mirror of the tap.

use tutti_buffers::InterleavedMut;
use tutti_graph::InletRing;
use tutti_session::{InletError, OutPort, Root, Session};
use tutti_types::{ChannelLayout, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;

fn offline() -> Session {
    Session::builder()
        .sample_rate(RATE)
        .build()
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
    s.graph()
        .set_sources_hard(Root::output(0), [OutPort::new(key, 0)])
        .unwrap();
    s.graph()
        .set_sources_hard(Root::output(1), [OutPort::new(key, 1)])
        .unwrap();
    let mut tap = s
        .graph()
        .taps()
        .open_output(Samples(1_024))
        .expect("the sources exist");
    let _ = s.update();

    producer.push_interleaved(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    let mut block = vec![0.0f32; 8 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());

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
    let _ = s.update();
    let err = s
        .graph()
        .open_inlet(ChannelLayout::STEREO, Samples(256), SampleRate::SR_44K1)
        .expect_err("an inlet does not resample");
    assert_eq!(
        err,
        InletError::RateMismatch {
            producer: SampleRate::SR_44K1,
            graph: RATE
        }
    );
    assert!(
        !s.graph().is_edited(),
        "a refused inlet leaves the graph as it was"
    );
}

/// **A ring carries the rate its producer fills it at**, and `add_inlet`
/// checks it against the graph's, with no rate to give apart from the ring:
/// one made at another rate is refused with both rates and adds no node, and
/// one at the graph's is added.
#[test]
fn add_inlet_checks_the_rate_the_ring_carries() {
    let mut s = offline();
    let _ = s.update();
    let (ring, _writer) = InletRing::pair(ChannelLayout::STEREO, Samples(64), SampleRate::SR_44K1);
    assert_eq!(
        s.graph().add_inlet(ring).expect_err("another rate"),
        InletError::RateMismatch {
            producer: SampleRate::SR_44K1,
            graph: RATE
        }
    );
    assert!(!s.graph().is_edited(), "nothing was added");

    let (ring, _writer) = InletRing::pair(ChannelLayout::STEREO, Samples(64), RATE);
    s.graph().add_inlet(ring).expect("the graph's rate");
    assert!(s.graph().is_edited(), "the node was added");
}
