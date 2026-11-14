//! A node with five outputs is played by the two it names: the handle's strip
//! reads them, and the others stay on the node.

use crate::instrument_support::Rig;
use tutti_graph::{
    Cx, ForkByClone, Instrument, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status,
};
use tutti_session::SessionNode;
use tutti_session::{AddedInstrument, Losses, Root};
use tutti_types::ChannelLayout;

/// Output `c` is the constant `c + 1`, whatever it is played.
#[derive(Clone)]
struct Five;

impl Node for Five {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::from_count(5)).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for c in 0..5 {
            io.output(c).fill(c as f32 + 1.0);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Dressed(ForkByClone<Five>);

impl IntoNode for Dressed {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        self.0.into_parts()
    }
}
impl Instrument for Dressed {
    fn main_outputs(&self) -> [u16; 2] {
        [2, 3]
    }
}
impl SessionNode for Dressed {}

#[test]
fn the_pair_a_node_names_is_the_one_played() {
    let mut session = crate::instrument_support::rig::offline_session();
    let AddedInstrument { handle, .. } = session
        .add_instrument(Dressed(ForkByClone(Five)))
        .expect("five outputs and an event input");
    let mut rig = Rig::played(session, handle);
    rig.render_to(4 * crate::instrument_support::BLOCK);
    let at = rig.frame() - 1;
    assert_eq!((rig.left[at], rig.right[at]), (3.0, 4.0));
}

/// `add_instrument` wires the strip to the master and `add_instrument_unwired`
/// leaves it for the mix: the unwired one is silent at the root until its strip
/// is connected, then plays the same pair.
#[test]
fn only_add_instrument_wires_the_strip_to_the_master() {
    let mut session = crate::instrument_support::rig::offline_session();
    let AddedInstrument { handle, .. } = session
        .add_instrument_unwired(Dressed(ForkByClone(Five)))
        .expect("an instrument");
    let strip = handle.out();
    let mut rig = Rig::played(session, handle);
    rig.render_to(4 * crate::instrument_support::BLOCK);
    assert_eq!(rig.peak(0..rig.frame()), 0.0, "nothing reaches the root");
    rig.session.graph().connect(strip, Root).unwrap();
    rig.render_to(8 * crate::instrument_support::BLOCK);
    let at = rig.frame() - 1;
    assert_eq!((rig.left[at], rig.right[at]), (3.0, 4.0));
}

/// A pair that names an output the node lacks is refused, and the node removed.
#[test]
fn a_pair_past_the_outputs_is_refused() {
    struct Short;
    impl IntoNode for Short {
        type Controls = ();
        fn into_parts(self) -> NodeParts<()> {
            ForkByClone(Five).into_parts()
        }
    }
    impl Instrument for Short {
        fn main_outputs(&self) -> [u16; 2] {
            [4, 5]
        }
    }
    impl SessionNode for Short {}
    let mut session = crate::instrument_support::rig::offline_session();
    match session.add_instrument(Short) {
        Err(tutti_session::Error::NotInstrument {
            why: tutti_session::NotInstrument::NoOutput(5),
            ..
        }) => {}
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("accepted"),
    }
}

/// The handle counts what its queue dropped: with a ring's worth of messages
/// waiting for frames far ahead, three more for frames still further are lost,
/// and the handle says so.
#[test]
fn the_handle_counts_what_its_queue_dropped() {
    use tutti_types::Frame;
    let mut session = crate::instrument_support::rig::offline_session();
    let AddedInstrument { handle, .. } = session
        .add_instrument(Dressed(ForkByClone(Five)))
        .expect("an instrument");
    let mut rig = Rig::played(session, handle);
    let on = crate::instrument_support::midi::note_on(0, 60, 100);
    for k in 0..256u64 {
        rig.handle
            .keys_mut()
            .push(Frame::new(1_000_000 + k), on)
            .unwrap();
    }
    rig.block();
    assert_eq!(rig.handle.keys().losses(), Losses::NONE);
    for k in 0..3u64 {
        rig.handle
            .keys_mut()
            .push(Frame::new(9_000_000 + k), on)
            .unwrap();
    }
    rig.block();
    assert_eq!(rig.handle.keys().losses(), Losses::evicted(3));
}

/// A root of five channels or more fed from an instrument with sends takes its
/// main pair, repeated, and never a send: connecting the node's whole main bus
/// to the root feeds every output (a five-output node on a five-wide root feeds
/// the speakers its sends), wiring the handle's pair onto each root channel
/// does not. Read from the graph's own topology.
#[test]
fn a_wide_root_is_fed_from_the_main_pair_not_the_sends() {
    use tutti_session::OutPort;
    let mut session = crate::instrument_support::rig::offline_session();
    let AddedInstrument { handle, .. } = session
        .add_instrument_unwired(Dressed(ForkByClone(Five)))
        .expect("an instrument");
    session.graph().widen_outputs(5).expect("a wider root");
    let node = handle.node();
    session.graph().connect(node, Root).unwrap();
    let wrapped: Vec<_> = (0..5)
        .map(|c| session.graph_ref().sources(Root::output(c)).unwrap())
        .collect();
    assert_eq!(wrapped[4], [OutPort::new(node, 4)], "the wrap feeds a send");

    let main = handle.main_outputs();
    for c in 0..5 {
        session
            .graph()
            .set_sources(Root::output(c), [OutPort::new(node, main[c % 2])])
            .unwrap();
    }
    assert_eq!(main, [2, 3]);
    for c in 0..5 {
        let want = 2 + c % 2;
        assert_eq!(
            session.graph_ref().sources(Root::output(c)).unwrap(),
            [OutPort::new(node, want)],
            "channel {c}"
        );
    }
}
