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
//!
//! Dropping the [`TapReader`] first is safe. The lane then finds no reader and
//! writes nothing, and the ring is freed with the producer when the tap is
//! closed ([`Editor::close_tap`](crate::Editor::close_tap)) or reaped
//! ([`Editor::reap_taps`](crate::Editor::reap_taps)), on the control side.
//! Dropping the reader last frees the ring on the reader's thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_rt::RtScope;
use tutti_types::{ChannelLayout, Samples, TapId};

use crate::slots::AudioSlots;

/// The executor's end of one lane.
pub(crate) struct TapLane {
    prod: HeapProd<f32>,
    /// Where this lane counts the frames its ring refused. Only the first
    /// lane of a tap holds it: the lanes fill and drain in step, so one
    /// lane's loss is the tap's, counted once.
    lost: Option<Arc<AtomicU64>>,
}

impl std::fmt::Debug for TapLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapLane").finish_non_exhaustive()
    }
}

impl Drop for TapLane {
    fn drop(&mut self) {
        RtScope::check_outside("a tap lane");
    }
}

/// [`Op::Tap`](crate::Op::Tap): slot `src` into the lane's ring. What does not
/// fit is counted, never silently dropped; with no reader there is nothing to
/// count or write.
#[inline]
pub(crate) fn tap_op<A: AudioSlots>(lane: &mut TapLane, arena: &A, src: u32, frames: usize) {
    if !lane.prod.read_is_held() {
        return;
    }
    let block = arena.slot(src, frames);
    let pushed = lane.prod.push_slice(block);
    if let Some(lost) = &lane.lost
        && pushed < block.len()
    {
        lost.fetch_add((block.len() - pushed) as u64, Ordering::Relaxed);
    }
}

/// The caller's end of a tap: every lane, read together.
///
/// Drain it from a thread of your own. Dropping it leaves the tap open until
/// the editor closes or reaps it; the audio thread then writes nothing for it.
/// It must not be dropped on the audio thread (debug builds panic there).
pub struct TapReader {
    id: TapId,
    lanes: Vec<HeapCons<f32>>,
    lost: Arc<AtomicU64>,
    /// The editor holds a `Weak` of this: when it dies, the reader is gone.
    alive: Arc<()>,
}

impl std::fmt::Debug for TapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapReader")
            .field("id", &self.id)
            .field("width", &self.lanes.len())
            .finish_non_exhaustive()
    }
}

impl Drop for TapReader {
    fn drop(&mut self) {
        RtScope::check_outside("a tap reader");
    }
}

impl TapReader {
    /// A reader and the lanes the executor will write, `width` of them, each
    /// holding `capacity` frames (at least one).
    pub(crate) fn pair(id: TapId, width: usize, capacity: Samples) -> (Self, Vec<TapLane>) {
        let lost = Arc::new(AtomicU64::new(0));
        let (lanes, prods): (Vec<_>, Vec<_>) = (0..width)
            .map(|lane| {
                let (prod, cons) = HeapRb::<f32>::new(capacity.get().max(1)).split();
                let lost = (lane == 0).then(|| Arc::clone(&lost));
                (cons, TapLane { prod, lost })
            })
            .unzip();
        let reader = Self {
            id,
            lanes,
            lost,
            alive: Arc::new(()),
        };
        (reader, prods)
    }

    /// A handle the editor keeps to see whether this reader is still alive.
    pub(crate) fn watch(&self) -> Weak<()> {
        Arc::downgrade(&self.alive)
    }

    /// The tap's id, the one [`Editor::close_tap`](crate::Editor::close_tap) takes.
    #[must_use]
    pub fn id(&self) -> TapId {
        self.id
    }

    /// How many lanes (channels) this tap carries.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lanes.len()
    }

    /// The channel layout of an interleaved read: `width` channels.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.lanes.len().try_into().unwrap_or(u16::MAX))
    }

    /// Frames readable now (the fewest across lanes, so a read is whole).
    #[must_use]
    pub fn available(&self) -> Samples {
        Samples(
            self.lanes
                .iter()
                .map(Observer::occupied_len)
                .min()
                .unwrap_or(0),
        )
    }

    /// Pops whole frames into `out`, interleaved `width` samples a frame, as
    /// many as fit; returns the frame count. A tail of `out` shorter than a
    /// frame is left as it was.
    pub fn pop_into(&mut self, out: &mut [f32]) -> Samples {
        let width = self.width();
        if width == 0 {
            return Samples(0);
        }
        let frames = self.available().get().min(out.len() / width);
        for (channel, lane) in self.lanes.iter_mut().enumerate() {
            let slots = out[..frames * width]
                .iter_mut()
                .skip(channel)
                .step_by(width);
            for (slot, sample) in slots.zip(lane.pop_iter()) {
                *slot = sample;
            }
        }
        Samples(frames)
    }

    /// Frames the executor could not push since the last call, and clears the
    /// count: nonzero is a gap in what was read.
    pub fn take_lost(&self) -> Samples {
        Samples(usize::try_from(self.lost.swap(0, Ordering::Relaxed)).unwrap_or(usize::MAX))
    }
}
