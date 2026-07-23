//! Live-input monitoring node — the mic twin of [`StreamingSamplerUnit`].
//!
//! [`StreamingSamplerUnit`](super::streaming_sampler::StreamingSamplerUnit)
//! drains a ring the *butler* fills off disk; [`MicMonitorNode`] drains a ring
//! a *capture device* fills. Both are `AudioUnit`s with 0 inputs / 2 outputs
//! whose whole job is "pop the next frame the producer pushed, or emit silence
//! on underrun." The producer end lives in the device layer (`bevy-tutti`'s
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
//! Here the **audio thread** is that sole popper: `tick`/`process` are the only
//! callers, and though fundsp may hold several clones of one `MicRing`, only one
//! is ticked per buffer and all live on the one audio thread, so pops serialize.
//! The device callback only ever *pushes* (the producer half), never pops.

use std::sync::Arc;

use ringbuf::{traits::Consumer, HeapCons};
use tutti_core::{AudioThreadCell, AudioUnit, BufferMut, BufferRef, Linear};

/// A stereo capture-ring consumer, shared across fundsp's graph-commit clones.
///
/// Built by the device layer: it splits a [`HeapRb`](ringbuf::HeapRb), keeps the
/// producer to push captured frames, and wraps the consumer in this handle for
/// [`MicMonitorNode::new`]. See the module docs for the single-consumer
/// invariant that makes the shared `&self` pop sound.
pub type MicRing = Arc<AudioThreadCell<HeapCons<[f32; 2]>>>;

/// Wrap a freshly-split ring consumer into a [`MicRing`] for handoff to the
/// audio thread. Mirrors `butler::share_reader` for the disk-stream path.
pub fn share_mic_ring(consumer: HeapCons<[f32; 2]>) -> MicRing {
    Arc::new(AudioThreadCell::new(consumer))
}

/// Live microphone monitoring as an `AudioUnit` (0 in → 2 out).
///
/// Drains the capture ring one frame per output sample; an empty ring (the
/// device hasn't pushed yet, or the audio thread outran it) emits silence rather
/// than stalling — the live-source analogue of the streaming unit's underrun
/// hold. No pitch/speed/interpolation: the mic is already at the graph rate
/// (the device layer opens it so), so this is a straight per-frame passthrough.
pub struct MicMonitorNode {
    ring: MicRing,
    gain: Linear,
}

impl MicMonitorNode {
    /// Build a monitor node over a shared capture ring at unity gain.
    pub fn new(ring: MicRing) -> Self {
        Self {
            ring,
            gain: Linear::new(1.0),
        }
    }

    /// Build a monitor node over a shared capture ring at `gain`.
    pub fn with_gain(ring: MicRing, gain: Linear) -> Self {
        Self { ring, gain }
    }

    /// Pop the next captured frame, or `(0.0, 0.0)` on underrun. Audio-thread
    /// only (see the single-consumer invariant in the module docs).
    #[inline]
    fn next_frame(&self) -> (f32, f32) {
        let g = self.gain.get();
        match self.ring.borrow_mut().try_pop() {
            Some([l, r]) => (l * g, r * g),
            None => (0.0, 0.0),
        }
    }
}

impl Clone for MicMonitorNode {
    fn clone(&self) -> Self {
        // Share the one ring consumer; only one clone is ticked per buffer.
        Self {
            ring: Arc::clone(&self.ring),
            gain: self.gain,
        }
    }
}

impl AudioUnit for MicMonitorNode {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        // Discard whatever the device buffered so monitoring resumes from
        // "now" rather than replaying a stale backlog.
        let mut cons = self.ring.borrow_mut();
        while cons.try_pop().is_some() {}
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

    audio_unit_boilerplate!(id = crate::node_id::MIC_MONITOR_ID, outputs = 2);
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
        let mut node = MicMonitorNode::with_gain(ring, Linear::new(0.5));

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert_eq!(out, [0.5, 0.5]);
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
