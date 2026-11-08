//! Taps: copy-out points of the compiled plan.
//!
//! A tap is declared on the [`Topology`](tutti_types::Topology) as "copy these
//! sources" ([`TapSpec`](tutti_types::TapSpec)); the compiler emits one
//! [`Op::Tap`](crate::Op::Tap) per tap, reading every lane's source, and, for an
//! aligned tap, the same delay an output channel gets for each. A tap that is not in the topology compiles to
//! nothing.
//!
//! The ring each lane writes is `ringbuf`'s single-producer, single-consumer
//! heap ring, already this crate's dependency for the editor-to-executor queue;
//! no new lock-free structure. The producer is owned by the executor (it rides
//! a commit in, like a unit), the consumer by the caller, so the audio thread
//! takes no lock and frees nothing: a producer left behind by a plan that no
//! longer names it is retired with the old state, on the control side.
//!
//! The lanes of a tap are written together by one op and read by whole
//! frames, so they cannot tear: frame `n` of every lane is the same frame,
//! whatever the reader's speed (see [`TapWriter`]).
//!
//! Dropping the [`TapReader`] first is safe. The tap then finds no reader and
//! writes nothing, and the rings are freed with the writer when the tap is
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

/// The executor's end of a tap: every lane's ring producer, written together.
///
/// A tap's lanes are rings of the same capacity that the executor fills with
/// the same number of frames each block and the reader drains by whole frames,
/// so frame `n` of every lane is the same frame by construction. One thread
/// writes and one reads, which is why the minimums below are safe: a ring's
/// vacancy only grows under the reader and its occupancy only grows under the
/// writer, so a count taken before a push can only be an underestimate.
pub struct TapWriter {
    lanes: Vec<HeapProd<f32>>,
    /// Frames the rings refused, shared with the reader.
    lost: Arc<AtomicU64>,
}

impl std::fmt::Debug for TapWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapWriter").finish_non_exhaustive()
    }
}

impl Drop for TapWriter {
    fn drop(&mut self) {
        RtScope::check_outside("a tap writer");
    }
}

impl TapWriter {
    /// How many lanes (channels) this writer fills.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lanes.len()
    }

    /// Writes a block of whole interleaved frames, `width` samples a frame
    /// (a tail shorter than a frame is ignored), to the lanes together: every
    /// lane takes the same count, the fewest frames any ring has room for, and
    /// the rest is counted lost, once. With no reader there is nothing to
    /// write or count. Audio thread: allocates nothing, locks nothing.
    #[inline]
    pub fn push_interleaved(&mut self, block: &[f32]) {
        let width = self.lanes.len();
        if width == 0 || !self.lanes.first().is_some_and(Observer::read_is_held) {
            return;
        }
        let frames = block.len() / width;
        let room = self
            .lanes
            .iter()
            .map(Observer::vacant_len)
            .min()
            .unwrap_or(0);
        let take = room.min(frames);
        for (channel, lane) in self.lanes.iter_mut().enumerate() {
            let pushed = lane.push_iter(
                block
                    .iter()
                    .skip(channel)
                    .step_by(width)
                    .copied()
                    .take(take),
            );
            debug_assert_eq!(pushed, take, "room was counted across every lane");
        }
        self.count_lost(frames - take);
    }

    /// Counts `frames` as lost without writing them: a block this writer
    /// could not take (a device of another width).
    #[inline]
    pub fn skip(&mut self, frames: usize) {
        if self.lanes.first().is_some_and(Observer::read_is_held) {
            self.count_lost(frames);
        }
    }

    #[inline]
    fn count_lost(&self, frames: usize) {
        if frames > 0 {
            self.lost.fetch_add(frames as u64, Ordering::Relaxed);
        }
    }
}

/// [`Op::Tap`](crate::Op::Tap): the first `frames` samples of each of `srcs`
/// into the tap's lanes. Every lane takes the same count, the fewest frames
/// any ring has room for, and the rest is counted lost, once for the tap. With
/// no reader there is nothing to write or count.
#[inline]
pub(crate) fn tap_op<A: AudioSlots>(tap: &mut TapWriter, arena: &A, srcs: &[u32], frames: usize) {
    if !tap.lanes.first().is_some_and(Observer::read_is_held) {
        return;
    }
    let room = tap
        .lanes
        .iter()
        .map(Observer::vacant_len)
        .min()
        .unwrap_or(0);
    let take = room.min(frames);
    for (lane, &src) in tap.lanes.iter_mut().zip(srcs) {
        let pushed = lane.push_slice(&arena.slot(src, frames)[..take]);
        debug_assert_eq!(pushed, take, "room was counted across every lane");
    }
    if take < frames {
        tap.lost
            .fetch_add((frames - take) as u64, Ordering::Relaxed);
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
    /// A reader and the writer the executor will fill it with: `width` lanes,
    /// each holding `capacity` frames (at least one).
    pub(crate) fn pair(id: TapId, width: usize, capacity: Samples) -> (Self, TapWriter) {
        let lost = Arc::new(AtomicU64::new(0));
        let (cons, prods): (Vec<_>, Vec<_>) = (0..width)
            .map(|_| HeapRb::<f32>::new(capacity.get().max(1)).split())
            .map(|(prod, cons)| (cons, prod))
            .unzip();
        let reader = Self {
            id,
            lanes: cons,
            lost: Arc::clone(&lost),
            alive: Arc::new(()),
        };
        (reader, TapWriter { lanes: prods, lost })
    }

    /// A reader and its writer, for a writer that is not an op of a plan (the
    /// engine's, after the fader): `width` lanes of `capacity` frames each.
    #[must_use]
    pub fn standalone(id: TapId, width: usize, capacity: Samples) -> (Self, TapWriter) {
        Self::pair(id, width, capacity)
    }

    /// A handle to see whether this reader is still alive: its strong count
    /// is zero once the reader is dropped.
    #[must_use]
    pub fn watch(&self) -> Weak<()> {
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

    /// Frames readable now: the fewest across lanes. The writer fills every
    /// lane by the same count, so a lane may briefly hold more than the
    /// others mid-block, and a read never goes past the shortest.
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

    /// Pops up to `want` whole frames into `planes`, one slice per lane in lane
    /// order, and returns the frame count: each plane's first frames are
    /// written and the rest of it is as it was. A lane with no plane (the
    /// iterator ends early) still drops the same frames, so the lanes stay
    /// level; a plane shorter than the count is the caller's to size
    /// (`want` at most its length).
    ///
    /// The shape of a graph node's outputs: no interleaved scratch to
    /// allocate, and the lanes copy straight out of their rings.
    ///
    /// # Panics
    ///
    /// If a plane is shorter than the frames popped.
    pub fn pop_planes<'p>(
        &mut self,
        planes: impl IntoIterator<Item = &'p mut [f32]>,
        want: Samples,
    ) -> Samples {
        let frames = self.available().get().min(want.get());
        let mut planes = planes.into_iter();
        for lane in &mut self.lanes {
            match planes.next() {
                Some(plane) => {
                    let popped = lane.pop_slice(&mut plane[..frames]);
                    debug_assert_eq!(popped, frames, "available counted every lane");
                }
                None => {
                    lane.skip(frames);
                }
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
