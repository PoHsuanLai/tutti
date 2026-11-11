//! The event inlet: MIDI from outside the graph, each event on its frame.
//!
//! The inverse of an [event tap](crate::EventTapReader), and the event sibling
//! of the audio [`InletNode`](crate::InletNode): a tap copies one event port
//! out to a thread of your own, and an inlet drains a ring a thread of your own
//! fills (a keyboard, a host's note calls) into one event output port.
//! [`EventInletNode`] is the node and [`EventInletWriter`] the producer's end;
//! `tutti-session`'s `Graph::open_event_inlet` makes the pair and inserts the
//! node.
//!
//! The node does scheduling and no MIDI. Each event is pushed for a time, an
//! [`At`], and the node holds it until the block that contains it, then emits
//! it on its offset in that block ([`Env::due`](crate::Env::due)): [`At::NextBlock`] is the next
//! block's first frame, an [`At::Frame`] is a frame of the graph's clock, and
//! [`At::Beat`] is refused, because the producer has no transport and a beat
//! may never come, which leaves the waiting list's bound unclear. An event
//! whose frame has already passed lands at offset 0 of the block it is found in
//! and is counted late ([`EventInletWriter::late`]); it is not dropped, as
//! no late command is.
//!
//! Events leave in the order they are due; those due together (a late one, a
//! `NextBlock`, the same frame) leave in the order they were pushed, so a
//! note-off pushed after its note-on never overtakes it. An event that is due
//! is never held up by those waiting for later ones.
//!
//! # The clock a frame is on
//!
//! The frame clock counts frames **at the current rate**: a re-prepare at
//! another rate moves it to the same wall-clock time at the new one, so it is
//! not the number of frames rendered. A [`Moment`] is a frame with the rate it
//! was counted at, and [`EventInletWriter::clock`] reads one: where the node's
//! last block ended. An event pushed for a moment
//! ([`push_at`](EventInletWriter::push_at)) names that instant whatever the
//! rate has become since: the node rescales it on arrival, and rescales every
//! event still waiting when it is prepared at another rate, with the
//! executor's own arithmetic ([`Frame::rescaled`]). A frame pushed with
//! [`push`](EventInletWriter::push) is taken to be at the rate the node last
//! published. There is no resampling and so no rate to refuse: the rate is the
//! stamp's.
//!
//! # What is counted
//!
//! The ring and the waiting list are sized once, at the capacity named when the
//! inlet is made, and so is the port's declared event capacity; nothing
//! allocates after that and nothing blocks. What does not fit is counted, each
//! in its own count, read from the writer's side:
//!
//! - [`lost`](EventInletWriter::lost): refused at the ring because it was full
//!   (the writer counts it and `push` answers [`Refused::Full`]);
//! - [`evicted`](EventInletWriter::evicted): dropped from the waiting list
//!   because more events wait than it holds. The latest-due event that may be
//!   lost goes; a note-off or a panic controller
//!   ([`Ump::is_panic_controller`]) is kept in preference to any other, so a
//!   note that began does not hang;
//! - [`refused`](EventInletWriter::refused): the port refused an event
//!   ([`EventWriter::push`](crate::EventWriter::push)). The executor counts it
//!   in `dropped_event_count` too, so an export watching that sees it.
//!
//! [`late`](EventInletWriter::late) is a count, not a loss.
//!
//! # One producer
//!
//! The ring is `ringbuf`'s single-producer, single-consumer ring, so the writer
//! is `Send` and is not `Clone`; two producers are two inlets, and the graph's
//! event fan-in merges them in source order:
//!
//! ```compile_fail
//! fn second_producer(writer: &tutti_graph::EventInletWriter) -> tutti_graph::EventInletWriter {
//!     writer.clone()
//! }
//! ```
//!
//! Nor is the node, which is the ring's only consumer:
//!
//! ```compile_fail
//! fn second_consumer(node: &tutti_graph::EventInletNode) -> tutti_graph::EventInletNode {
//!     node.clone()
//! }
//! ```
//!
//! # Fork
//!
//! A fork is **silent**: a node with the same port, an empty ring and no
//! writer. An offline render has no live events, and refusing the fork would
//! stop the export of any project with a keyboard in it. (The audio inlet is
//! unforkable, because silence in place of a live signal would be an export
//! that looks complete and is not.) [`reset`](Node::reset) keeps the ring and
//! the waiting list: a note-off waiting must still arrive.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytemuck::{Pod, Zeroable};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_rt::{SeqReader, SeqWriter};
use tutti_types::{At, Frame, Moment, SampleRate};
use tutti_ump::Ump;

use crate::cost::Cost;
use crate::event::Event;
use crate::fork::{ForkCause, ForkMode, ForkSource, Forked};
use crate::io::Io;
use crate::node::{Cx, IntoNode, Node, NodeParts, Prepare, Shape, Status};
use crate::time::Due;

/// Why [`EventInletWriter::push`] did not queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The ring is full: the event was dropped and counted
    /// ([`EventInletWriter::lost`]).
    Full,
    /// [`At::Beat`]: resolving a beat needs the transport, which a producer
    /// does not have.
    Beat,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("the event inlet's ring is full"),
            Self::Beat => f.write_str("a beat cannot be resolved from a producer's push"),
        }
    }
}

impl std::error::Error for Refused {}

/// What the node publishes at the end of each block and at each prepare: where
/// its clock stands.
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C)]
struct Reading {
    frame: u64,
    rate: f64,
}

/// When a pushed event is due, as the producer named it.
#[derive(Clone, Copy, Debug)]
enum When {
    /// The next block's first frame.
    NextBlock,
    /// `frame`, counted at `rate`; a `rate` of 0 names none (the node had not
    /// published one) and is taken to be the node's.
    Frame { frame: u64, rate: f64 },
}

/// A pushed event, in the ring.
#[derive(Clone, Copy, Debug)]
struct Pushed {
    when: When,
    ump: Ump,
}

/// An event taken from the ring: due at `frame` of the node's rate.
#[derive(Clone, Copy, Debug)]
struct Waiting {
    frame: u64,
    ump: Ump,
}

/// The counts the node and the writer share.
#[derive(Debug, Default)]
struct Counts {
    lost: AtomicU64,
    late: AtomicU64,
    evicted: AtomicU64,
    refused: AtomicU64,
}

/// A cloneable view of an inlet's four counts, for a thread that does not hold
/// the writer (the control side, while a driver thread pushes): the same
/// cumulative figures as [`EventInletWriter::lost`], [`evicted`](Self::evicted),
/// [`refused`](Self::refused) and [`late`](Self::late).
#[derive(Clone, Debug)]
pub struct InletCounts(Arc<Counts>);

impl InletCounts {
    /// As [`EventInletWriter::lost`].
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.0.lost.load(Ordering::Relaxed)
    }

    /// As [`EventInletWriter::evicted`].
    #[must_use]
    pub fn evicted(&self) -> u64 {
        self.0.evicted.load(Ordering::Relaxed)
    }

    /// As [`EventInletWriter::refused`].
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.0.refused.load(Ordering::Relaxed)
    }

    /// As [`EventInletWriter::late`].
    #[must_use]
    pub fn late(&self) -> u64 {
        self.0.late.load(Ordering::Relaxed)
    }
}

/// The producer's end of an event inlet: push events for a time, from one
/// thread.
///
/// `Send`, not `Clone`, and `push` takes `&mut self`: see the module docs.
/// Nothing here blocks, locks or allocates.
pub struct EventInletWriter {
    ring: HeapProd<Pushed>,
    clock: SeqReader<Reading>,
    counts: Arc<Counts>,
}

impl std::fmt::Debug for EventInletWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventInletWriter").finish_non_exhaustive()
    }
}

impl EventInletWriter {
    fn reading(&self) -> Option<Reading> {
        // A write is a few stores; a reader that loses this many copies was
        // descheduled, and an absent clock is the honest answer then.
        self.clock.read(1_000).ok().flatten()
    }

    /// The graph's clock where the node's last block ended, which is the first
    /// frame of the next block; `None` before the node has been prepared.
    ///
    /// It is the instant the node last reached: while the graph is suspended
    /// for a re-prepare the node does not run and this stands still, and an
    /// event pushed for it ([`push_at`](Self::push_at)) or for a later moment
    /// still names the right wall-clock instant, because the node rescales it.
    #[must_use]
    pub fn clock(&self) -> Option<Moment> {
        self.reading()
            .map(|r| Moment::new(Frame(r.frame), SampleRate(r.rate)))
    }

    /// Queues `ump` for `at`; an [`At::Frame`] is taken to be counted at the
    /// rate the node last published. A caller that read the clock earlier
    /// should push for that reading ([`push_at`](Self::push_at)): the rate may
    /// have moved between the read and this call.
    ///
    /// # Errors
    ///
    /// [`Refused::Beat`] for [`At::Beat`]; [`Refused::Full`] when the ring is
    /// full (the event is dropped and counted in [`lost`](Self::lost); dropping
    /// a note-off whose note-on landed leaves a note stuck, so a caller must
    /// report it).
    pub fn push(&mut self, at: At, ump: Ump) -> Result<(), Refused> {
        let when = match at {
            At::NextBlock => When::NextBlock,
            At::Frame(f) => When::Frame {
                frame: f.get(),
                rate: self.reading().map_or(0.0, |r| r.rate),
            },
            At::Beat(_) => return Err(Refused::Beat),
        };
        self.queue(when, ump)
    }

    /// Queues `ump` for the instant `at` names, at whatever rate the node runs
    /// at when it takes it.
    ///
    /// # Errors
    ///
    /// [`Refused::Full`], as [`push`](Self::push).
    pub fn push_at(&mut self, at: Moment, ump: Ump) -> Result<(), Refused> {
        self.queue(
            When::Frame {
                frame: at.frame().get(),
                rate: at.rate().get(),
            },
            ump,
        )
    }

    fn queue(&mut self, when: When, ump: Ump) -> Result<(), Refused> {
        self.ring.try_push(Pushed { when, ump }).map_err(|_| {
            self.counts.lost.fetch_add(1, Ordering::Relaxed);
            Refused::Full
        })
    }

    /// The counts as a view that outlives the writer's thread: read them from
    /// the control side while another thread pushes.
    #[must_use]
    pub fn counts(&self) -> InletCounts {
        InletCounts(Arc::clone(&self.counts))
    }

    /// How many events the ring refused because it was full: each
    /// [`push`](Self::push) that answered [`Refused::Full`]. Cumulative.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.counts.lost.load(Ordering::Relaxed)
    }

    /// How many events the node dropped from its waiting list for want of room
    /// to hold them until their frame: the latest that may be lost (never a
    /// note-off or a panic while anything else waits; see the module docs).
    /// Only events for frames far ahead are lost this way; one that is due
    /// always gets through. Cumulative.
    #[must_use]
    pub fn evicted(&self) -> u64 {
        self.counts.evicted.load(Ordering::Relaxed)
    }

    /// How many events the node's port refused. Cumulative.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.counts.refused.load(Ordering::Relaxed)
    }

    /// How many events were found already past and emitted at offset 0 of the
    /// block they were found in: late, not lost. Cumulative.
    #[must_use]
    pub fn late(&self) -> u64 {
        self.counts.late.load(Ordering::Relaxed)
    }
}

/// A node that copies a ring a thread of your own fills onto one event output,
/// each event on its frame: no inputs, one event output. See the module docs.
///
/// It is event-only, a ring drain with work per event and nothing per frame:
/// [`Cost::Light`]. It has no audio input, so the executor never parks it as
/// silent. Not `Clone`, and its fork is silent.
///
/// # Examples
///
/// ```
/// use tutti_graph::EventInletNode;
/// use tutti_types::At;
/// use tutti_fixtures::midi::m1_note_on;
/// use tutti_ump::{MidiChannel, NoteNumber, U7};
///
/// let (node, mut writer) = EventInletNode::pair(256);
/// let note_on = m1_note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
/// writer.push(At::NextBlock, note_on).unwrap(); // from any one thread
/// assert_eq!(node.capacity(), 256);
/// ```
pub struct EventInletNode {
    ring: HeapCons<Pushed>,
    counts: Arc<Counts>,
    clock: SeqWriter<Reading>,
    capacity: usize,
    /// The rate the node is prepared at, and every frame below is counted at;
    /// 0 before the first prepare.
    rate: f64,
    /// The first frame of the next block.
    next: u64,
    /// Taken from the ring and not yet emitted, in due order; sized once.
    waiting: Vec<Waiting>,
}

impl std::fmt::Debug for EventInletNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventInletNode")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl EventInletNode {
    /// A node over a new ring of `capacity` events, and the writer the producer
    /// fills it with. The ring, the waiting list and the port's declared
    /// capacity are all `capacity` events (a count of events, a bare `usize` as
    /// [`Shape::with_event_capacity`] takes one: no unit covers it). Allocates;
    /// control side.
    ///
    /// # Panics
    ///
    /// If `capacity` is 0 or does not fit a `u32` (a port's capacity is one).
    #[must_use = "the writer is the only way to fill the ring"]
    pub fn pair(capacity: usize) -> (Self, EventInletWriter) {
        assert!(
            capacity > 0 && u32::try_from(capacity).is_ok(),
            "an event inlet holds 1 to u32::MAX events, not {capacity}"
        );
        let (producer, consumer) = HeapRb::<Pushed>::new(capacity).split();
        let counts = Arc::new(Counts::default());
        let clock = SeqWriter::new();
        let writer = EventInletWriter {
            ring: producer,
            clock: clock.reader(),
            counts: Arc::clone(&counts),
        };
        let node = Self {
            ring: consumer,
            counts,
            clock,
            capacity,
            rate: 0.0,
            next: 0,
            waiting: Vec::with_capacity(capacity),
        };
        (node, writer)
    }

    /// The ring's capacity in events, which is also how many may wait for their
    /// frame and the port's declared capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn publish(&mut self) {
        self.clock.write(Reading {
            frame: self.next,
            rate: self.rate,
        });
    }

    /// Adds `ump`, due at `frame` of the node's rate, in due order. When the
    /// list is full an event goes, and counts as evicted; which one is chosen
    /// so that what ends a sound is the last to be lost:
    ///
    /// - the latest event that may be lost (not a note-off, not a panic
    ///   controller: [`must_arrive`]) goes, the new one itself when it is the
    ///   latest;
    /// - a new event that must arrive takes the place of any event that may be
    ///   lost;
    /// - only when everything waiting must arrive too does a note-off or a
    ///   panic go, the latest of them (the new one or the last waiting). That
    ///   takes a full list of note-offs and panics waiting at once.
    fn keep(&mut self, ump: Ump, frame: u64) {
        if self.waiting.len() == self.capacity {
            self.counts.evicted.fetch_add(1, Ordering::Relaxed);
            let lost = self.waiting.iter().rposition(|w| !must_arrive(w.ump));
            match lost {
                Some(i) if must_arrive(ump) || self.waiting[i].frame > frame => {
                    self.waiting.remove(i);
                }
                Some(_) => return,
                // Only a must-arrive event displaces one: a loseable event that
                // meets a full list of note-offs and panics is the one lost.
                None if must_arrive(ump)
                    && self.waiting.last().is_some_and(|last| last.frame > frame) =>
                {
                    self.waiting.pop();
                }
                None => return,
            }
        }
        let at = self.waiting.partition_point(|w| w.frame <= frame);
        self.waiting.insert(at, Waiting { frame, ump });
    }
}

/// Whether losing `ump` can leave a sound hanging: a note-off (its note-on has
/// played), or All Sound Off or All Notes Off.
fn must_arrive(ump: Ump) -> bool {
    ump.is_note_off() || ump.is_panic_controller()
}

impl Node for EventInletNode {
    /// No audio; one event output of [`capacity`](EventInletNode::capacity).
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            // `pair` checked that it fits.
            .with_event_capacity(self.capacity as u32)
            // Event-only: a ring drain, work per event, nothing per frame.
            .with_cost(Cost::Light)
    }

    /// A new rate moves what waits, and the clock it published, to the same
    /// wall-clock time at the new rate, as the executor moves its own.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate().get();
        if self.rate > 0.0 && (self.rate - rate).abs() > f64::EPSILON {
            let (from, to) = (SampleRate(self.rate), SampleRate(rate));
            for w in &mut self.waiting {
                w.frame = Frame(w.frame).rescaled(from, to).get();
            }
            self.next = Frame(self.next).rescaled(from, to).get();
        }
        self.rate = rate;
        self.publish();
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let env = cx.env;
        let start = env.frame;
        // What waited through blocks the node did not run (a re-prepare
        // suspends it) is late: due at this block's first frame, in the order
        // it was due. The list is sorted, so what is late is a prefix and the
        // clamp keeps it sorted.
        for w in &mut self.waiting {
            if env.due(At::Frame(Frame(w.frame))) != Due::Late {
                break;
            }
            w.frame = start.get();
            self.counts.late.fetch_add(1, Ordering::Relaxed);
        }
        // Everything the ring holds joins the list, in due order. One already
        // due (late, or `NextBlock`) is due at this block's first frame, so it
        // sorts by arrival among those: `partition_point` with `<=` puts an
        // equal one last. At most a ring's worth, so a producer cannot hold the
        // block here.
        for _ in 0..self.capacity {
            let Some(pushed) = self.ring.try_pop() else {
                break;
            };
            let frame = match pushed.when {
                When::NextBlock => start.get(),
                When::Frame { frame, rate } => {
                    let frame = if rate > 0.0 && (rate - self.rate).abs() > f64::EPSILON {
                        Frame(frame)
                            .rescaled(SampleRate(rate), SampleRate(self.rate))
                            .get()
                    } else {
                        frame
                    };
                    if env.due(At::Frame(Frame(frame))) == Due::Late {
                        self.counts.late.fetch_add(1, Ordering::Relaxed);
                        start.get()
                    } else {
                        frame
                    }
                }
            };
            self.keep(pushed.ump, frame);
        }
        let out = io.event_out(0);
        let mut sent = 0;
        let mut refused = 0u64;
        for w in &self.waiting {
            let Due::In(offset) = env.due(At::Frame(Frame(w.frame))) else {
                break;
            };
            // A refusal is counted by the port (the executor's dropped-event
            // count) and here.
            if out.push(Event::midi(offset, w.ump)).is_err() {
                refused += 1;
            }
            sent += 1;
        }
        self.waiting.drain(..sent);
        if refused > 0 {
            self.counts.refused.fetch_add(refused, Ordering::Relaxed);
        }
        self.next = start.get() + env.block_len.get() as u64;
        self.publish();
        Status::Modified
    }

    /// Keeps the ring and what waits: a note-off waiting must still arrive.
    fn reset(&mut self) {}
}

/// An [`EventInletNode`]'s fork: the same port, nothing queued, no writer.
struct SilentInlet {
    capacity: usize,
}

impl ForkSource for SilentInlet {
    fn fork(&self, _: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(EventInletNode::pair(self.capacity).0)))
    }
}

/// Inserted with a **silent** fork (see the module docs). No controls: the
/// writer came out of [`EventInletNode::pair`].
impl IntoNode for EventInletNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        let capacity = self.capacity;
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(SilentInlet { capacity })),
        }
    }
}
