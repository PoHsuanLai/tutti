//! The wiring vocabulary on typed keys and ports (doc 013, "The session
//! graph's surface"): what `connect` takes and refuses, `Root`, a connection's
//! gain, and `disconnect`.
//!
//! Every figure is analytic: constants of exactly representable values.

use tutti_nodes::testing::{Const, Through};
use tutti_session::{ConnectError, Edge, EdgeGainError, OutPort, Root, Session};
use tutti_types::{Amplitude, ChannelLayout, InterleavedMut, MaxBlock, SampleRate, Samples};

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// The first frame of a committed 64-frame block, per channel.
fn first_frame(s: &mut Session) -> [f32; 2] {
    let _ = s.update();
    let mut block = vec![f32::NAN; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    [block[0], block[1]]
}

/// **A main bus connected to the root feeds its channels in order**: a stereo
/// constant (0.25, 0.5) is heard as (0.25, 0.5), and a mono one, narrower than
/// the root, feeds the first channel only and leaves the second silent (no
/// wrap, no zero-fill claim on the rest).
#[test]
fn a_main_bus_feeds_the_roots_channels_in_order() {
    let mut s = offline();
    let (stereo, _) = s.graph().insert(Const::frame(&[0.25, 0.5]));
    s.graph().connect(stereo, Root).unwrap().hard();
    assert_eq!(first_frame(&mut s), [0.25, 0.5]);

    let mut s = offline();
    let (mono, _) = s.graph().insert(Const::mono(0.5));
    s.graph().connect(mono, Root).unwrap().hard();
    assert_eq!(first_frame(&mut s), [0.5, 0.0]);
}

/// **A bus wider than the root is refused, never truncated**, and nothing is
/// stored.
#[test]
fn a_bus_wider_than_the_root_is_refused() {
    let mut s = offline();
    let (wide, _) = s.graph().insert(Const::frame(&[1.0, 2.0, 3.0]));
    let _ = s.update();
    let root_width = s.graph_ref().outputs();
    assert_eq!(
        s.graph().connect(wide, Root).err(),
        Some(ConnectError::RootNarrow {
            from: wide.erase(),
            from_width: 3,
            root_width,
        })
    );
    assert!(!s.graph_ref().is_edited(), "nothing was stored");
}

/// **A port that is not there is named and refused**: an output port past the
/// node's, an input past it, a root channel past the outputs; none is stored.
#[test]
fn a_port_that_is_not_there_is_refused() {
    let mut s = offline();
    let (src, _) = s.graph().insert(Const::mono(0.5));
    let (through, _) = s.graph().insert(Through::mono());
    let _ = s.update();
    let g = s.graph();
    assert_eq!(
        g.connect(src.out(1), through.input(0)).err(),
        Some(ConnectError::NoSuchOutPort(src.out(1)))
    );
    assert_eq!(
        g.connect(src.out(0), through.input(1)).err(),
        Some(ConnectError::NoSuchInPort(through.input(1)))
    );
    assert_eq!(
        g.connect(src.out(0), Root::output(9)).err(),
        Some(ConnectError::NoSuchInPort(Root::output(9)))
    );
    assert_eq!(
        g.connect(Root::input(3), through.input(0)).err(),
        Some(ConnectError::NoSuchOutPort(Root::input(3)))
    );
    assert!(!g.is_edited(), "nothing was stored");
}

/// **A connection's gain weights what the call added**: `0.5` of 0.5 is heard,
/// the edge reads back its gain, and a gain that is not a number takes the edge
/// the call added out again.
#[test]
fn a_connections_gain_weights_the_edge_it_added() {
    let mut s = offline();
    let (src, _) = s.graph().insert(Const::mono(0.5));
    s.graph()
        .connect(src.out(0), Root::output(0))
        .unwrap()
        .hard()
        .gain(Amplitude::new(0.5))
        .unwrap();
    assert_eq!(first_frame(&mut s), [0.25, 0.0]);
    let edge = Edge::new(src.out(0), Root::output(0));
    assert_eq!(edge.gain(s.graph_ref()), Some(Amplitude::new(0.5)));
    assert!(edge.clear_gain(s.graph()));
    assert_eq!(edge.gain(s.graph_ref()), None);
    assert_eq!(first_frame(&mut s), [0.5, 0.0]);

    let (other, _) = s.graph().insert(Const::mono(0.125));
    assert_eq!(
        s.graph()
            .connect(other.out(0), Root::output(1))
            .unwrap()
            .hard()
            .gain(Amplitude::new(f32::NAN))
            .err(),
        Some(EdgeGainError::NotFinite)
    );
    assert!(
        s.graph_ref().sources(Root::output(1)).is_empty(),
        "the edge the refused gain came with is gone"
    );
}

/// **`disconnect` takes out exactly the edge named**, says whether it was
/// there, and `disconnect_all` takes out the rest.
#[test]
fn disconnect_removes_one_edge_and_disconnect_all_the_rest() {
    let mut s = offline();
    let (a, _) = s.graph().insert(Const::mono(0.5));
    let (b, _) = s.graph().insert(Const::mono(0.25));
    let sink = Root::output(0);
    s.graph().connect(a.out(0), sink).unwrap().hard();
    s.graph().connect(b.out(0), sink).unwrap().hard();
    assert_eq!(first_frame(&mut s), [0.75, 0.0]);

    assert!(s.graph().disconnect_hard(a.out(0), sink));
    assert!(!s.graph().disconnect_hard(a.out(0), sink), "already gone");
    assert_eq!(s.graph_ref().sources(sink), [b.out(0)]);
    assert_eq!(first_frame(&mut s), [0.25, 0.0]);

    assert!(s.graph().disconnect_all(sink));
    assert!(s.graph_ref().sources(sink).is_empty());
    // The edge fades out over 256 frames: the fifth 64-frame block starts
    // past it.
    for _ in 0..4 {
        first_frame(&mut s);
    }
    assert_eq!(first_frame(&mut s), [0.0, 0.0]);
}

/// **A main bus disconnects as it connected**: channel by channel, leaving a
/// channel wired by hand alone.
#[test]
fn a_main_bus_disconnects_what_it_connected() {
    let mut s = offline();
    let (src, _) = s.graph().insert(Const::frame(&[0.25, 0.5]));
    let (through, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    s.graph().connect(src, through).unwrap().hard();
    assert_eq!(
        s.graph_ref().sources(through.input(1)),
        [src.out(1)],
        "channel by channel"
    );
    assert!(s.graph().disconnect_hard(src, through));
    assert!(s.graph_ref().sources(through.input(0)).is_empty());
    assert!(s.graph_ref().sources(through.input(1)).is_empty());
    assert!(
        !s.graph().disconnect_hard(src, through),
        "nothing left to take out"
    );
}

/// **A key and its erasure name the same node**: connecting both ways adds one
/// edge, and the second call changes nothing.
#[test]
fn a_key_and_its_erasure_are_one_node() {
    let mut s = offline();
    let (src, _) = s.graph().insert(Const::mono(0.5));
    s.graph().connect(src, Root).unwrap().hard();
    let _ = s.update();
    s.graph().connect(src.erase(), Root).unwrap().hard();
    s.graph()
        .connect(OutPort::new(src.erase(), 0), Root::output(0))
        .unwrap()
        .hard();
    assert!(!s.graph_ref().is_edited(), "the same edge, three spellings");
    assert_eq!(s.graph_ref().sources(Root::output(0)), [src.out(0)]);
}
