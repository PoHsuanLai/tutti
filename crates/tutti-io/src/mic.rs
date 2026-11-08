//! Live-input monitoring node.
//!
//! [`MicMonitorNode`] drains a ring a *capture device* fills: a graph
//! node with no inputs and one output a captured channel, whose whole job is "pop the next frame
//! the producer pushed, or emit silence on underrun." The producer end lives
//! in the device layer (`tutti-cpal`'s `MicIn`); this node is device-free so
//! it can sit anywhere in the graph — wire it through effects and the mic is
//! heard live and effected while the same ring records to a
//! [`WavOut`](crate::WavOut).
//!
//! # The ring handle
//!
//! The node owns its ring consumer, wrapped in [`MicRing`]: a plain newtype
//! over a tap's reader (one lane a channel, popped by whole frames) that pops
//! through `&mut self`, so the borrow checker, not
//! a cell, is what keeps it to one popper. The ring is single-consumer, so no
//! lock is required — the same reasoning the streaming reader runs on.
//!
//! # Single-consumer invariant
//!
//! Popping advances the read index, so exactly one party may pop.
//! What keeps that true: **the ring is popped only from
//! [`Node::process`], and there is one node.**
//!
//! - The node is **not `Clone`**: the graph owns its node and never clones
//!   it, and without `Clone` no second consumer can be made from the node at
//!   all:
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

use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status, TapReader, TapWriter,
};
use tutti_types::{Amplitude, ChannelLayout, SampleRate, Samples, Tail, TapId};

/// The consumer end of a capture ring, as an opaque handle for
/// [`MicMonitorNode`]: any number of channels, written and read by whole
/// frames.
///
/// Built with [`MicRing::pair`]: the device layer keeps the [`TapWriter`] to
/// push captured frames from its input callback
/// ([`push_interleaved`](TapWriter::push_interleaved)) and hands this to the
/// monitor node. It is a [`TapReader`] with one lane a channel, so a frame
/// cannot tear across channels, and a frame the ring has no room for is
/// refused whole and counted. Only the monitor node, on the audio thread, can
/// pop from it, so the ring keeps exactly one consumer. Not `Clone`: the
/// handle *is* the one consumer, and the node that owns it pops through
/// `&mut self`.
///
/// ```
/// use tutti_io::MicRing;
/// use tutti_types::{ChannelLayout, Samples};
///
/// let (ring, mut device_side) = MicRing::pair(ChannelLayout::STEREO, Samples(64));
/// device_side.push_interleaved(&[0.5, -0.5]);
/// assert_eq!(ring.layout(), ChannelLayout::STEREO);
/// ```
///
/// A second consumer cannot be made:
///
/// ```compile_fail
/// use tutti_io::MicRing;
/// use tutti_types::{ChannelLayout, Samples};
///
/// let (ring, _device_side) = MicRing::pair(ChannelLayout::STEREO, Samples(64));
/// let second = ring.clone();
/// ```
// Mutation (run) for the `compile_fail` doctest: a `Clone` impl on
// `MicRing` (`unimplemented!()` in its body, since `TapReader` has none) →
// the doctest compiles → fails.
pub struct MicRing(TapReader);

impl MicRing {
    /// A ring of `width` channels holding `capacity` frames, and the writer
    /// that fills it. Allocates; control side.
    #[must_use = "the writer is the only way to fill the ring, and the ring the only way to read it"]
    pub fn pair(width: ChannelLayout, capacity: Samples) -> (Self, TapWriter) {
        let (reader, writer) =
            TapReader::standalone(TapId(0), usize::from(width.count()), capacity);
        (Self(reader), writer)
    }

    /// The ring's width: one lane a channel.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.0.layout()
    }

    /// Pop up to `frames` captured frames into the planes, one a channel,
    /// returning how many were popped. Crate-internal so only
    /// `MicMonitorNode`'s audio-thread `process` can advance the read index.
    ///
    /// Each lane copies straight out of its ring's storage and advances its
    /// read index once: two atomics a lane per block.
    #[inline]
    pub(crate) fn pop_planes<'p>(
        &mut self,
        planes: impl IntoIterator<Item = &'p mut [f32]>,
        frames: usize,
    ) -> usize {
        self.0.pop_planes(planes, Samples(frames)).get()
    }
}

impl std::fmt::Debug for MicRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Its occupancy is a hot-path read this must not take.
        f.debug_struct("MicRing").finish_non_exhaustive()
    }
}

/// Live microphone monitoring as a graph node: no inputs, one output a channel
/// of its ring.
///
/// Each block it pops one captured frame per output frame from its
/// [`MicRing`], scaled by its gain; an empty ring (the device has not pushed
/// yet, or the audio thread outran it) yields silence rather than stalling.
/// It does not resample: the device must run at the graph's rate, which
/// `tutti_cpal::MicIn` ensures. Wire it through effects to hear the mic live
/// while the same capture callback records to a [`WavOut`](crate::WavOut).
///
/// Real-time safe: `process` does not allocate or lock. It reports an
/// unbounded tail, so the executor never skips it as silent.
///
/// Not `Clone`, and inserted **unforkable**: a clone or fork would be a second
/// consumer of the one ring. Forking a graph that holds it (an offline export
/// of the live mic, for instance) fails with
/// [`ForkError::NotForkable`](tutti_graph::ForkError::NotForkable) naming its
/// key.
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
    /// Creates a monitor node over a capture ring, at unity gain.
    pub fn new(ring: MicRing) -> Self {
        Self {
            ring,
            gain: Amplitude::new(1.0),
            device_rate: None,
        }
    }

    /// Creates a monitor node over a ring the device layer opened at
    /// `device_rate`, at unity gain.
    ///
    /// In debug builds, preparing the node for a graph at any other rate
    /// panics, since the node does not resample. This is what
    /// `tutti_cpal::MicIn::open_with_monitor` uses; a caller wiring a ring by
    /// hand can use [`new`](Self::new).
    pub fn new_at(ring: MicRing, device_rate: SampleRate) -> Self {
        Self {
            ring,
            gain: Amplitude::new(1.0),
            device_rate: Some(device_rate),
        }
    }

    /// Creates a monitor node over a capture ring, at `gain`.
    pub fn with_gain(ring: MicRing, gain: Amplitude) -> Self {
        Self {
            ring,
            gain,
            device_rate: None,
        }
    }
}

impl Node for MicMonitorNode {
    /// As wide as the ring: one output a captured channel.
    ///
    /// The producer is the device layer's input callback, which writes the
    /// channels the mic was opened with (`tutti_cpal::MicIn`), so the ring's
    /// width is the arity the source can fill. Graph ports are counts; the
    /// channels are the device's inputs, which feed no speaker
    /// ([`Speaker::Discrete`](tutti_types::Speaker::Discrete)), and where they
    /// go is the wiring's.
    ///
    /// A live source: [`Tail::Unbounded`], so the executor never skips it as
    /// silent (a skipped block would leave its frames to pile up in the ring).
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.ring.layout())
            .with_tail(Tail::Unbounded)
            // A block copy out of the ring: measured 0.4–0.6 ns per frame and
            // channel, against a gain's 0.05 and `SvfFilterNode`'s 4.0 on the
            // same box, under the boundary (doc 013, "The cost model and
            // coarsening").
            .with_cost(tutti_graph::Cost::Light)
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
        // Width check BEFORE any pop: `pop_planes` consumes from the ring, so
        // popping and then declining to write would throw captured frames
        // away. Unreachable through the graph, which hands the node the
        // outputs it declared, but a discard is never the branch you want on a
        // path whose whole job is not losing frames.
        if outputs.is_empty() || outputs.len() != usize::from(self.ring.layout().count()) {
            return Status::Modified;
        }
        let frames = outputs.get(0).len();
        // The frames the ring holds, then silence for the rest: an underrun
        // yields silence rather than stalling.
        let n = self.ring.pop_planes(outputs.iter_mut(), frames);
        let gain = self.gain.get();
        for plane in outputs.iter_mut() {
            let (heard, rest) = plane.split_at_mut(n);
            heard.iter_mut().for_each(|s| *s *= gain);
            rest.fill(0.0);
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
mod tests;
