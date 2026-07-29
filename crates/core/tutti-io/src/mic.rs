//! Live-input monitoring node — the mic twin of the disk-streaming unit.
//!
//! `DiskSource`
//! drains a ring the *butler* fills off disk; [`MicMonitorNode`] drains a ring
//! a *capture device* fills. Both are `AudioUnit`s with 0 inputs / 2 outputs
//! whose whole job is "pop the next frame the producer pushed, or emit silence
//! on underrun." The producer end lives in the device layer (`tutti-cpal`'s
//! `MicIn`); this node is device-free so it can sit anywhere in the graph —
//! `pipe` it through effects and you hear the mic live, effected, while
//! recording the same ring to a `WavOut`.
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
//!   concurrent borrows). This mirrors why the streaming reader dropped its old
//!   `Arc<Mutex<_>>` — the ring is single-consumer, so no lock is required.
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
use tutti_core::{Amplitude, AudioThreadCell, AudioUnit, BufferMut, BufferRef};

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
        // The inner `HeapCons` isn't `Debug` and its occupancy is a hot-path
        // read we shouldn't take here; a shared-count summary is enough.
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
}

impl MicMonitorNode {
    /// Build a monitor node over a shared capture ring at unity gain.
    pub fn new(ring: MicRing) -> Self {
        Self {
            ring,
            gain: Amplitude::new(1.0),
        }
    }

    /// Build a monitor node over a shared capture ring at `gain`.
    pub fn with_gain(ring: MicRing, gain: Amplitude) -> Self {
        Self { ring, gain }
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

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {
        // No-op: the device layer opens the mic at the graph's sample rate, so
        // there's no resampling to reconfigure here.
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        let (l, r) = self.next_frame();
        if output.len() >= 2 {
            output[0] = l;
            output[1] = r;
        }
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

    #[test]
    fn clone_shares_the_same_ring() {
        // A commit-clone must drain the SAME ring, not a fresh empty one.
        let (ring, _prod) = ring_with(&[[7.0, 8.0]]);
        let node = MicMonitorNode::new(ring);
        let mut clone = node.clone();

        let mut out = [0.0f32; 2];
        clone.tick(&[], &mut out);
        assert_eq!(out, [7.0, 8.0], "clone drains the shared ring");
    }
}
