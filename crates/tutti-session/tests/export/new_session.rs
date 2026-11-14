//! `Session::new`: the full build, with the engine, bounces what it holds.

use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{Bounce, BounceLatency, BounceTail, Range, Root, Session};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Seconds};

/// **A session from `Session::new` is a full session.** A constant wired to
/// the root bounces to buffers holding that constant on every frame, and
/// `render`, which only a session with an engine has, answers instead of
/// panicking.
#[test]
fn a_session_from_new_bounces_a_node_and_renders() {
    let mut session = Session::new(SampleRate::SR_48K).expect("builds");
    let (node, _) = session
        .graph()
        .add(ForkByClone(Const::new(0.25, ChannelLayout::STEREO)));
    session.graph().connect(node, Root).unwrap().hard();
    let planes = session
        .bounce(
            Bounce::project()
                .master_buffers()
                .range(Range::Seconds(Seconds(0.0), Seconds(0.05)))
                .latency(BounceLatency::Keep)
                .tail(BounceTail::None),
        )
        .expect("the bounce renders")
        .into_master()
        .into_planes();
    assert_eq!(planes.len(), 2);
    assert!(planes.iter().all(|p| !p.is_empty()));
    assert!(planes.iter().flatten().all(|&x| x == 0.25));

    let mut block = [0.0f32; 128];
    session.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
}
