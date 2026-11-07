//! Taps: copy-out points of the compiled plan.
//!
//! A tap is declared on the [`Topology`](tutti_types::Topology) as "copy these
//! sources" ([`TapSpec`](tutti_types::TapSpec)); the compiler emits one
//! [`Op::Tap`](crate::Op::Tap) per source and, for an aligned tap, the same
//! delay an output channel gets. A tap that is not in the topology compiles to
//! nothing.
//!
//! The ring each lane writes is `ringbuf`'s single-producer, single-consumer
//! heap ring, already this crate's dependency for the editor-to-executor queue;
//! no new lock-free structure. The producer is owned by the executor (it rides
//! a commit in, like a unit), the consumer by the caller, so the audio thread
//! takes no lock and frees nothing: a producer left behind by a plan that no
//! longer names it is retired with the old state, on the control side.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_types::Samples;

use crate::slots::AudioSlots;

/// The executor's end of one lane.
pub(crate) struct TapLane {
    prod: HeapProd<f32>,
    lost: Arc<AtomicU64>,
}

impl std::fmt::Debug for TapLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapLane").finish_non_exhaustive()
    }
}

/// [`Op::Tap`](crate::Op::Tap): slot `src` into the lane's ring. What does not
/// fit is counted, never silently dropped.
#[inline]
pub(crate) fn tap_op<A: AudioSlots>(lane: &mut TapLane, arena: &A, src: u32, frames: usize) {
    let s = arena.slot(src, frames);
    let pushed = lane.prod.push_slice(s);
    if pushed < s.len() {
        lane.lost
            .fetch_add((s.len() - pushed) as u64, Ordering::Relaxed);
    }
}

/// The caller's end of a tap: every lane, read together.
pub struct TapReader {
    lanes: Vec<HeapCons<f32>>,
    lost: Arc<AtomicU64>,
}

impl std::fmt::Debug for TapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapReader")
            .field("width", &self.lanes.len())
            .finish_non_exhaustive()
    }
}

impl TapReader {
    /// A reader and the lanes the executor will write, `width` of them, each
    /// holding `capacity` frames.
    pub(crate) fn pair(width: usize, capacity: usize) -> (Self, Vec<TapLane>) {
        let lost = Arc::new(AtomicU64::new(0));
        let (lanes, prods): (Vec<_>, Vec<_>) = (0..width)
            .map(|_| {
                let (prod, cons) = HeapRb::<f32>::new(capacity).split();
                (
                    cons,
                    TapLane {
                        prod,
                        lost: Arc::clone(&lost),
                    },
                )
            })
            .unzip();
        (Self { lanes, lost }, prods)
    }

    /// How many lanes (channels) this tap carries.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lanes.len()
    }

    /// Frames readable now (the fewest across lanes, so a read is whole).
    #[must_use]
    pub fn available(&self) -> Samples {
        Samples(
            self.lanes
                .iter()
                .map(|l| l.occupied_len())
                .min()
                .unwrap_or(0),
        )
    }

    /// Pops whole frames into `out`, interleaved `width` samples a frame, as
    /// many as fit; returns the frame count.
    pub fn pop_into(&mut self, out: &mut [f32]) -> Samples {
        let w = self.width();
        if w == 0 {
            return Samples(0);
        }
        let n = self.available().0.min(out.len() / w);
        for (c, lane) in self.lanes.iter_mut().enumerate() {
            for i in 0..n {
                out[i * w + c] = lane.try_pop().unwrap_or(0.0);
            }
        }
        Samples(n)
    }

    /// Pops up to `out.len()` frames of one lane, planar; returns the count.
    /// One copy per lane, for a reader that wants each channel apart.
    pub fn pop_lane(&mut self, lane: usize, out: &mut [f32]) -> Samples {
        Samples(self.lanes[lane].pop_slice(out))
    }

    /// Frames the executor could not push since the last call, and clears the
    /// count: nonzero is a gap in what was read.
    pub fn take_lost(&self) -> Samples {
        Samples(self.lost.swap(0, Ordering::Relaxed) as usize)
    }
}
