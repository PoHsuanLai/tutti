//! The published latency shrinks when the latency-bearing node leaves.
//!
//! The session half of `bevy-tutti`'s `graph_value`
//! `the_latency_plan_shrinks_when_the_latency_bearing_node_leaves`, which
//! read the figure off the adapter's topology value. Here it is the figure
//! the session publishes after the compiler's solve: the total, and the
//! per-channel compensation a source outside the graph seeks by.
//!
//! Two paths reach the outputs: one through a lookahead limiter, one dry. The
//! dry channel pre-rolls to match, which is the whole figure. Removing the
//! limiter takes the channel it fed to silence, and with nothing
//! latency-bearing left the published latency is nothing at all.
//!
//! Mutation (run): `Session::publish_sent` keeping the larger of the old and
//! the new total (`self.latency = self.latency.max(sent.total())`) → the
//! figure never shrinks and the last assertions fail.

use tutti_graph::ForkByClone;
use tutti_nodes::LimiterNode;
use tutti_nodes::testing::Const;
use tutti_session::{GraphSource, Session};
use tutti_types::{ChannelLayout, Db, SampleRate, Samples};

#[test]
fn the_published_latency_shrinks_when_the_latency_bearing_node_leaves() {
    let mut s = Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate(48_000.0))
        .expect("builds");
    let (dry, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    let (lim, _) = s.graph().add(LimiterNode::with_channels(
        ChannelLayout::MONO,
        Db(-1.0),
        Db(-0.3),
    ));
    s.graph().set_source(lim, 0, GraphSource::Node(src, 0));
    s.graph().set_output_source(0, GraphSource::Node(lim, 0));
    s.graph().set_output_source(1, GraphSource::Node(dry, 0));
    s.update();

    let before = s.latency();
    assert!(
        before > Samples(0),
        "the limiter reports latency, so the graph has a figure to display"
    );
    assert_eq!(
        s.compensation().to_vec().as_slice(),
        [Samples(0), before],
        "the dry channel pre-rolls by the limiter's lookahead"
    );

    assert!(s.remove(lim));
    s.update();
    assert!(s.latency() < before, "the figure shrank");
    assert_eq!(
        s.latency(),
        Samples(0),
        "and with nothing latency-bearing left, to nothing at all"
    );
    assert_eq!(
        s.compensation().to_vec().as_slice(),
        [Samples(0), Samples(0)]
    );
}
