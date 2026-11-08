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
//! [`At::Frame`] is a position on the engine's frame clock. That clock counts
//! frames **at the current rate**: a re-prepare at another rate moves it to the
//! same wall-clock time at the new one (`Executor`'s clock), so it is not the
//! number of frames rendered, and a sender that counts those lands early or late
//! after a rate change by the difference. [`TimedSender::clock`] is the clock
//! as the node last saw it, which is what a frame is measured from.
//!
//! The ring and the waiting list are sized once; nothing allocates after
//! [`new`](TimedQueueNode::new), a send never blocks or locks, and the audio
//! side takes no lock. Its fork is silent: nothing a control thread sends
//! reaches an offline render.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;
use tutti_graph::{
    Cost, Cx, Event, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts,
    Offset, Prepare, Shape, Status,
};
use tutti_types::{At, ChannelLayout, Frame};
use tutti_ump::Ump;

use super::mailbox::MAILBOX_CAPACITY;

/// The ring's capacity, which is also how many events may wait for their frame
/// and the node's event port capacity.
pub const TIMED_QUEUE_CAPACITY: usize = MAILBOX_CAPACITY;

/// Why [`TimedSender::send`] did not queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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

/// A queued event: its frame (0 for `NextBlock`) and its words.
#[derive(Clone, Copy, Debug)]
struct Stamped {
    frame: u64,
    data: [u32; 4],
}

/// The push half of a [`TimedQueueNode`]: `Clone`, `Send`, lock-free.
#[derive(Clone, Debug)]
pub struct TimedSender {
    ring: Arc<ArrayQueue<Stamped>>,
    /// The first frame of the block after the last the node rendered.
    next: Arc<AtomicU64>,
}

impl TimedSender {
    /// The engine's clock at the first frame of the next block: where the last
    /// block the node rendered ended. [`Frame`]`(0)` until it has rendered one.
    ///
    /// It does not move while the graph is suspended for a re-prepare (the node
    /// does not run), so a frame measured from it then lands early by the
    /// suspended stretch; read it again once the re-prepare has landed.
    #[must_use]
    pub fn clock(&self) -> Frame {
        Frame(self.next.load(Ordering::Relaxed))
    }

    /// Queues `ump` for `at`. Never blocks, locks or allocates.
    ///
    /// # Errors
    ///
    /// [`TimedSendError::Beat`] for [`At::Beat`]; [`TimedSendError::Full`] when
    /// the ring is full (the event is dropped; dropping a note-off whose
    /// note-on landed leaves a note stuck, so a caller must report it).
    pub fn send(&self, at: At, ump: Ump) -> Result<(), TimedSendError> {
        let frame = match at {
            At::NextBlock => 0,
            At::Frame(f) => f.get(),
            At::Beat(_) => return Err(TimedSendError::Beat),
        };
        self.ring
            .push(Stamped { frame, data: ump.0 })
            .map_err(|_| TimedSendError::Full)
    }
}

/// See the module docs.
pub struct TimedQueueNode {
    ring: Arc<ArrayQueue<Stamped>>,
    next: Arc<AtomicU64>,
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
        let ring = Arc::new(ArrayQueue::new(TIMED_QUEUE_CAPACITY));
        let next = Arc::new(AtomicU64::new(0));
        (
            Self {
                ring: Arc::clone(&ring),
                next: Arc::clone(&next),
                pending: Vec::with_capacity(TIMED_QUEUE_CAPACITY),
            },
            TimedSender { ring, next },
        )
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

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        let len = cx.env.block_len;
        let end = start + len.get() as u64;
        self.next.store(end, Ordering::Relaxed);
        // Arrivals join the list in due order. One already due (late, or
        // `NextBlock`) is due at this block's first frame, so it sorts by
        // arrival among those: `partition_point` with `<=` puts an equal one
        // last.
        while self.pending.len() < self.pending.capacity() {
            let Some(mut s) = self.ring.pop() else {
                break;
            };
            s.frame = s.frame.max(start);
            let at = self.pending.partition_point(|p| p.frame <= s.frame);
            self.pending.insert(at, s);
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
    use tutti_graph::EventKind;
    use tutti_graph::contract::Direct;
    use tutti_types::{Beat, Frame, SampleRate};

    fn note(n: u8) -> Ump {
        Ump([0x2090_0000 | u32::from(n) << 8 | 100, 0, 0, 0])
    }

    fn rig() -> (Direct<TimedQueueNode>, TimedSender) {
        let (node, tx) = TimedQueueNode::new();
        (Direct::new(node, SampleRate(48_000.0), 64), tx)
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
    /// Mutation (run): clamping nothing on arrival (the `max(start)` removed)
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
    ///
    /// Mutation (run): a ring of one more slot makes the count 257.
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

    /// The sender reads where the node's last block ended, on the clock the
    /// node ran on, and not a count of blocks: a block of another length moves
    /// it by that length.
    ///
    /// Mutation (run): storing `start` instead of `end` fails both figures.
    #[test]
    fn the_sender_reads_the_clock_the_next_block_starts_on() {
        let (mut d, tx) = rig();
        assert_eq!(tx.clock(), Frame(0));
        d.block();
        assert_eq!(tx.clock(), Frame(64));
        d.set_block_len(48);
        d.block();
        assert_eq!(tx.clock(), Frame(112));
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
}
