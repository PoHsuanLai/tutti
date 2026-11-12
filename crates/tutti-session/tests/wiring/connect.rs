//! `Graph::connect` and `Graph::chain`: a whole main bus per call, adding
//! sources and refusing a width change.
//!
//! Every figure is analytic: constants of exactly representable values.

use tutti_graph::NodeKey;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{ConnectError, OutPort, Root, Session};
use tutti_types::{ChannelLayout, InterleavedMut, MaxBlock, SampleRate, Samples};

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

fn feed_root(s: &mut Session, node: NodeKey) {
    for c in 0..2 {
        s.graph()
            .set_sources(Root::output(c), [OutPort::new(node, c)])
            .unwrap();
    }
}

/// **`connect` wires every channel**: a stereo constant of (0.25, 0.5) through
/// a stereo pass-through is heard as (0.25, 0.5), with one call and no
/// per-channel wiring.
#[test]
fn connect_wires_each_channel_to_its_own() {
    let mut s = offline();
    let (src, _) = s.graph().insert(Const::frame(&[0.25, 0.5]));
    let (through, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    s.graph().connect(src, through).expect("both are stereo");
    feed_root(&mut s, through.erase());
    assert_eq!(first_frame(&mut s), [0.25, 0.5]);
}

/// **`chain` connects each node to the next**: source, two pass-throughs.
#[test]
fn chain_connects_each_node_to_the_next() {
    let mut s = offline();
    let (src, _) = s.graph().insert(Const::frame(&[0.25, 0.5]));
    let (a, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    let (b, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    s.graph()
        .chain([src.erase(), a.erase(), b.erase()])
        .expect("all stereo");
    feed_root(&mut s, b.erase());
    assert_eq!(first_frame(&mut s), [0.25, 0.5]);
}

/// **`connect` adds**: a second connect into the same node sums with the
/// first; it replaces nothing.
#[test]
fn connect_adds_to_what_feeds_the_input() {
    let mut s = offline();
    let (one, _) = s.graph().insert(Const::frame(&[0.25, 0.125]));
    let (two, _) = s.graph().insert(Const::frame(&[0.5, 0.25]));
    let (sink, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    s.graph().connect(one, sink).unwrap();
    s.graph().connect(two, sink).unwrap();
    feed_root(&mut s, sink.erase());
    assert_eq!(first_frame(&mut s), [0.75, 0.375]);
}

/// **Mismatched main-bus widths are refused, and nothing is stored**: a mono
/// source into a stereo input is `ConnectError::Width`, never a truncation or
/// a zero fill; a chain refuses at its bad link with nothing wired before it.
#[test]
fn a_width_mismatch_is_refused_and_stores_nothing() {
    let mut s = offline();
    let (mono, _) = s.graph().insert(Const::mono(0.5));
    let (stereo, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    let (more, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    let (narrow, _) = s.graph().insert(Through::mono());
    let _ = s.update();
    let err = s.graph().connect(mono, stereo).unwrap_err();
    assert_eq!(
        err,
        ConnectError::Width {
            from: mono.erase(),
            from_width: 1,
            to: stereo.erase(),
            to_width: 2
        }
    );
    assert_eq!(
        s.graph().chain([stereo, more, narrow]).unwrap_err(),
        ConnectError::Width {
            from: more.erase(),
            from_width: 2,
            to: narrow.erase(),
            to_width: 1
        }
    );
    assert!(!s.graph().is_edited(), "nothing was wired");
}

/// **A node cannot feed itself, and an absent node is named.**
#[test]
fn a_node_that_is_not_there_or_feeds_itself_is_refused() {
    let mut s = offline();
    let (a, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    assert_eq!(
        s.graph().connect(a, a).err(),
        Some(ConnectError::Itself(a.erase()))
    );
    let (gone, _) = s.graph().insert(Through::new(ChannelLayout::STEREO));
    s.graph().remove(gone);
    assert_eq!(
        s.graph().connect(a, gone).err(),
        Some(ConnectError::NoSuchNode(gone.erase()))
    );
}
