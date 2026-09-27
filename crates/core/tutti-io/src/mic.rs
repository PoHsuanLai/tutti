//! Live-input monitoring node.
//!
//! [`MicMonitorNode`] drains a ring a *capture device* fills: a graph
//! node with no inputs and two outputs whose whole job is "pop the next frame
//! the producer pushed, or emit silence on underrun." The producer end lives
//! in the device layer (`tutti-cpal`'s `MicIn`); this node is device-free so
//! it can sit anywhere in the graph — wire it through effects and the mic is
//! heard live and effected while the same ring records to a
//! [`WavOut`](crate::WavOut).
//!
//! # The ring handle
//!
//! The node holds its ring consumer behind [`MicRing`], an
//! `Arc<AudioThreadCell<HeapCons<_>>>`: lock-free on the hot path
//! ([`AudioThreadCell`] hands `&mut` access through `&self` with no `Mutex`;
//! a debug-only in-use flag catches genuine concurrent borrows). The ring is
//! single-consumer, so no lock is required — the same reasoning the
//! streaming reader runs on.
//!
//! # Single-consumer invariant
//!
//! [`HeapCons::try_pop`] advances the read index, so exactly one party may pop.
//! What keeps that true: **the ring is popped only from
//! [`Node::process`](tutti_graph::Node::process), and there is one node.**
//!
//! - The node is **not `Clone`**: under `Net`, every commit cloned every unit
//!   and kept a clone on the main thread over the same consumer, which is why
//!   `reset` had to be a no-op. The graph owns its unit and never
//!   clones it, and without `Clone` no second consumer can be made from the
//!   node at all:
//!
//!   ```compile_fail
//!   fn second_consumer(node: &tutti_io::MicMonitorNode) -> tutti_io::MicMonitorNode {
//!       node.clone()
//!   }
//!   ```
//!
//! - It is **unforkable**: its `IntoNode` hands the editor no fork source, so
//!   a fork of a graph that needs it (an export of the live mic) is
//!   [`ForkError::NotForkable`](tutti_graph::ForkError::NotForkable) naming
//!   its key, never a copy that would take live frames beside it.
//!
//! - `reset` still pops nothing: the frames in the ring are the device's,
//!   not state of the node's, and the ring self-limits (~10 ms).
//!
//! The device callback only ever *pushes* (the producer half, holding
//! `HeapProd` directly), never pops.

use std::sync::Arc;

use ringbuf::{traits::Consumer, HeapCons};
use tutti_core::{Amplitude, AudioThreadCell, ChannelLayout, SampleRate, Tail};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};

/// A stereo capture-ring consumer.
///
/// Built by the device layer via [`share_mic_ring`]: it splits a
/// [`HeapRb`](ringbuf::HeapRb), keeps the producer to push captured frames, and
/// hands the consumer here for [`MicMonitorNode::new`]. This is an **opaque
/// handle** — the inner `Arc<AudioThreadCell<HeapCons<_>>>` is deliberately
/// private so the ring's single-consumer invariant (see the module docs) can't
/// be broken by a consumer popping the raw ring off the audio thread. `Clone`
/// shares the one underlying consumer: a device layer may hold it until it
/// builds the node.
#[derive(Clone)]
pub struct MicRing(Arc<AudioThreadCell<HeapCons<[f32; 2]>>>);

impl MicRing {
    /// Pop the next captured frame. Crate-internal so only `MicMonitorNode`'s
    /// audio-thread `process` can advance the SPSC read index.
    #[inline]
    pub(crate) fn try_pop(&self) -> Option<[f32; 2]> {
        self.0.borrow_mut().try_pop()
    }
}

impl std::fmt::Debug for MicRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner `HeapCons` isn't `Debug`, and its occupancy is a hot-path
        // read this must not take; a shared-count summary is enough.
        f.debug_struct("MicRing")
            .field("shared", &Arc::strong_count(&self.0))
            .finish_non_exhaustive()
    }
}

/// Wrap a freshly-split ring consumer into a [`MicRing`] for handoff to the
/// audio thread.
#[must_use = "the returned MicRing is the only handle to the capture ring; drop it and the monitor node has nothing to drain"]
pub fn share_mic_ring(consumer: HeapCons<[f32; 2]>) -> MicRing {
    MicRing(Arc::new(AudioThreadCell::new(consumer)))
}

/// Live microphone monitoring as a graph node (0 in → 2 out),
/// inserted unforkable (see the module docs).
///
/// Drains the capture ring one frame per output sample; an empty ring (the
/// device hasn't pushed yet, or the audio thread outran it) emits silence rather
/// than stalling — the live-source analogue of the streaming unit's underrun
/// hold. No pitch/speed/interpolation: the mic is already at the graph rate
/// (the device layer opens it so), so this is a straight per-frame passthrough.
///
/// Not `Clone`: a clone would be a second consumer of the one ring (see the
/// module's single-consumer note).
#[derive(Debug)]
pub struct MicMonitorNode {
    ring: MicRing,
    gain: Amplitude,
    /// The rate the device layer opened the mic at, when it said so.
    ///
    /// `None` for a node built by hand, which cannot know. When it is known,
    /// `prepare` debug-asserts the graph agrees — the unchecked half
    /// of the guarantee whose checked half is `tutti_cpal`'s
    /// `Error::SampleRateMismatch`. Same two-check shape as `pump`'s layout
    /// `debug_assert` beside `Recorder::start`'s returned error, and for the
    /// same reason: this node does not resample, so a disagreement is a drift
    /// nobody reports.
    device_rate: Option<SampleRate>,
}

impl MicMonitorNode {
    /// Build a monitor node over a shared capture ring at unity gain.
    pub fn new(ring: MicRing) -> Self {
        Self {
            ring,
            gain: Amplitude::new(1.0),
            device_rate: None,
        }
    }

    /// Build a monitor node over a ring the device layer opened at
    /// `device_rate`, which the graph is then held to.
    ///
    /// This is what `tutti_cpal::MicIn::open_with_monitor` uses; a caller
    /// wiring a ring by hand wants [`new`](Self::new).
    pub fn new_at(ring: MicRing, device_rate: SampleRate) -> Self {
        Self {
            ring,
            gain: Amplitude::new(1.0),
            device_rate: Some(device_rate),
        }
    }

    /// Build a monitor node over a shared capture ring at `gain`.
    pub fn with_gain(ring: MicRing, gain: Amplitude) -> Self {
        Self {
            ring,
            gain,
            device_rate: None,
        }
    }

    /// Pop the next captured frame, or `(0.0, 0.0)` on underrun. Audio-thread
    /// only (see the single-consumer invariant in the module docs).
    #[inline]
    fn next_frame(&self) -> (f32, f32) {
        let g = self.gain.get();
        match self.ring.try_pop() {
            Some([l, r]) => (l * g, r * g),
            None => (0.0, 0.0),
        }
    }
}

impl Node for MicMonitorNode {
    /// Stereo out, deliberately — unlike the voice units, which take a
    /// runtime width.
    ///
    /// The producer is the CPAL input callback in `tutti-cpal`, which downmixes
    /// each interleaved device frame to a stereo pair before it ever reaches
    /// [`MicRing`] (a `HeapCons<[f32; 2]>`). Widening this node without widening
    /// that callback and the ring would declare an arity its own source can
    /// never fill, so live capture stays stereo until the device edge is
    /// widened with it. That edge is app-side, not engine-side.
    ///
    /// A live source: [`Tail::Unbounded`], so the executor never skips it as
    /// silent (a skipped block would leave its frames to pile up in the ring).
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_tail(Tail::Unbounded)
    }

    fn prepare(&mut self, p: &Prepare) {
        // Nothing to reconfigure — this node does not resample. But the claim
        // that it does not *need* to is checked: the device layer opens the
        // mic at the graph's rate. `MicIn::open` is the checked half (it
        // returns `Error::SampleRateMismatch`); this is the unchecked half, in
        // the same shape as `pump`'s layout `debug_assert` beside
        // `Recorder::start`'s returned error.
        debug_assert!(
            self.device_rate.is_none_or(|r| r == p.sample_rate()),
            "mic opened at {:?} but the graph runs at {:?} — \
             MicMonitorNode does not resample, so this drifts silently",
            self.device_rate,
            p.sample_rate()
        );
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        // Width check BEFORE any pop: `next_frame` consumes from the ring, so
        // popping and then declining to write would throw captured frames
        // away. Unreachable through the graph — the node declares stereo and
        // is handed two outputs — but a discard is never the branch you want
        // on a path whose whole job is not losing frames.
        let (Some(left), Some(right)) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            (*l, *r) = self.next_frame();
        }
        Status::Modified
    }

    fn reset(&mut self) {
        // Pops nothing: the frames in the ring are the device's, not the
        // node's state, and the ring self-limits to ~10 ms, so there is no
        // stale backlog worth draining.
    }
}

/// Inserted **unforkable**: no fork source, so a fork of a graph that needs
/// the monitor fails naming it (see the module docs). No controls.
impl IntoNode for MicMonitorNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_graph::contract::prepared;

    fn ring_with(frames: &[[f32; 2]]) -> (MicRing, ringbuf::HeapProd<[f32; 2]>) {
        let rb = HeapRb::<[f32; 2]>::new(64);
        let (mut prod, cons) = rb.split();
        for &f in frames {
            let _ = prod.try_push(f);
        }
        (share_mic_ring(cons), prod)
    }

    const RATE: SampleRate = SampleRate(48_000.0);

    /// One block of `frames` frames, as `[left, right]`.
    fn block(node: &mut MicMonitorNode, frames: usize) -> Vec<Vec<f32>> {
        // `drive_in`, not `drive`: with no inputs and no params, the block's
        // length comes from its `Env`.
        let env = tutti_graph::Env {
            frame: tutti_core::Frame(0),
            sample_rate: RATE,
            block_len: tutti_core::Samples(frames),
            transport: tutti_graph::Transport::default(),
            changes: tutti_graph::TransportChanges::NONE,
        };
        tutti_graph::contract::drive_in(node, &env, &[], &[], &[]).audio
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
    /// Mutation (run): `reset` draining the ring (`while self.ring.try_pop()
    /// .is_some() {}`) → the block reads silence → fails.
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
    /// it: the `let … else` returns before `next_frame` runs.
    ///
    /// Mutation (run): pop a frame before the width check (`let _ =
    /// self.next_frame();` above the `let … else`) → the good block below
    /// reads `[3.0, 4.0]` first → fails.
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

    /// The declared shape: no inputs, stereo out, never skipped as silent,
    /// no latency.
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
        use tutti_core::graph::{OutPort, Source};
        use tutti_core::NodeKey;
        use tutti_graph::{Editor, ForkError, ForkMode, ForkTarget};

        let (ring, _prod) = ring_with(&[[1.0, 2.0]]);
        let prepare = Prepare::new(RATE, tutti_core::Samples(64));
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
            .err()
            .expect("a fork through the live mic is refused");
        assert_eq!(err, ForkError::NotForkable { key: NodeKey(7) });
    }
}
