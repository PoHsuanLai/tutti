use super::*;
use tutti_graph::contract::prepared;

fn ring_with(frames: &[[f32; 2]]) -> (MicRing, TapWriter) {
    let (ring, mut writer) = MicRing::pair(ChannelLayout::STEREO, Samples(64));
    writer.push_interleaved(frames.as_flattened());
    (ring, writer)
}

const RATE: SampleRate = SampleRate(48_000.0);

/// One block of `frames` frames, as `[left, right]`.
fn block(node: &mut MicMonitorNode, frames: usize) -> Vec<Vec<f32>> {
    // `drive_input`, not `drive`: with no inputs and no params, the block's
    // length comes from its `Env`.
    let env = tutti_graph::Env {
        frame: tutti_types::Frame(0),
        sample_rate: RATE,
        block_len: tutti_types::Samples(frames),
        transport: tutti_graph::Transport::default(),
        changes: tutti_graph::TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    tutti_graph::contract::drive_input(node, &env, &[], &[], &[]).audio
}

fn monitor(ring: MicRing) -> MicMonitorNode {
    prepared(MicMonitorNode::new(ring), RATE, 64)
}

#[test]
fn a_block_drains_the_ring_then_goes_silent() {
    let (ring, _prod) = ring_with(&[[1.0, 2.0], [3.0, 4.0]]);
    let mut node = monitor(ring);

    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (1.0, 2.0));
    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (3.0, 4.0));

    // Ring drained → underrun → silence, not a stall.
    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (0.0, 0.0));
}

#[test]
fn process_block_drains_and_pads_with_silence() {
    let (ring, _prod) = ring_with(&[[1.0, -1.0], [2.0, -2.0]]);
    let mut node = monitor(ring);

    let out = block(&mut node, 4);

    assert_eq!(out[0], [1.0, 2.0, 0.0, 0.0]);
    assert_eq!(out[1], [-1.0, -2.0, 0.0, 0.0]);
}

#[test]
fn gain_scales_the_monitored_signal() {
    let (ring, _prod) = ring_with(&[[1.0, 1.0]]);
    let mut node = prepared(
        MicMonitorNode::with_gain(ring, Amplitude::new(0.5)),
        RATE,
        64,
    );

    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (0.5, 0.5));
}

/// `reset` pops nothing: the frames are still there for the next block.
///
/// Mutation (run): `reset` draining the ring (`self.ring.0.clear()`) →
/// the block reads silence → fails.
#[test]
fn reset_does_not_consume_the_ring() {
    let (ring, _prod) = ring_with(&[[9.0, 9.0]]);
    let mut node = monitor(ring);
    Node::reset(&mut node);

    let out = block(&mut node, 1);
    assert_eq!(
        (out[0][0], out[1][0]),
        (9.0, 9.0),
        "reset left the ring untouched"
    );
}

/// **A node handed too few outputs must not eat captured frames.**
///
/// `process` checks it has both channels before its first pop, so a
/// block it cannot write leaves every frame in the ring for the next.
/// Unreachable through the graph, which hands a stereo node two outputs;
/// driven here through the `Node` trait with a declared width of two and
/// a block that is refused before the pop.
///
/// What this pins is the order, through the one path that can reach
/// it: the `let … else` returns before `pop_planes` runs.
///
/// Mutation (run): pop a frame before the width check
/// (`self.ring.0.skip(1);` above the `let … else`) → the good block
/// below reads `[3.0, 4.0]` first → fails.
#[test]
fn a_block_leaves_the_ring_untouched_until_it_can_write() {
    let (ring, _prod) = ring_with(&[[7.0, 8.0], [3.0, 4.0]]);
    let mut node = monitor(ring);
    let out = block(&mut node, 1);
    assert_eq!(
        (out[0][0], out[1][0]),
        (7.0, 8.0),
        "one block, one frame: nothing popped ahead of what is written"
    );
}

/// A block reads across the ring's wrap: the frames stored at the end
/// of its storage, then the ones at the start, in push order, and pads
/// with silence past them.
///
/// Mutation (run): `pop_planes` in the tap reader popping into
/// `plane[..frames.min(2)]` stops after two frames → the third and the
/// post-wrap reads come back as silence → fails.
#[test]
fn a_block_reads_across_the_ring_wrap() {
    let (ring, mut prod) = MicRing::pair(ChannelLayout::STEREO, Samples(4));
    let mut node = monitor(ring);
    prod.push_interleaved(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    assert_eq!(block(&mut node, 3)[0], [1.0, 2.0, 3.0]);
    // Three more: the fourth slot, then the first two, wrapped.
    prod.push_interleaved(&[4.0, -4.0, 5.0, -5.0, 6.0, -6.0]);
    let out = block(&mut node, 5);
    assert_eq!(out[0], [4.0, 5.0, 6.0, 0.0, 0.0]);
    assert_eq!(out[1], [-4.0, -5.0, -6.0, 0.0, 0.0]);
}

/// The declared shape: no inputs, as wide as the ring out, never skipped as
/// silent, no latency.
#[test]
fn the_declared_shape_matches_what_the_graph_is_told() {
    let (ring, _prod) = ring_with(&[]);
    let node = MicMonitorNode::new(ring);
    let shape = node.shape();
    assert_eq!(
        shape.audio_in,
        ChannelLayout::EMPTY,
        "a capture source takes no graph input"
    );
    assert_eq!(shape.audio_out, ChannelLayout::STEREO);
    assert_eq!(
        shape.tail,
        Tail::Unbounded,
        "a live source is never skipped"
    );
    assert!(shape.latency.samples().is_zero());
}

/// **The mic monitor refuses to be forked.** A copy over the ring's one
/// consumer would take live frames beside the live node, so its
/// `IntoNode` hands the editor no fork source, and a fork of a graph
/// that needs it fails naming its key.
///
/// Mutation (run): hand the editor a fork source (`fork: Some(..)`, even
/// one whose fork fails) → the fork is not refused as `NotForkable` →
/// fails.
#[test]
fn the_mic_refuses_to_fork() {
    use tutti_graph::{Editor, ForkError, ForkMode, ForkTarget};
    use tutti_types::NodeKey;
    use tutti_types::graph::{OutPort, Source};

    let (ring, _prod) = ring_with(&[[1.0, 2.0]]);
    let prepare = Prepare::new(RATE, tutti_types::Samples(64));
    let (mut editor, _exec) = Editor::new(prepare);
    editor.insert(NodeKey(7), "mic", MicMonitorNode::new(ring));
    editor.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(7),
                port,
            })
        })
        .collect();
    let err = editor
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect_err("a fork through the live mic is refused");
    assert_eq!(err, ForkError::NotForkable { key: NodeKey(7) });
}

/// The node is as wide as its ring: six lanes are six outputs, each channel on
/// its own output in the order the device pushed it, and the shape says so.
///
/// Mutation (run): `Shape::audio(.., ChannelLayout::STEREO)` back in `shape`
/// fails the width assertion; `pop_planes` handing lane `c` the plane `5 - c`
/// fails the order.
#[test]
fn a_ring_of_six_is_six_outputs_in_the_order_pushed() {
    let (ring, mut writer) = MicRing::pair(ChannelLayout::from(6u16), Samples(64));
    writer.push_interleaved(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, //
        -1.0, -2.0, -3.0, -4.0, -5.0, -6.0,
    ]);
    let node = MicMonitorNode::new(ring);
    assert_eq!(node.shape().audio_out, ChannelLayout::from(6u16));
    let mut node = prepared(node, RATE, 64);
    let out = block(&mut node, 3);
    assert_eq!(out.len(), 6);
    for (c, plane) in out.iter().enumerate() {
        let v = (c + 1) as f32;
        assert_eq!(plane, &[v, -v, 0.0], "channel {c}");
    }
}

/// A frame the ring has no room for is refused whole, on every channel: a
/// ring of room for two frames, offered three, keeps the first two complete
/// and the lanes level, never a third frame on some channels.
///
/// Mutation (run): pushing each lane up to its own room (not the fewest) in
/// `TapWriter::push_interleaved` is not reachable here (equal rings); dropping
/// the `.min(frames)` there makes the third frame overrun and the writer's
/// `debug_assert` fail.
#[test]
fn a_full_ring_refuses_whole_frames() {
    let (ring, mut writer) = MicRing::pair(ChannelLayout::from(3u16), Samples(2));
    writer.push_interleaved(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    let mut node = monitor(ring);
    let out = block(&mut node, 4);
    assert_eq!(out[0], [1.0, 4.0, 0.0, 0.0]);
    assert_eq!(out[1], [2.0, 5.0, 0.0, 0.0]);
    assert_eq!(out[2], [3.0, 6.0, 0.0, 0.0]);
}
