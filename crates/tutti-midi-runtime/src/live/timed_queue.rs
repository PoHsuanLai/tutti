//! [`TimedQueueNode`]: MIDI a control thread sends *for a frame*, over the
//! graph's event inlet.
//!
//! The waiting list, the ring, the clock and the rules (late events, a frame's
//! [`Moment`], what is evicted first) are `tutti-graph`'s
//! [`EventInletNode`]; this node is that node and nothing more, and
//! [`TimedSender`] is its writer behind a lock so that it can be `Clone`. The
//! lock is a control thread's: it is taken to push, to read the clock and to
//! read a count, never on the audio thread, which owns the node. A caller that
//! needs no `Clone` uses the inlet directly (design 017, "The event inlet").
//!
//! [`MidiQueueNode`](crate::MidiQueueNode) plays what is queued from the next
//! block; this node holds each event until the block that contains its frame,
//! as a transport command's [`At`] does. [`At::Beat`] is refused: a control
//! thread's send has no transport.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tutti_graph::{
    Cx, EventInletNode, EventInletWriter, IntoNode, Io, Node, NodeParts, Prepare, Refused, Shape,
    Status,
};
use tutti_types::{At, Moment};
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

impl From<Refused> for TimedSendError {
    fn from(r: Refused) -> Self {
        match r {
            Refused::Full => Self::Full,
            Refused::Beat => Self::Beat,
        }
    }
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

/// The push half of a [`TimedQueueNode`]: `Clone`, `Send`. Its clones share one
/// [`EventInletWriter`] behind a lock taken on control threads only.
#[derive(Clone, Debug)]
pub struct TimedSender {
    writer: Arc<Mutex<EventInletWriter>>,
}

impl TimedSender {
    fn writer(&self) -> MutexGuard<'_, EventInletWriter> {
        // A panic while pushing leaves the writer whole.
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The engine's clock where the node's last block ended; `None` before
    /// the node has been prepared ([`EventInletWriter::clock`]).
    #[must_use]
    pub fn clock(&self) -> Option<Moment> {
        self.writer().clock()
    }

    /// Queues `ump` for `at` ([`EventInletWriter::push`]).
    ///
    /// # Errors
    ///
    /// [`TimedSendError::Beat`] for [`At::Beat`]; [`TimedSendError::Full`] when
    /// the ring is full (the event is dropped).
    pub fn send(&self, at: At, ump: Ump) -> Result<(), TimedSendError> {
        Ok(self.writer().push(at, ump)?)
    }

    /// Queues `ump` for the instant `moment` names ([`EventInletWriter::push_at`]).
    ///
    /// # Errors
    ///
    /// [`TimedSendError::Full`], as [`send`](Self::send).
    pub fn send_at(&self, moment: Moment, ump: Ump) -> Result<(), TimedSendError> {
        Ok(self.writer().push_at(moment, ump)?)
    }

    /// How many events the node dropped from its waiting list
    /// ([`EventInletWriter::evicted`]).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.writer().evicted()
    }
}

/// See the module docs: an [`EventInletNode`] of [`TIMED_QUEUE_CAPACITY`]
/// events, with a [`TimedSender`] for its writer.
#[derive(Debug)]
pub struct TimedQueueNode(EventInletNode);

impl TimedQueueNode {
    /// A node and the sender into it.
    #[must_use]
    pub fn new() -> (Self, TimedSender) {
        let (node, writer) = EventInletNode::standalone(TIMED_QUEUE_CAPACITY);
        let sender = TimedSender {
            writer: Arc::new(Mutex::new(writer)),
        };
        (Self(node), sender)
    }
}

impl Node for TimedQueueNode {
    fn shape(&self) -> Shape {
        self.0.shape()
    }

    fn prepare(&mut self, p: &Prepare) {
        self.0.prepare(p);
    }

    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.process(cx, io)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

impl IntoNode for TimedQueueNode {
    /// Nothing: the sender came out of [`TimedQueueNode::new`]. The fork is the
    /// inlet's, silent.
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        self.0.into_parts()
    }
}

#[cfg(test)]
mod tests {
    //! The waiting list, the late rule, the rescale and the eviction order are
    //! `tutti-graph`'s (`tests/event_inlet.rs`); what is here is what the
    //! wrapper adds: its error type, its lock and its `dropped`.
    use super::*;
    use tutti_graph::EventKind;
    use tutti_graph::contract::Direct;
    use tutti_types::{Beat, Frame, SampleRate};

    const R48: SampleRate = SampleRate(48_000.0);

    fn note(n: u8) -> Ump {
        Ump([0x2090_0000 | u32::from(n) << 8 | 100, 0, 0, 0])
    }

    fn rig() -> (Direct<TimedQueueNode>, TimedSender) {
        let (node, tx) = TimedQueueNode::new();
        (Direct::new(node, R48, 64), tx)
    }

    fn sent(d: &Direct<TimedQueueNode>) -> Vec<(u32, u32)> {
        d.events_out(0)
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => (e.offset.get(), (w[0] >> 8) & 0x7F),
                _ => unreachable!(),
            })
            .collect()
    }

    /// An event waits for the block that holds its frame and lands on its
    /// offset, through the wrapper.
    ///
    /// Mutation (run): `send` pushing `At::NextBlock` whatever `at` is lands
    /// it in the first block and the first assertion fails.
    #[test]
    fn a_send_reaches_its_frame() {
        let (mut d, tx) = rig();
        tx.send(At::Frame(Frame(64 + 13)), note(60)).unwrap();
        d.block();
        assert_eq!(sent(&d), []);
        d.block();
        assert_eq!(sent(&d), [(13, 60)]);
    }

    /// A beat is refused with its own error, and queues nothing.
    ///
    /// Mutation (run): mapping `Refused::Beat` to `Full` fails it.
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
    /// Mutation (run): mapping `Refused::Full` to `Beat` fails it.
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

    /// The clock and a moment sent for round-trip through the shared writer,
    /// and `dropped` is the waiting list's evictions.
    ///
    /// Mutation (run): `dropped` reading `lost()` (the ring's refusals) reads
    /// 0; `clock` returning `None` fails the first assertion.
    #[test]
    fn the_clock_and_the_eviction_count_come_through() {
        let (mut d, tx) = rig();
        assert_eq!(tx.clock(), Some(Moment::new(Frame(0), R48)));
        d.block();
        let read = tx.clock().expect("prepared");
        assert_eq!(read, Moment::new(Frame(64), R48));
        for k in 0..TIMED_QUEUE_CAPACITY {
            tx.send_at(read.after(tutti_types::Samples(1_000_000 + k)), note(10))
                .unwrap();
        }
        d.block(); // the list is full
        tx.send(At::NextBlock, note(72)).unwrap();
        d.block();
        assert_eq!(sent(&d), [(0, 72)]);
        assert_eq!(tx.dropped(), 1);
    }
}
