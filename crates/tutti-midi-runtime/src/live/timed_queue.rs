//! [`TimedQueueNode`]: MIDI a control thread sends *for a frame*.
//!
//! [`MidiQueueNode`](crate::MidiQueueNode) plays what is queued from the next
//! block. This node holds each event until the block that contains its frame,
//! then sends it on that frame: a host that says "at frame N" gets frame N, the
//! way a transport command's [`At`] does. [`At::NextBlock`] is the next block's
//! first frame, and an event whose frame has passed lands there too (late
//! commands are not dropped, the `At` rule). [`At::Beat`] needs the transport,
//! which a control thread's send does not have, so it is refused.
//!
//! Events leave in the order they are due; those due together (a late one, a
//! `NextBlock`, the same frame) leave in the order they were sent, so a
//! note-off sent after its note-on never overtakes it.
//!
//! # The clock a frame is on
//!
//! A frame is a position on the engine's frame clock. That clock counts frames
//! **at the current rate**: a re-prepare at another rate moves it to the same
//! wall-clock time at the new one, so it is not the number of frames rendered.
//! A [`Moment`] is a frame together with the rate it was counted at, and
//! [`TimedSender::clock`] reads one: where the node's last block ended. An event
//! sent for a [`Moment`] ([`TimedSender::send_at`]) names that wall-clock
//! instant whatever the rate has become since: the node rescales it on arrival,
//! and rescales every event still waiting when it is prepared at another rate,
//! with the executor's own arithmetic ([`Frame::rescaled`]). So a reading taken
//! at 48 kHz and sent after the graph went to 44.1 kHz lands where it would have.
//! A frame sent with [`TimedSender::send`] is taken to be at the rate the node
//! last published.
//!
//! The ring and the waiting list are sized once; nothing allocates after
//! [`new`](TimedQueueNode::new), a send never blocks or locks, and the audio
//! side takes no lock. When more events wait than the list holds, the latest
//! are dropped and counted ([`TimedSender::dropped`]); an event that is due is
//! never held up by those waiting for later. Its fork is silent: nothing a
//! control thread sends reaches an offline render.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytemuck::{Pod, Zeroable};
use crossbeam_queue::ArrayQueue;
use tutti_graph::{
    Cost, Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts,
    Offset, Prepare, Shape, Status,
};
use tutti_rt::{SeqReader, SeqWriter};
use tutti_types::{At, ChannelLayout, Frame, SampleRate, Samples};
use tutti_ump::Ump;

use super::mailbox::MAILBOX_CAPACITY;

/// The ring's capacity, which is also how many events may wait for their frame
/// and the node's event port capacity.
pub const TIMED_QUEUE_CAPACITY: usize = MAILBOX_CAPACITY;

/// Why [`TimedSender::send`] did not queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimedSendError {
    /// The ring is full: the event was dropped.
    Full,
    /// [`At::Beat`]: resolving a beat needs the transport.
    Beat,
}

impl std::fmt::Display for TimedSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("the timed queue is full"),
            Self::Beat => f.write_str("a beat cannot be resolved from a control thread's send"),
        }
    }
}

impl std::error::Error for TimedSendError {}

/// A position on the engine's clock and the rate it was counted at: an instant
/// that stays the same instant when the rate changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moment {
    frame: Frame,
    rate: SampleRate,
}

impl Moment {
    /// The frame, at [`rate`](Self::rate).
    #[must_use]
    pub fn frame(self) -> Frame {
        self.frame
    }

    /// The rate the frame was counted at.
    #[must_use]
    pub fn rate(self) -> SampleRate {
        self.rate
    }

    /// `frames` later, at the same rate.
    #[must_use]
    pub fn after(self, frames: Samples) -> Self {
        Self {
            frame: self.frame + frames,
            rate: self.rate,
        }
    }
}

/// What the node publishes at the end of each block and at each prepare.
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C)]
struct Reading {
    frame: u64,
    rate: f64,
}

/// A queued event: its frame (0 for `NextBlock`), the rate that frame was
/// counted at (0 when it has none to name: `NextBlock`), and its words.
#[derive(Clone, Copy, Debug)]
struct Stamped {
    frame: u64,
    rate: f64,
    data: [u32; 4],
}

/// The ring and the count of events the node had to drop.
#[derive(Debug)]
struct Shared {
    ring: ArrayQueue<Stamped>,
    dropped: AtomicU64,
}

/// The push half of a [`TimedQueueNode`]: `Clone`, `Send`, lock-free.
#[derive(Clone, Debug)]
pub struct TimedSender {
    shared: Arc<Shared>,
    clock: SeqReader<Reading>,
}

impl TimedSender {
    fn reading(&self) -> Option<Reading> {
        // A write is a few stores; a reader that loses this many copies was
        // descheduled, and an absent clock is the honest answer then.
        self.clock.read(1_000).ok().flatten()
    }

    /// The engine's clock where the node's last block ended, which is the first
    /// frame of the next block; `None` before the node has been prepared.
    ///
    /// It is the instant the node last reached: while the graph is suspended
    /// for a re-prepare the node does not run and this stands still, and an
    /// event sent for it ([`send_at`](Self::send_at)) or for a later moment
    /// still names the right wall-clock instant, because the node rescales it.
    #[must_use]
    pub fn clock(&self) -> Option<Moment> {
        self.reading().map(|r| Moment {
            frame: Frame(r.frame),
            rate: SampleRate(r.rate),
        })
    }

    /// Queues `ump` for `at`; an [`At::Frame`] is taken to be counted at the
    /// rate the node last published. A caller that read the clock earlier
    /// should send for that reading ([`send_at`](Self::send_at)): the rate may
    /// have moved between the read and this call. Never blocks, locks or
    /// allocates.
    ///
    /// # Errors
    ///
    /// [`TimedSendError::Beat`] for [`At::Beat`]; [`TimedSendError::Full`] when
    /// the ring is full (the event is dropped; dropping a note-off whose
    /// note-on landed leaves a note stuck, so a caller must report it).
    pub fn send(&self, at: At, ump: Ump) -> Result<(), TimedSendError> {
        let (frame, rate) = match at {
            At::NextBlock => (0, 0.0),
            At::Frame(f) => (f.get(), self.reading().map_or(0.0, |r| r.rate)),
            At::Beat(_) => return Err(TimedSendError::Beat),
        };
        self.push(frame, rate, ump)
    }

    /// Queues `ump` for the instant `moment` names, at whatever rate the node
    /// runs at when it takes it. Never blocks, locks or allocates.
    ///
    /// # Errors
    ///
    /// [`TimedSendError::Full`], as [`send`](Self::send).
    pub fn send_at(&self, moment: Moment, ump: Ump) -> Result<(), TimedSendError> {
        self.push(moment.frame.get(), moment.rate.get(), ump)
    }

    fn push(&self, frame: u64, rate: f64, ump: Ump) -> Result<(), TimedSendError> {
        self.shared
            .ring
            .push(Stamped {
                frame,
                rate,
                data: ump.0,
            })
            .map_err(|_| TimedSendError::Full)
    }

    /// How many events the node dropped for want of room to hold them until
    /// their frame: the latest of those waiting, when more wait than
    /// [`TIMED_QUEUE_CAPACITY`]. Only events for frames far ahead are lost this
    /// way; one that is due always gets through.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

/// See the module docs.
pub struct TimedQueueNode {
    shared: Arc<Shared>,
    clock: SeqWriter<Reading>,
    /// The rate the node is prepared at, and every frame below is counted at;
    /// 0 before the first prepare.
    rate: f64,
    /// The first frame of the next block.
    next: u64,
    /// Taken from the ring and not yet due, in due order; sized once.
    pending: Vec<Stamped>,
}

impl std::fmt::Debug for TimedQueueNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedQueueNode").finish_non_exhaustive()
    }
}

impl TimedQueueNode {
    /// A node and the sender into it.
    #[must_use]
    pub fn new() -> (Self, TimedSender) {
        let shared = Arc::new(Shared {
            ring: ArrayQueue::new(TIMED_QUEUE_CAPACITY),
            dropped: AtomicU64::new(0),
        });
        let clock = SeqWriter::new();
        let sender = TimedSender {
            shared: Arc::clone(&shared),
            clock: clock.reader(),
        };
        (
            Self {
                shared,
                clock,
                rate: 0.0,
                next: 0,
                pending: Vec::with_capacity(TIMED_QUEUE_CAPACITY),
            },
            sender,
        )
    }

    fn publish(&mut self) {
        self.clock.write(Reading {
            frame: self.next,
            rate: self.rate,
        });
    }

    /// `s` as a frame at the node's rate.
    fn at_node_rate(&self, s: &Stamped) -> u64 {
        if s.rate > 0.0 && (s.rate - self.rate).abs() > f64::EPSILON {
            Frame(s.frame)
                .rescaled(SampleRate(s.rate), SampleRate(self.rate))
                .get()
        } else {
            s.frame
        }
    }

    /// Adds `s`, due at `frame` of the node's rate, in due order. When the list
    /// is full the latest event goes: `s` itself when it is the latest, else
    /// the last waiting.
    fn keep(&mut self, mut s: Stamped, frame: u64) {
        s.frame = frame;
        s.rate = self.rate;
        if self.pending.len() == self.pending.capacity() {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            if self.pending.last().is_none_or(|last| last.frame <= frame) {
                return;
            }
            self.pending.pop();
        }
        let at = self.pending.partition_point(|p| p.frame <= frame);
        self.pending.insert(at, s);
    }
}

impl Node for TimedQueueNode {
    /// No audio; one event output of [`TIMED_QUEUE_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(TIMED_QUEUE_CAPACITY as u32)
            // Event-only: a ring drain, work per event, nothing per frame.
            .with_cost(Cost::Light)
    }

    /// A new rate moves what waits, and the clock it published, to the same
    /// wall-clock time at the new rate, as the executor moves its own.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate().get();
        if self.rate > 0.0 && (self.rate - rate).abs() > f64::EPSILON {
            let (from, to) = (SampleRate(self.rate), SampleRate(rate));
            for s in &mut self.pending {
                s.frame = Frame(s.frame).rescaled(from, to).get();
                s.rate = rate;
            }
            self.next = Frame(self.next).rescaled(from, to).get();
        }
        self.rate = rate;
        self.publish();
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        let len = cx.env.block_len;
        let end = start + len.get() as u64;
        // What waited through blocks the node did not run (a re-prepare
        // suspends it) is due at this block's first frame, in the order it was
        // due: the list is sorted, so the clamp keeps it so.
        for s in &mut self.pending {
            if s.frame >= start {
                break;
            }
            s.frame = start;
        }
        // Everything the ring holds joins the list, in due order. One already
        // due (late, or `NextBlock`) is due at this block's first frame, so it
        // sorts by arrival among those: `partition_point` with `<=` puts an
        // equal one last. At most a ring's worth, so a sender cannot hold the
        // block here.
        for _ in 0..TIMED_QUEUE_CAPACITY {
            let Some(s) = self.shared.ring.pop() else {
                break;
            };
            let frame = self.at_node_rate(&s).max(start);
            self.keep(s, frame);
        }
        let out = io.event_out(0);
        let mut sent = 0;
        for s in &self.pending {
            if s.frame >= end {
                break;
            }
            let offset = Offset::new((s.frame - start) as usize, len).unwrap_or(Offset::ZERO);
            let _ = out.push(Event::midi(offset, s.data));
            sent += 1;
        }
        self.pending.drain(..sent);
        self.next = end;
        self.publish();
        Status::Modified
    }

    /// Keeps what waits: a note-off waiting must still arrive.
    fn reset(&mut self) {}
}

/// A [`TimedQueueNode`]'s fork: the same port, nothing queued, no sender.
struct SilentTimed;

impl ForkSource for SilentTimed {
    fn fork(&self, _: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(TimedQueueNode::new().0)))
    }
}

impl IntoNode for TimedQueueNode {
    /// Nothing: the sender came out of [`TimedQueueNode::new`].
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(SilentTimed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::Direct;
    use tutti_graph::{Env, EventKind, PlayClock, Transport, TransportChanges};
    use tutti_types::{Beat, Frame, SampleRate};

    const R48: SampleRate = SampleRate(48_000.0);
    const R441: SampleRate = SampleRate(44_100.0);

    fn note(n: u8) -> Ump {
        Ump([0x2090_0000 | u32::from(n) << 8 | 100, 0, 0, 0])
    }

    fn rig() -> (Direct<TimedQueueNode>, TimedSender) {
        let (node, tx) = TimedQueueNode::new();
        (Direct::new(node, R48, 64), tx)
    }

    /// `(offset, key)` of what the last block sent.
    fn sent(d: &Direct<TimedQueueNode>) -> Vec<(u32, u32)> {
        d.events_out(0)
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => (e.offset.get(), (w[0] >> 8) & 0x7F),
                _ => unreachable!(),
            })
            .collect()
    }

    /// A block of `len` frames starting at `frame` of the engine's clock at
    /// `rate`, wherever the last one ended: what the executor hands a node
    /// after a re-prepare or a stretch it did not run.
    fn block_at(d: &mut Direct<TimedQueueNode>, frame: u64, rate: SampleRate, len: usize) {
        d.block_in(&Env {
            frame: Frame(frame),
            sample_rate: rate,
            block_len: Samples(len),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        });
    }

    /// The re-prepare the executor makes: the node is prepared at the new rate.
    fn reprepare(d: &mut Direct<TimedQueueNode>, rate: SampleRate) {
        d.node.prepare(&Prepare::new(rate, Samples(64)));
    }

    /// A frame in the third block lands there, on its offset, once, and not
    /// before.
    ///
    /// Mutation (run): sending at once (the `s.frame >= end` break removed)
    /// fails the first two assertions; an offset of `s.frame % 64` instead of
    /// `s.frame - start` fails the last (blocks of 48, so the two differ).
    #[test]
    fn an_event_waits_for_the_block_that_holds_its_frame() {
        let (mut d, tx) = rig();
        d.set_block_len(48);
        tx.send(At::Frame(Frame(48 * 2 + 13)), note(60)).unwrap();
        d.block();
        assert_eq!(sent(&d), []);
        d.block();
        assert_eq!(sent(&d), []);
        d.block();
        assert_eq!(sent(&d), [(13, 60)]);
        d.block();
        assert_eq!(sent(&d), [], "and only once");
    }

    /// Events due together leave in the order they were sent; one whose
    /// frame has passed, and `NextBlock`, land on the block's first frame.
    ///
    /// Mutation (run): clamping nothing on arrival (the `.max(start)` removed)
    /// lets the past frame 5 sort before the earlier-sent `NextBlock` and
    /// puts the note-off before its note-on: the order assertion fails.
    #[test]
    fn late_and_next_block_events_land_on_the_first_frame_in_send_order() {
        let (mut d, tx) = rig();
        d.block();
        d.block();
        tx.send(At::Frame(Frame(128 + 40)), note(61)).unwrap();
        tx.send(At::Frame(Frame(128 + 10)), note(62)).unwrap();
        tx.send(At::NextBlock, note(64)).unwrap();
        tx.send(At::Frame(Frame(5)), note(63)).unwrap();
        d.block();
        assert_eq!(sent(&d), [(0, 64), (0, 63), (10, 62), (40, 61)]);
    }

    /// A beat is refused with its own error, and queues nothing.
    #[test]
    fn a_beat_is_refused() {
        let (mut d, tx) = rig();
        assert_eq!(
            tx.send(At::Beat(Beat(1.0)), note(60)),
            Err(TimedSendError::Beat)
        );
        d.block();
        assert_eq!(sent(&d), []);
    }

    /// A full ring refuses the next send with `Full` and keeps what it held.
    #[test]
    fn a_full_ring_refuses_and_says_so() {
        let (mut d, tx) = rig();
        let ok = (0..TIMED_QUEUE_CAPACITY)
            .filter(|_| tx.send(At::NextBlock, note(60)).is_ok())
            .count();
        assert_eq!(ok, TIMED_QUEUE_CAPACITY);
        assert_eq!(tx.send(At::NextBlock, note(60)), Err(TimedSendError::Full));
        d.block();
        assert_eq!(sent(&d).len(), TIMED_QUEUE_CAPACITY);
    }

    /// The sender reads where the node's last block ended, with the rate it was
    /// counted at, on the clock the node ran on and not a count of blocks, and
    /// a new rate moves it to the same wall-clock time.
    ///
    /// Mutation (run): storing `start` instead of `end` fails the first two
    /// figures; `prepare` leaving `next` alone fails the last.
    #[test]
    fn the_sender_reads_the_clock_and_its_rate() {
        let (mut d, tx) = rig();
        let at = |frame, rate| {
            Some(Moment {
                frame: Frame(frame),
                rate,
            })
        };
        assert_eq!(tx.clock(), at(0, R48));
        d.block();
        assert_eq!(tx.clock(), at(64, R48));
        d.set_block_len(48);
        d.block();
        assert_eq!(tx.clock(), at(112, R48));
        reprepare(&mut d, R441);
        // 112 x 44100 / 48000 = 102.9
        assert_eq!(tx.clock(), at(103, R441));
    }

    /// Queued before a rate change, an event lands at the rescaled frame: the
    /// same wall-clock time at the new rate, not the old frame number.
    ///
    /// Mutation (run): `prepare` not rescaling `pending` leaves it at 1000, in
    /// the block of 976..1040, and the block at 912 sees nothing.
    #[test]
    fn what_waits_is_rescaled_by_a_rate_change() {
        let (mut d, tx) = rig();
        tx.send(At::Frame(Frame(1_000)), note(60)).unwrap();
        d.block();
        d.block();
        d.block(); // the node holds it now
        assert_eq!(sent(&d), []);
        reprepare(&mut d, R441);
        // 1000 x 44100 / 48000 = 918.75
        block_at(&mut d, 912, R441, 64);
        assert_eq!(sent(&d), [(7, 60)]);
    }

    /// A reading taken at the old rate and sent after the rate changed still
    /// names its instant: the read, the change and the push in that order.
    ///
    /// Mutation (run): `at_node_rate` returning the frame unrescaled lands it
    /// at 692, and the block at 628 sees nothing.
    #[test]
    fn a_reading_sent_after_a_rate_change_keeps_its_instant() {
        let (mut d, tx) = rig();
        d.block();
        d.block();
        d.block();
        let read = tx.clock().expect("prepared");
        assert_eq!((read.frame(), read.rate()), (Frame(192), R48));
        reprepare(&mut d, R441);
        tx.send_at(read.after(Samples(500)), note(61)).unwrap();
        // 692 x 44100 / 48000 = 635.7
        block_at(&mut d, 628, R441, 64);
        assert_eq!(sent(&d), [(8, 61)]);
    }

    /// An event whose frame passed while the node did not run (a re-prepare
    /// suspends it) lands on the first frame of the block that follows, and
    /// those waiting keep their order.
    ///
    /// Mutation (run): without the clamp of what waits, `s.frame - start`
    /// underflows and the block panics in a debug build.
    #[test]
    fn what_came_due_while_the_node_slept_lands_on_the_first_frame() {
        let (mut d, tx) = rig();
        tx.send(At::Frame(Frame(100)), note(60)).unwrap();
        tx.send(At::Frame(Frame(120)), note(61)).unwrap();
        d.block(); // frames 0..64: both wait
        block_at(&mut d, 500, R48, 64);
        assert_eq!(sent(&d), [(0, 60), (0, 61)]);
    }

    /// The waiting list survives a reset: a note-off waiting must still
    /// arrive.
    ///
    /// Mutation (run): `reset` clearing `pending` fails it.
    #[test]
    fn what_waits_survives_a_reset() {
        let (mut d, tx) = rig();
        tx.send(At::Frame(Frame(200)), note(70)).unwrap();
        d.block();
        d.node.reset();
        d.block();
        d.block();
        d.block();
        assert_eq!(sent(&d), [(8, 70)]);
    }

    /// A due event is not held up by a full list of later ones: with the list
    /// full of far-future events, a note-off sent now leaves in the next block.
    ///
    /// Mutation (run): draining the ring only while the list has room (the
    /// old `while pending.len() < capacity`) leaves the note-off in the ring.
    #[test]
    fn a_due_event_passes_a_list_full_of_later_ones() {
        let (mut d, tx) = rig();
        for k in 0..TIMED_QUEUE_CAPACITY {
            tx.send(At::Frame(Frame(1_000_000 + k as u64)), note(10))
                .unwrap();
        }
        d.block(); // the list is full now
        tx.send(At::NextBlock, note(72)).unwrap();
        d.block();
        assert_eq!(sent(&d), [(0, 72)]);
        // The room it took came from the latest of the far-future events.
        assert_eq!(tx.dropped(), 1);
    }

    /// Past the list's room the latest event goes and is counted; the earlier
    /// ones keep their places.
    ///
    /// Mutation (run): dropping the new event always (the `pop` removed) keeps
    /// the late one and loses the early one.
    #[test]
    fn the_latest_event_is_dropped_and_counted_when_the_list_is_full() {
        let (mut d, tx) = rig();
        for k in 0..TIMED_QUEUE_CAPACITY {
            tx.send(At::Frame(Frame(1_000 + 2 * k as u64)), note(10))
                .unwrap();
        }
        d.block();
        tx.send(At::Frame(Frame(900_000)), note(11)).unwrap(); // later than all: lost
        tx.send(At::Frame(Frame(1_001)), note(12)).unwrap(); // earlier than the last: kept
        d.block();
        assert_eq!(tx.dropped(), 2);
        block_at(&mut d, 1_000, R48, 64);
        assert_eq!(sent(&d)[..3], [(0, 10), (1, 12), (2, 10)]);
    }
}
