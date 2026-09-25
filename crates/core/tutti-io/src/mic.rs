//! Live-input monitoring node — the mic twin of the disk-streaming unit.
//!
//! `DiskSource` (tutti-sampler) drains a ring the *butler* fills off disk;
//! [`MicMonitorNode`] drains a ring a *capture device* fills. Both are
//! `AudioUnit`s with 0 inputs / 2 outputs whose whole job is "pop the next frame
//! the producer pushed, or emit silence on underrun." The producer end lives in
//! the device layer (`tutti-cpal`'s `MicIn`); this node is device-free so it can
//! sit anywhere in the graph — `pipe` it through effects and the mic is heard
//! live and effected while the same ring records to a [`WavOut`](crate::WavOut).
//!
//! # The ring handle
//!
//! The node holds its ring consumer behind [`MicRing`], an
//! `Arc<AudioThreadCell<HeapCons<_>>>`. Two properties matter:
//!
//! - **`Clone`-able.** fundsp clones a node on graph commit. A raw `HeapCons`
//!   (an SPSC consumer) is *not* `Clone` — you can't have two consumers of one
//!   ring. Sharing the one consumer through an `Arc` lets clones coexist; the
//!   single-consumer discipline below keeps that sound.
//! - **Lock-free on the hot path.** [`AudioThreadCell`] hands `&mut` access
//!   through `&self` with no `Mutex` (a debug-only in-use flag catches genuine
//!   concurrent borrows). The ring is single-consumer, so no lock is required —
//!   the same reasoning the streaming reader runs on.
//!
//! # Single-consumer invariant
//!
//! [`HeapCons::try_pop`] advances the read index, so exactly one party may pop.
//! The invariant that keeps the shared `&self`→`&mut` pop sound: **the ring is
//! popped ONLY from `tick`/`process`, and only the graph *backend* clone is
//! ticked.**
//!
//! This needs care because fundsp's `Net::commit` clones every vertex unit and
//! keeps the clone on the main-thread *frontend* net (it swaps the original
//! vertices to the backend — see `Net::commit`). So a second `MicMonitorNode`
//! clone, sharing this exact `Arc`'d consumer, lives on the frontend. The rule
//! is therefore: the frontend clone must **never** touch the ring — hence
//! [`reset`](MicMonitorNode::reset) is a deliberate no-op (fundsp drives `reset`
//! / `set_sample_rate` on the frontend, on the main thread, and popping there
//! would race the backend's `tick`). Only `tick`/`process` pop, and fundsp ticks
//! only the backend copy on the one audio thread, so pops serialize. This is the
//! same discipline `DiskSource` follows — its `reset` likewise leaves
//! the shared `SharedReader` untouched. The device callback only ever *pushes*
//! (the producer half, holding `HeapProd` directly), never pops.

use std::sync::Arc;

use crate::node_id::MIC_MONITOR_ID;
use ringbuf::{traits::Consumer, HeapCons};
use tutti_core::{Amplitude, AudioThreadCell, AudioUnit, BufferMut, BufferRef, SampleRate};

/// A stereo capture-ring consumer, shared across fundsp's graph-commit clones.
///
/// Built by the device layer via [`share_mic_ring`]: it splits a
/// [`HeapRb`](ringbuf::HeapRb), keeps the producer to push captured frames, and
/// hands the consumer here for [`MicMonitorNode::new`]. This is an **opaque
/// handle** — the inner `Arc<AudioThreadCell<HeapCons<_>>>` is deliberately
/// private so the ring's single-consumer invariant (see the module docs) can't
/// be broken by a consumer popping the raw ring off the audio thread. `Clone`
/// shares the one underlying consumer (that's the whole point — fundsp clones
/// the node on graph commit).
#[derive(Clone)]
pub struct MicRing(Arc<AudioThreadCell<HeapCons<[f32; 2]>>>);

impl MicRing {
    /// Pop the next captured frame. Crate-internal so only `MicMonitorNode`'s
    /// audio-thread `tick`/`process` can advance the SPSC read index.
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
/// audio thread. Mirrors `butler::share_reader` for the disk-stream path.
#[must_use = "the returned MicRing is the only handle to the capture ring; drop it and the monitor node has nothing to drain"]
pub fn share_mic_ring(consumer: HeapCons<[f32; 2]>) -> MicRing {
    MicRing(Arc::new(AudioThreadCell::new(consumer)))
}

/// Live microphone monitoring as an `AudioUnit` (0 in → 2 out).
///
/// Drains the capture ring one frame per output sample; an empty ring (the
/// device hasn't pushed yet, or the audio thread outran it) emits silence rather
/// than stalling — the live-source analogue of the streaming unit's underrun
/// hold. No pitch/speed/interpolation: the mic is already at the graph rate
/// (the device layer opens it so), so this is a straight per-frame passthrough.
///
/// `Clone` shares the one ring consumer (fundsp clones the node on graph commit;
/// only one clone is ticked per buffer — see the module's single-consumer note).
#[derive(Clone, Debug)]
pub struct MicMonitorNode {
    ring: MicRing,
    gain: Amplitude,
    /// The rate the device layer opened the mic at, when it said so.
    ///
    /// `None` for a node built by hand, which cannot know. When it is known,
    /// `set_sample_rate` debug-asserts the graph agrees — the unchecked half
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

impl AudioUnit for MicMonitorNode {
    /// Never forked: a clone shares the mic ring's consumer (`MicRing`), so a
    /// fork would take live input frames and race the read index, and there
    /// is no second consumer to sever it onto — the ring is SPSC.
    fn forkable(&self) -> bool {
        false
    }

    fn inputs(&self) -> usize {
        0
    }

    /// Stereo, deliberately — unlike the voice units, which take a runtime width.
    ///
    /// The producer is the CPAL input callback in `tutti-cpal`, which downmixes
    /// each interleaved device frame to a stereo pair before it ever reaches
    /// [`MicRing`] (a `HeapCons<[f32; 2]>`). Widening this node without widening
    /// that callback and the ring would declare an arity its own source can
    /// never fill, so live capture stays stereo until the device edge is
    /// widened with it. That edge is app-side, not engine-side.
    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        // Deliberately does NOT touch the ring. fundsp's commit clones every
        // vertex unit (Net::commit swaps original vertices to the backend, keeps
        // the clones on the frontend), so a `MicMonitorNode` clone lives on the
        // main-thread frontend net sharing this same `Arc`'d consumer. `reset`
        // can be driven on that frontend clone (via `Net::reset` /
        // `set_sample_rate`) concurrently with the backend clone's `tick` on the
        // audio thread. Popping here would be a second `&mut` into the shared
        // `HeapCons` from another thread — a data race on the SPSC read index.
        //
        // So the ring is popped ONLY from `tick`/`process` (the single backend
        // consumer), exactly as `DiskSource::reset` leaves its shared
        // `SharedReader` untouched. The monitor ring self-limits to ~10ms, so
        // there's no stale backlog worth draining anyway.
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        // Nothing to reconfigure — this node does not resample. But the claim
        // that it does not *need* to was, until now, only a comment: the
        // device layer opens the mic at the graph's rate, and nothing checked
        // it. `MicIn::open` is the checked half (it returns
        // `Error::SampleRateMismatch`); this is the unchecked half, in the
        // same shape as `pump`'s layout `debug_assert` beside
        // `Recorder::start`'s returned error.
        debug_assert!(
            self.device_rate.is_none_or(|r| r == sample_rate),
            "mic opened at {:?} but the graph runs at {sample_rate:?} — \
             MicMonitorNode does not resample, so this drifts silently",
            self.device_rate
        );
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        // Width check BEFORE the pop, not after. `next_frame` consumes from the
        // ring, so popping first and then declining to write threw the frame
        // away: a caller that handed over a short buffer lost captured audio
        // with nothing to show for it. Unreachable through fundsp — a 2-out
        // node is always given a 2-wide buffer — but a discard is never the
        // branch you want on a path whose whole job is not losing frames, and
        // ordering the check first costs nothing.
        if output.len() < 2 {
            return;
        }
        let (l, r) = self.next_frame();
        output[0] = l;
        output[1] = r;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let (l, r) = self.next_frame();
            output.set_f32(0, i, l);
            output.set_f32(1, i, r);
        }
    }

    // Written out rather than macro-expanded: `audio_unit_boilerplate!` is
    // tutti-sampler's, and this crate exists precisely to not depend on that
    // one. One unit's worth of tail methods is cheaper than a shared macro
    // crate.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn get_id(&self) -> u64 {
        MIC_MONITOR_ID
    }

    fn route(
        &mut self,
        _input: &tutti_core::SignalFrame,
        _frequency: f64,
    ) -> tutti_core::SignalFrame {
        tutti_core::SignalFrame::new(2)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_core::BufferVec;

    fn ring_with(frames: &[[f32; 2]]) -> (MicRing, ringbuf::HeapProd<[f32; 2]>) {
        let rb = HeapRb::<[f32; 2]>::new(64);
        let (mut prod, cons) = rb.split();
        for &f in frames {
            let _ = prod.try_push(f);
        }
        (share_mic_ring(cons), prod)
    }

    #[test]
    fn tick_drains_the_ring_then_goes_silent() {
        let (ring, _prod) = ring_with(&[[1.0, 2.0], [3.0, 4.0]]);
        let mut node = MicMonitorNode::new(ring);

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert_eq!(out, [1.0, 2.0]);
        node.tick(&[], &mut out);
        assert_eq!(out, [3.0, 4.0]);

        // Ring drained → underrun → silence, not a stall.
        node.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn process_block_drains_and_pads_with_silence() {
        let (ring, _prod) = ring_with(&[[1.0, -1.0], [2.0, -2.0]]);
        let mut node = MicMonitorNode::new(ring);

        let mut buf = BufferVec::new(2);
        let mut bm = buf.buffer_mut();
        let empty = BufferRef::new(&[]);
        node.process(4, &empty, &mut bm);

        assert_eq!((bm.at_f32(0, 0), bm.at_f32(1, 0)), (1.0, -1.0));
        assert_eq!((bm.at_f32(0, 1), bm.at_f32(1, 1)), (2.0, -2.0));
        // Frames 2,3: ring empty → silence.
        assert_eq!((bm.at_f32(0, 2), bm.at_f32(1, 2)), (0.0, 0.0));
        assert_eq!((bm.at_f32(0, 3), bm.at_f32(1, 3)), (0.0, 0.0));
    }

    #[test]
    fn gain_scales_the_monitored_signal() {
        let (ring, _prod) = ring_with(&[[1.0, 1.0]]);
        let mut node = MicMonitorNode::with_gain(ring, Amplitude::new(0.5));

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn reset_does_not_consume_the_ring() {
        // reset() must NOT pop — the frontend clone can be reset on the main
        // thread while the backend clone ticks, and a pop there would race the
        // backend. So after reset, the frames are still there for `tick`.
        let (ring, _prod) = ring_with(&[[9.0, 9.0]]);
        let mut node = MicMonitorNode::new(ring);
        node.reset();

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert_eq!(out, [9.0, 9.0], "reset left the ring untouched");
    }

    /// **The module's central claim, which had no test.**
    ///
    /// `reset`'s comment is an argument about clones: `Net::commit` swaps
    /// vertices to the backend and keeps clones on the frontend, so a
    /// `MicMonitorNode` clone lives on the main thread sharing this same
    /// `Arc`'d consumer, and the frontend's `reset`/`set_sample_rate` can run
    /// concurrently with the backend's `tick`. That is the entire reason
    /// `reset` is empty. Nothing simulated the scenario — `reset_does_not_
    /// consume_the_ring` resets and ticks *the same node*, which is not the
    /// shape the comment is about.
    ///
    /// Here the clone is made explicitly, the frontend half is driven, and the
    /// backend half must still find every frame.
    ///
    /// Mutation-checked: making `reset` drain the ring, or
    /// `set_sample_rate` call `next_frame`, fails this while leaving
    /// `reset_does_not_consume_the_ring` green.
    #[test]
    fn a_frontend_clone_driven_alongside_the_backend_consumes_nothing() {
        let (ring, _prod) = ring_with(&[[1.0, 2.0], [3.0, 4.0]]);
        let backend = MicMonitorNode::new(ring);
        // What `Net::commit` leaves on the frontend: a clone over the same ring.
        let mut frontend = backend.clone();
        let mut backend = backend;

        // Everything the graph drives on a frontend vertex.
        frontend.reset();
        frontend.set_sample_rate(tutti_core::SampleRate::SR_48K);

        let mut out = [0.0f32; 2];
        backend.tick(&[], &mut out);
        assert_eq!(out, [1.0, 2.0], "the frontend clone must not have popped");
        backend.tick(&[], &mut out);
        assert_eq!(out, [3.0, 4.0], "nor on the second frame");
    }

    /// **A short output buffer must not eat a captured frame.**
    ///
    /// `tick` used to call `next_frame` — which pops — and only then check the
    /// width, so a caller handing over a 1-wide buffer lost the frame with
    /// nothing written. Unreachable through fundsp, which always gives a 2-out
    /// node a 2-wide buffer, so this was latent rather than live; it is fixed
    /// because a discard is never the branch you want on a path whose job is
    /// not losing frames.
    ///
    /// Mutation-checked: restoring the pop-then-check order fails this.
    #[test]
    fn a_short_output_buffer_leaves_the_frame_in_the_ring() {
        let (ring, _prod) = ring_with(&[[7.0, 8.0]]);
        let mut node = MicMonitorNode::new(ring);

        let mut narrow = [0.0f32; 1];
        node.tick(&[], &mut narrow);
        assert_eq!(
            narrow,
            [0.0],
            "nothing may be written to a too-narrow buffer"
        );

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert_eq!(
            out,
            [7.0, 8.0],
            "the frame the narrow tick could not write must still be there"
        );
    }

    /// The `AudioUnit` tail methods, which nothing named.
    ///
    /// Cheap, and not ceremony: `get_id` is how the graph's node-type dispatch
    /// recognises this unit, so a wrong constant misroutes silently, and
    /// `route`'s width is what tells fundsp the node is stereo.
    #[test]
    fn the_declared_shape_matches_what_the_graph_is_told() {
        let (ring, _prod) = ring_with(&[]);
        let mut node = MicMonitorNode::new(ring);

        assert_eq!(node.inputs(), 0, "a capture source takes no graph input");
        assert_eq!(node.outputs(), 2);
        assert_eq!(node.get_id(), crate::node_id::MIC_MONITOR_ID);
        assert_eq!(
            node.route(&tutti_core::SignalFrame::new(0), 48_000.0).len(),
            2,
            "the routing frame must match `outputs()`, or fundsp plans the \
             wrong width"
        );
        assert!(node.footprint() >= std::mem::size_of::<MicMonitorNode>());
        assert!(
            node.as_any().downcast_ref::<MicMonitorNode>().is_some(),
            "downcasting is how a host reaches back to the concrete node"
        );
        assert!(node.as_any_mut().downcast_mut::<MicMonitorNode>().is_some());
    }

    /// **The mic monitor refuses to be forked.** A clone shares the ring's
    /// one consumer, so a fork rendering beside the live graph would take
    /// live frames — the clone below drains what the original then never
    /// sees, `isolate` or not. A graph fork trusts `forkable()`, so it must
    /// say `false`.
    ///
    /// Mutation: drop the `forkable` override (the default is `true`) →
    /// fails.
    #[test]
    fn a_clone_steals_live_frames_so_it_is_not_forkable() {
        let (ring, _prod) = ring_with(&[[1.0, 2.0]]);
        let mut live = MicMonitorNode::new(ring);
        let mut clone = live.clone();
        clone.isolate();
        let mut out = [0.0f32; 2];
        clone.tick(&[], &mut out);
        assert_eq!(out, [1.0, 2.0], "the clone read the live ring");
        live.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "and the live node lost the frame");
        assert!(!live.forkable());
    }
}
