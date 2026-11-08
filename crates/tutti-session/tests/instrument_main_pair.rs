//! A node with five outputs is played by the two it names: the handle's strip
//! reads them, and the others stay on the node.

mod instrument_support;

use instrument_support::Rig;
use tutti_graph::{
    Cx, ForkByClone, Instrument, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status,
};
use tutti_session::SessionNode;
use tutti_types::ChannelLayout;

/// Output `c` is the constant `c + 1`, whatever it is played.
#[derive(Clone)]
struct Five;

impl Node for Five {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(5)).with_events(1, 0)
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

/// Mutations (run): the strip's ports read `[0, 1]` regardless (left 1, right
/// 2); the two swapped (left 4, right 3).
#[test]
fn the_pair_a_node_names_is_the_one_played() {
    let mut session = instrument_support::rig::offline_session();
    let (handle, ()) = session
        .add_instrument(Dressed(ForkByClone(Five)))
        .expect("five outputs and an event input");
    let mut rig = Rig::wired(session, handle);
    rig.render_to(4 * instrument_support::BLOCK);
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
    let mut session = instrument_support::rig::offline_session();
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
///
/// Mutation (run): `InstrumentHandle::dropped` returning 0 fails it.
#[test]
fn the_handle_counts_what_its_queue_dropped() {
    use tutti_types::{At, Frame};
    let mut session = instrument_support::rig::offline_session();
    let (handle, ()) = session
        .add_instrument(Dressed(ForkByClone(Five)))
        .expect("an instrument");
    let mut rig = Rig::wired(session, handle.clone());
    let on = instrument_support::midi::note_on(0, 60, 100);
    for k in 0..256u64 {
        handle.send(At::Frame(Frame(1_000_000 + k)), on).unwrap();
    }
    rig.block();
    assert_eq!(handle.dropped(), 0);
    for k in 0..3u64 {
        handle.send(At::Frame(Frame(9_000_000 + k)), on).unwrap();
    }
    rig.block();
    assert_eq!(handle.dropped(), 3);
}
