//! The two reverbs through the session: their params by address, indexed for the
//! FDN's three decays, and the tail each declares to the graph (the FDN's
//! unbounded where it can freeze).

use tutti_nodes::{FdnControls, FdnDesign, FdnNode, ReverbNode};
use tutti_session::Session;
use tutti_types::{ChannelLayout, SampleRate, Samples, Seconds, Tail, UnitAddr, UnitParam};

fn session() -> Session {
    Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(256))
        .expect("builds")
}

/// `Graph::set_param` at a decay's index reaches that decay's cell and no other,
/// and the freeze is addressable only where the design can freeze.
///
/// Mutation (run): registering `FdnNode` as a `SessionNode` without its params
/// (`impl SessionNode for FdnNode {}`) makes `set_param` a no-op and fails the
/// `low` assertion.
#[test]
fn set_param_reaches_the_decay_it_names() {
    let mut s = session();
    let (node, cells) = s.graph().add::<FdnNode>(ReverbNode::new(
        FdnDesign::new(8, Seconds(8.0), true).unwrap(),
        FdnControls::DEFAULT,
    ));
    let at = |i| UnitAddr::new(UnitParam::Decay, i);
    s.graph().set_param(node, at(1), 5.0);
    s.graph().set_param(node, UnitParam::Freeze, 1.0);
    assert_eq!(cells.get(at(1)), Some(5.0), "low");
    assert_eq!(cells.get(at(0)), Some(2.0), "mid untouched");
    assert_eq!(cells.get(at(2)), Some(1.2), "high untouched");
    assert_eq!(cells.get(UnitParam::Freeze), Some(1.0));

    let (plain, cells) = s
        .graph()
        .add::<FdnNode>(ReverbNode::new(FdnDesign::default(), FdnControls::DEFAULT));
    s.graph().set_param(plain, UnitParam::Freeze, 1.0);
    assert_eq!(cells.get(UnitParam::Freeze), None, "no freeze to write");
}

/// The tail each reverb declares to the graph: finite for a design that cannot
/// freeze (the room at its longest decay), unbounded for one that can.
///
/// Mutation (run): a finite ceiling for the freezable design fails the second
/// assertion.
#[test]
fn the_graph_sees_each_reverbs_ceiling() {
    let mut s = session();
    let (plain, _) = s
        .graph()
        .add::<FdnNode>(ReverbNode::new(FdnDesign::default(), FdnControls::DEFAULT));
    let (frozen, _) = s.graph().add::<FdnNode>(ReverbNode::new(
        FdnDesign::new(8, Seconds(8.0), true).unwrap(),
        FdnControls::DEFAULT,
    ));
    s.update();
    let Tail::Finite(n) = s.graph_ref().node_tail(plain) else {
        panic!("finite")
    };
    assert!(n.get() as f64 / 48_000.0 > 8.0);
    assert_eq!(s.graph_ref().node_tail(frozen), Tail::Unbounded);
}
