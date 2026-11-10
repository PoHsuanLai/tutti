use super::*;
use crate::contract::{drive_input, prepared};
use crate::event::{EventWriter, SortedEvents};
use crate::node::{ConstantMask, Env, InPlaceMask, SilenceMask};
use crate::{PlayClock, Transport, TransportChanges};
use tutti_types::{Frame, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);

fn ring_with(frames: &[[f32; 2]]) -> (InletNode, TapWriter) {
    let (node, mut writer) = InletNode::standalone(ChannelLayout::STEREO, Samples(64));
    writer.push_interleaved(frames.as_flattened());
    (node, writer)
}

/// One block of `frames` frames, as `[left, right]`.
fn block(node: &mut InletNode, frames: usize) -> Vec<Vec<f32>> {
    // `drive_input`, not `drive`: with no inputs and no params, the block's
    // length comes from its `Env`.
    let env = Env {
        frame: Frame(0),
        sample_rate: RATE,
        block_len: Samples(frames),
        transport: Transport::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    };
    drive_input(node, &env, &[], &[], &[]).audio
}

fn prepared_node(node: InletNode) -> InletNode {
    prepared(node, RATE, 64)
}

#[test]
fn a_block_drains_the_ring_then_goes_silent() {
    let (node, _prod) = ring_with(&[[1.0, 2.0], [3.0, 4.0]]);
    let mut node = prepared_node(node);

    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (1.0, 2.0));
    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (3.0, 4.0));

    // Ring drained, so an underrun: silence, not a stall.
    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (0.0, 0.0));
}

#[test]
fn a_block_drains_and_pads_with_silence() {
    let (node, _prod) = ring_with(&[[1.0, -1.0], [2.0, -2.0]]);
    let mut node = prepared_node(node);

    let out = block(&mut node, 4);

    assert_eq!(out[0], [1.0, 2.0, 0.0, 0.0]);
    assert_eq!(out[1], [-1.0, -2.0, 0.0, 0.0]);
}

/// There is no gain: what the producer pushed is what the graph gets.
///
/// Mutation (run): scale each popped plane by 0.5 in `process` and the block
/// reads half the pushed level -> fails.
#[test]
fn the_signal_passes_at_unity() {
    let (node, _prod) = ring_with(&[[1.0, 0.25]]);
    let mut node = prepared_node(node);
    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (1.0, 0.25));
}

/// `reset` pops nothing: the frames are still there for the next block.
///
/// Mutation (run): `reset` draining the ring (`self.ring.pop_planes(..)`) ->
/// the block reads silence -> fails.
#[test]
fn reset_does_not_consume_the_ring() {
    let (node, _prod) = ring_with(&[[9.0, 9.0]]);
    let mut node = prepared_node(node);
    Node::reset(&mut node);

    let out = block(&mut node, 1);
    assert_eq!((out[0][0], out[1][0]), (9.0, 9.0), "reset left the ring");
}

/// A block reads across the ring's wrap: the frames stored at the end of its
/// storage, then the ones at the start, in push order, and pads with silence
/// past them.
///
/// Mutation (run): `pop_planes` in the tap reader popping into
/// `plane[..frames.min(2)]` stops after two frames -> the third and the
/// post-wrap reads come back as silence -> fails.
#[test]
fn a_block_reads_across_the_ring_wrap() {
    let (node, mut prod) = InletNode::standalone(ChannelLayout::STEREO, Samples(4));
    let mut node = prepared_node(node);
    prod.push_interleaved(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    assert_eq!(block(&mut node, 3)[0], [1.0, 2.0, 3.0]);
    // Three more: the fourth slot, then the first two, wrapped.
    prod.push_interleaved(&[4.0, -4.0, 5.0, -5.0, 6.0, -6.0]);
    let out = block(&mut node, 5);
    assert_eq!(out[0], [4.0, 5.0, 6.0, 0.0, 0.0]);
    assert_eq!(out[1], [-4.0, -5.0, -6.0, 0.0, 0.0]);
}

/// The declared shape: no inputs, as wide as the ring out, never skipped as
/// silent, no latency, cheap.
///
/// Mutation (run): `Cost::Light` removed from `shape` fails the cost row;
/// `Tail::Unbounded` replaced by `Tail::ZERO` fails the tail row.
#[test]
fn the_declared_shape_matches_what_the_graph_is_told() {
    let (node, _prod) = ring_with(&[]);
    let shape = node.shape();
    assert_eq!(
        shape.audio_in,
        ChannelLayout::EMPTY,
        "a source takes no input"
    );
    assert_eq!(shape.audio_out, ChannelLayout::STEREO);
    assert_eq!(
        shape.tail,
        Tail::Unbounded,
        "a live source is never skipped"
    );
    assert!(shape.latency.samples().is_zero());
    assert_eq!(shape.cost, Cost::Light);
}

/// The inlet refuses to be forked. A copy over the ring's one consumer would
/// take live frames beside the live node, so its `IntoNode` hands the editor
/// no fork source, and a fork of a graph that needs it fails naming its key.
///
/// Mutation (run): hand the editor a fork source (`fork: Some(..)`, even one
/// whose fork fails) -> the fork is not refused as `NotForkable` -> fails.
#[test]
fn the_inlet_refuses_to_fork() {
    use crate::{Editor, ForkError, ForkMode, ForkTarget};
    use tutti_types::NodeKey;
    use tutti_types::graph::{OutPort, Source};

    let (node, _prod) = ring_with(&[[1.0, 2.0]]);
    let prepare = Prepare::new(RATE, Samples(64));
    let (mut editor, _exec) = Editor::new(prepare);
    editor.insert(NodeKey(7), "inlet", node);
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
        .expect_err("a fork through the live inlet is refused");
    assert_eq!(err, ForkError::NotForkable { key: NodeKey(7) });
}

/// The node is as wide as its ring: six lanes are six outputs, each channel on
/// its own output in the order the producer pushed it, and the shape says so.
///
/// Mutation (run): `Shape::audio(.., ChannelLayout::STEREO)` back in `shape`
/// fails the width assertion; `pop_planes` handing lane `c` the plane `5 - c`
/// fails the order.
#[test]
fn a_ring_of_six_is_six_outputs_in_the_order_pushed() {
    let (node, mut writer) = InletNode::standalone(ChannelLayout::from(6u16), Samples(64));
    writer.push_interleaved(&[
        1.0, 2.0, 3.0, 4.0, 5.0, 6.0, //
        -1.0, -2.0, -3.0, -4.0, -5.0, -6.0,
    ]);
    assert_eq!(node.shape().audio_out, ChannelLayout::from(6u16));
    let mut node = prepared_node(node);
    let out = block(&mut node, 3);
    assert_eq!(out.len(), 6);
    for (c, plane) in out.iter().enumerate() {
        let v = (c + 1) as f32;
        assert_eq!(plane, &[v, -v, 0.0], "channel {c}");
    }
}

/// A frame the ring has no room for is refused whole, on every channel: a ring
/// of room for two frames, offered three, keeps the first two complete and the
/// lanes level, never a third frame on some channels.
///
/// Mutation (run): dropping the `.min(frames)` in `TapWriter::push_interleaved`
/// makes the third frame overrun and the writer's `debug_assert` fail.
#[test]
fn a_full_ring_refuses_whole_frames() {
    let (node, mut writer) = InletNode::standalone(ChannelLayout::from(3u16), Samples(2));
    writer.push_interleaved(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    let mut node = prepared_node(node);
    let out = block(&mut node, 4);
    assert_eq!(out[0], [1.0, 4.0, 0.0, 0.0]);
    assert_eq!(out[1], [2.0, 5.0, 0.0, 0.0]);
    assert_eq!(out[2], [3.0, 6.0, 0.0, 0.0]);
}

/// Runs `node.process` by hand over `planes` outputs that already hold
/// `fill`, `frames` long, as a graph hands a node whose arena was dirty.
fn process_over(node: &mut InletNode, planes: usize, frames: usize, fill: f32) -> Vec<Vec<f32>> {
    let mut out = vec![vec![fill; frames]; planes];
    let mut refs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    let events_in: Vec<SortedEvents<'_>> = Vec::new();
    let mut writers: Vec<EventWriter<'_>> = Vec::new();
    let env = Env {
        frame: Frame(0),
        sample_rate: RATE,
        block_len: Samples(frames),
        transport: Transport::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    };
    let cx = Cx {
        env: &env,
        arrival: tutti_types::Latency::ZERO,
        history: crate::TransportHistory::none(),
    };
    let max = Prepare::new(RATE, Samples(frames)).max_block();
    let io = Io::new(
        max,
        frames,
        &[],
        &mut refs,
        SilenceMask::NONE,
        ConstantMask::NONE,
        InPlaceMask::NONE,
        &events_in,
        &mut writers,
    );
    node.process(&cx, io);
    out
}

/// **A node handed a number of outputs other than its ring's writes silence
/// on all of them, and leaves the ring alone.**
///
/// Unreachable through a graph, which hands a node the outputs it declared,
/// so the node is driven by hand over outputs holding a sentinel: a node that
/// left them unwritten would hand the sentinel on. The ring is not popped
/// either, so the frames wait for a block that can be written.
///
/// Mutation (run): returning early without the `fill(0.0)` leaves the
/// sentinel and fails the silence assertions; popping before the width check
/// (`self.ring.pop_planes(..)` above it) fails the final read of `[7.0, 8.0]`.
#[test]
fn a_width_mismatch_writes_silence_and_keeps_the_ring() {
    let (node, _prod) = ring_with(&[[7.0, 8.0]]);
    let mut node = prepared_node(node);

    for planes in [1, 3] {
        let out = process_over(&mut node, planes, 4, 9.0);
        assert!(
            out.iter().flatten().all(|&s| s == 0.0),
            "{planes} outputs against a stereo ring: silence, not the arena's leftovers"
        );
    }

    let out = block(&mut node, 1);
    assert_eq!(
        (out[0][0], out[1][0]),
        (7.0, 8.0),
        "the mismatched blocks popped nothing"
    );
}

/// The matched case over dirty outputs: the popped frames, then silence over
/// what the arena held, never its leftovers.
///
/// Mutation (run): dropping the `plane[popped..].fill(0.0)` leaves the
/// sentinel past the popped frame and fails.
#[test]
fn an_underrun_overwrites_what_the_output_held() {
    let (node, _prod) = ring_with(&[[1.0, 2.0]]);
    let mut node = prepared_node(node);
    let out = process_over(&mut node, 2, 3, 9.0);
    assert_eq!(out[0], [1.0, 0.0, 0.0]);
    assert_eq!(out[1], [2.0, 0.0, 0.0]);
}
