//! The two reverbs through the session: their params by address, indexed for the
//! FDN's three decays, and the tail each declares to the graph (the FDN's
//! unbounded where it can freeze).

use tutti_nodes::{DesignNode, FdnControls, FdnDesign, FdnNode};
use tutti_session::{ParamError, Session};
use tutti_types::MaxBlock;
use tutti_types::{ChannelLayout, SampleRate, Samples, Seconds, Tail, UnitAddr, UnitParam};

fn session() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(256)),
        )
        .expect("builds")
}

/// `Graph::set_param` at a decay's index reaches that decay's cell and no other,
/// and the freeze is addressable only where the design can freeze.
#[test]
fn set_param_reaches_the_decay_it_names() {
    let mut s = session();
    let (node, cells) = s.graph().add::<FdnNode>(DesignNode::live(
        FdnDesign::new(8, Seconds(8.0), true).unwrap(),
        FdnControls::DEFAULT,
    ));
    let at = |i| UnitAddr::new(UnitParam::Decay, i);
    s.graph().set_param(node.decay_at(1), Seconds(5.0)).unwrap();
    s.graph().set_param(node.freeze(), true).unwrap();
    assert_eq!(cells.get(at(1)), Some(5.0), "low");
    assert_eq!(cells.get(at(0)), Some(2.0), "mid untouched");
    assert_eq!(cells.get(at(2)), Some(1.2), "high untouched");
    assert_eq!(cells.get(UnitParam::Freeze), Some(1.0));

    let (plain, cells) = s
        .graph()
        .add::<FdnNode>(DesignNode::live(FdnDesign::default(), FdnControls::DEFAULT));
    assert!(matches!(
        s.graph().set_param(plain.freeze(), true),
        Err(ParamError::NoSuchParam { .. })
    ));
    assert_eq!(cells.get(UnitParam::Freeze), None, "no freeze to write");
}

/// The tail each reverb declares to the graph: finite for a design that cannot
/// freeze (the room at its longest decay), unbounded for one that can.
#[test]
fn the_graph_sees_each_reverbs_ceiling() {
    let mut s = session();
    let (plain, _) = s
        .graph()
        .add::<FdnNode>(DesignNode::live(FdnDesign::default(), FdnControls::DEFAULT));
    let (frozen, _) = s.graph().add::<FdnNode>(DesignNode::live(
        FdnDesign::new(8, Seconds(8.0), true).unwrap(),
        FdnControls::DEFAULT,
    ));
    let _ = s.update();
    let Tail::Finite(n) = s.graph_ref().node_tail(plain) else {
        panic!("finite")
    };
    assert!(n.get() as f64 / 48_000.0 > 8.0);
    assert_eq!(s.graph_ref().node_tail(frozen), Tail::Unbounded);
}
