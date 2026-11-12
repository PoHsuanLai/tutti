//! Taps: copy-out points of the compiled plan.
//!
//! A tap is declared on the [`Topology`](crate::Topology) as "copy these
//! sources" ([`TapSpec`](crate::TapSpec)); the compiler emits one
//! [`Op::Tap`](crate::Op::Tap) per tap, reading every lane's source, and, for an
//! aligned tap, the same delay an output channel gets for each. The outputs
//! form ([`TapSpec::Outputs`](crate::TapSpec::Outputs)) reads what each output
//! channel carries, after the alignment delays and the sum of a fan-in. A tap
//! that is not in the topology compiles to nothing. Opening a tap refuses a source that does not exist then
//! ([`MissingSource`](crate::MissingSource)); a lane whose port goes missing
//! later (a node replaced by a narrower one, a removed node) reads silence and
//! the commit succeeds; the reader is not told, and the zeros count as neither
//! data nor loss.
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

use crate::TapId;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_rt::RtScope;
use tutti_types::{ChannelLayout, Frame, Pull, Samples, Source};

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
    /// One [`Run`] for each block written, pushed before its samples.
    runs: HeapProd<Run>,
    /// The frame the next block of a writer that is not an op of a plan
    /// starts at: the frames offered so far, written or lost.
    next: Frame,
    /// Frames the rings refused, shared with the reader.
    lost: Arc<AtomicU64>,
}

/// The graph frame a block's samples start at, and how many of them the rings
/// took (a block the rings had no room for whole is a gap between two runs,
/// never a run of its own). The reader's clock ([`TapReader::next_frame`]).
#[derive(Clone, Copy, Debug)]
struct Run {
    start: Frame,
    frames: usize,
}

/// The most blocks a ring holds the start frame of at once, however deep its
/// lanes: a block that finds this many waiting is refused whole and counted
/// lost, like one the lanes have no room for. A ring of fewer frames than this
/// cannot hold more runs than frames.
const MAX_RUNS: usize = 4096;

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
    ///
    /// The block's frame is the count of frames offered before it: a writer
    /// that is not an op of a plan numbers its frames from zero.
    #[inline]
    pub fn push_interleaved(&mut self, block: &[f32]) {
        let width = self.lanes.len();
        if width == 0 || !self.lanes.first().is_some_and(Observer::read_is_held) {
            return;
        }
        let frames = block.len() / width;
        let start = self.next;
        self.next = start + Samples(frames);
        let room = self.room(frames);
        let take = room.min(frames);
        if take > 0 {
            self.note(start, take);
        }
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
            self.next += Samples(frames);
            self.count_lost(frames);
        }
    }

    /// Frames every lane has room for, and zero when the run ring is full.
    #[inline]
    fn room(&self, frames: usize) -> usize {
        if self.runs.is_full() || frames == 0 {
            return 0;
        }
        self.lanes
            .iter()
            .map(Observer::vacant_len)
            .min()
            .unwrap_or(0)
    }

    /// Records that `frames` frames starting at `start` are about to be
    /// pushed. Before the samples, so a reader that sees a sample sees its run.
    #[inline]
    fn note(&mut self, start: Frame, frames: usize) {
        let pushed = self.runs.try_push(Run { start, frames });
        debug_assert!(pushed.is_ok(), "room() counted the run ring");
    }

    #[inline]
    fn count_lost(&self, frames: usize) {
        if frames > 0 {
            self.lost.fetch_add(frames as u64, Ordering::Relaxed);
        }
    }
}

/// [`Op::Tap`](crate::Op::Tap): the first `frames` samples of each of `srcs`
/// into the tap's lanes, the block starting at graph frame `start`. Every lane
/// takes the same count, the fewest frames any ring has room for, and the rest
/// is counted lost, once for the tap. With no reader there is nothing to write
/// or count.
#[inline]
pub(crate) fn tap_op<A: AudioSlots>(
    tap: &mut TapWriter,
    arena: &A,
    srcs: &[u32],
    start: Frame,
    frames: usize,
) {
    if !tap.lanes.first().is_some_and(Observer::read_is_held) {
        return;
    }
    let take = tap.room(frames).min(frames);
    if take > 0 {
        tap.note(start, take);
    }
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
    /// The writer's [`Run`]s not yet read through.
    runs: HeapCons<Run>,
    /// The graph frame of the next frame to hand out, while `left` is not
    /// zero; once a run is read through, the frame after it.
    at: Frame,
    /// Frames of the current run not yet handed out.
    left: usize,
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
    /// A reader and the writer that fills it: `width` lanes, each holding
    /// `capacity` frames (at least one). A plan's tap gets one from the editor;
    /// a writer that is not an op of a plan (the engine's, after the fader; an
    /// inlet's producer) takes one here.
    #[must_use]
    pub fn pair(id: TapId, width: usize, capacity: Samples) -> (Self, TapWriter) {
        let lost = Arc::new(AtomicU64::new(0));
        let (run_prod, run_cons) = HeapRb::<Run>::new(capacity.get().clamp(1, MAX_RUNS)).split();
        let (cons, prods): (Vec<_>, Vec<_>) = (0..width)
            .map(|_| HeapRb::<f32>::new(capacity.get().max(1)).split())
            .map(|(prod, cons)| (cons, prod))
            .unzip();
        let reader = Self {
            id,
            lanes: cons,
            lost: Arc::clone(&lost),
            runs: run_cons,
            at: Frame::ZERO,
            left: 0,
            alive: Arc::new(()),
        };
        let writer = TapWriter {
            lanes: prods,
            runs: run_prod,
            next: Frame::ZERO,
            lost,
        };
        (reader, writer)
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
        self.consume(frames);
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
        self.consume(frames);
        Samples(frames)
    }

    /// The graph frame of the next frame this reader hands out: the clock a
    /// consumer places what it reads on.
    ///
    /// The executor stamps each block it writes with the graph frame it starts
    /// at ([`Env::frame`](crate::Env)), the reader keeps the count, and a block
    /// the rings had no room for (counted by [`take_lost`](Self::take_lost))
    /// advances it too: once a later block has arrived, this is the frame of
    /// *that* block's first frame, so a gap shows as a jump between the frame a
    /// read ended on and this one. Before the next block arrives the reader
    /// cannot know there was a gap, and this is the frame after the last one
    /// read. A tap that is [aligned](crate::TapAlign::ToOutput) is on the
    /// output's clock: its frame `F` is what the outputs carry at `F`.
    ///
    /// A writer that is not an op of a plan ([`pair`](Self::pair): an inlet's
    /// producer, the engine's heard tap) has no graph clock; it numbers the
    /// frames it was offered from zero, written or lost.
    #[must_use]
    pub fn next_frame(&self) -> Frame {
        if self.left > 0 {
            return self.at;
        }
        self.runs.try_peek().map_or(self.at, |run| run.start)
    }

    /// Frames readable now before the first gap (a block the rings refused):
    /// what a consumer that places every frame reads at once, so a gap is
    /// never inside a read. At most [`available`](Self::available).
    #[must_use]
    pub fn contiguous(&self) -> Samples {
        let mut end = self.at + Samples(self.left);
        let mut run = self.left;
        for next in self.runs.iter() {
            if run > 0 && next.start != end {
                break;
            }
            run += next.frames;
            end = next.start + Samples(next.frames);
        }
        Samples(run.min(self.available().get()))
    }

    /// Advances the clock over `frames` just handed out, across run
    /// boundaries.
    fn consume(&mut self, mut frames: usize) {
        while frames > 0 {
            if self.left == 0 {
                // A run is written before its samples, so every sample read
                // has its run.
                let Some(run) = self.runs.try_pop() else {
                    debug_assert!(false, "a sample was read with no run to place it");
                    return;
                };
                self.at = run.start;
                self.left = run.frames;
            }
            let step = frames.min(self.left);
            self.at += Samples(step);
            self.left -= step;
            frames -= step;
        }
    }

    /// Frames the executor could not push since the last call, and clears the
    /// count: nonzero is a gap in what was read.
    pub fn take_lost(&self) -> Samples {
        Samples(usize::try_from(self.lost.swap(0, Ordering::Relaxed)).unwrap_or(usize::MAX))
    }
}

/// A tap's reader is a [`Source`], so a recorder (`tutti-session`'s
/// `Recorder`) takes it as it is: record what the graph is producing.
///
/// Drain it from a thread of your own, never from the audio thread. It is as
/// wide as the tap: a stereo output tap is stereo, a six-lane tap six
/// channels, so a sink must be built to match.
///
/// **A tap that falls behind loses frames, and a recording closes up over the
/// gap.** The rings are bounded (the capacity named at the open); what the
/// consumer does not drain in time is dropped and counted
/// ([`take_lost`](Source::take_lost)), and the stream has no marker where
/// they were.
///
/// Like every [`Source`] it carries no sample rate: a tap runs at the graph's,
/// so build a sink at that rate or the file plays at the wrong speed.
impl Source for TapReader {
    /// As wide as the tap: one channel a lane.
    fn layout(&self) -> ChannelLayout {
        TapReader::layout(self)
    }

    /// Pops up to `out.len() / width` frames into `out` ([`pop_into`](TapReader::pop_into)).
    ///
    /// **The graph does not end**, so an empty tap is [`Starved`](Pull::Starved),
    /// never [`Ended`](Pull::Ended): it means the graph has not rendered since
    /// the last pull (the audio thread was between blocks, or the tap was only
    /// just opened: it flows from the commit that carries it). A consumer that
    /// read it as end-of-stream would stop within a block of starting.
    fn pull(&mut self, out: &mut [f32]) -> Pull {
        let frames = self.pop_into(out);
        if frames.is_zero() {
            Pull::Starved
        } else {
            Pull::Frames(frames)
        }
    }

    /// The frames the executor could not push since the last call
    /// ([`TapReader::take_lost`]).
    fn take_lost(&mut self) -> Samples {
        TapReader::take_lost(self)
    }
}
