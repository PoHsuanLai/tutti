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
//! The node does scheduling and no MIDI. Each event is pushed for a time, a
//! [`Stamp`], and the node holds it until the block that contains it, then
//! emits it on its offset in that block ([`Env::due`](crate::Env::due)):
//! [`Stamp::NextBlock`] is the next block's first frame, a [`Stamp::Frame`] is
//! a frame of the graph's clock and a [`Stamp::Moment`] is a frame with the
//! rate it was counted at. There is no beat. The node could resolve one
//! against its block's transport, as `Env::due` does, but a beat the transport
//! never reaches (it is stopped, a loop skips it, a locate jumps past it) would
//! hold its slot in the bounded waiting list for good, and a frame stamp has no
//! such case: the clock always reaches it. Musical time is the transport's and
//! a clip's, which read it every block (`MotionFsm::schedule` for a command,
//! the MIDI clip for events). So the type has no way to say a beat. An event whose frame has already passed
//! lands at offset 0 of the block it is found in and is counted late
//! ([`Losses::late`]); it is not dropped, as no late command is.
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
//! last block ended. An event pushed for a moment names that instant whatever
//! the rate has become since: the node rescales it on arrival, and rescales
//! every event still waiting when it is prepared at another rate, with the
//! executor's own arithmetic ([`Frame::rescaled`]). A bare [`Frame`] carries
//! no rate and is counted at the rate the node has when it takes the event
//! from the ring, so a producer whose frames may straddle a re-prepare pushes a
//! `Moment`. There is no resampling and so no rate to refuse.
//!
//! # What is counted
//!
//! The ring and the waiting list are sized once, at the capacity named when the
//! inlet is made, and so is the port's declared event capacity; nothing
//! allocates after that and nothing blocks. What does not fit is counted, as
//! [`Losses`], read from [`InletCounts::losses`]:
//!
//! - `lost`: refused at the ring because it was full (the writer counts it and
//!   `push` answers [`Refused::Full`]);
//! - `evicted`: dropped from the waiting list because more events wait than it
//!   holds. The latest-due event that may be lost goes; a note-off or a panic
//!   controller ([`Ump::is_panic_controller`]) is kept in preference to any
//!   other, so a note that began does not hang;
//! - `refused`: the port refused an event
//!   ([`EventWriter::push`](crate::EventWriter::push)). The executor counts it
//!   in `dropped_event_count` too, so an export watching that sees it;
//! - `late`: a count, not a loss.
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
use crate::event_losses::Losses;
use crate::fork::{ForkCause, ForkMode, ForkSource, Forked};
use crate::io::Io;
use crate::node::{Cx, IntoNode, Node, NodeParts, Prepare, Shape, Status};
use crate::time::Due;

/// Why [`EventInletWriter::push`] did not queue.
///
/// One variant: a stamp cannot name a beat any more, so the ring being full is
/// the only way left. It stays an enum so the reason reads at the call site
/// (`Err(Refused::Full)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The ring is full: the event was dropped and counted
    /// ([`Losses::lost`]).
    Full,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("the event inlet's ring is full"),
        }
    }
}

impl std::error::Error for Refused {}

/// When a pushed event is due, as the producer names it.
///
/// There is no beat: see the module docs. A [`Frame`] and a [`Moment`] convert
/// into one (`push(Frame(10), ump)`, `push(clock.after(n), ump)`), so a call
/// site names the clock it means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stamp {
    /// The next block's first frame.
    NextBlock,
    /// A frame of the graph's clock, at the rate the node has when it takes
    /// the event.
    Frame(Frame),
    /// A frame and the rate it was counted at.
    Moment(Moment),
}

impl From<Frame> for Stamp {
    fn from(frame: Frame) -> Self {
        Self::Frame(frame)
    }
}

impl From<Moment> for Stamp {
    fn from(moment: Moment) -> Self {
        Self::Moment(moment)
    }
}

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
    /// `frame`, at the node's rate when it takes the event.
    Frame(u64),
    /// `frame`, counted at `rate`.
    Moment { frame: u64, rate: f64 },
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

/// A cloneable view of an inlet's counts, for a thread that does not hold the
/// writer (the control side, while a driver thread pushes).
#[derive(Clone, Debug)]
pub struct InletCounts(Arc<Counts>);

impl InletCounts {
    /// Everything the inlet has lost, cumulative, in events.
    ///
    /// `lost`: pushes the ring refused because it was full; `evicted`: events
    /// dropped from the waiting list for room; `refused`: events the port
    /// refused; `late`: events emitted at offset 0 for being past their frame.
    #[must_use]
    pub fn losses(&self) -> Losses {
        let c = &*self.0;
        Losses {
            lost: c.lost.load(Ordering::Relaxed),
            evicted: c.evicted.load(Ordering::Relaxed),
            refused: c.refused.load(Ordering::Relaxed),
            late: c.late.load(Ordering::Relaxed),
        }
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
    /// event pushed for it or for a later moment
    /// still names the right wall-clock instant, because the node rescales it.
    #[must_use]
    pub fn clock(&self) -> Option<Moment> {
        self.reading()
            .map(|r| Moment::new(Frame(r.frame), SampleRate(r.rate)))
    }

    /// Queues `ump` for `at`: [`Stamp::NextBlock`], a [`Frame`] or a
    /// [`Moment`] (the last names an instant whatever rate the node runs at
    /// when it takes it; a bare frame is at that rate).
    ///
    /// # Errors
    ///
    /// [`Refused::Full`] when the ring is full (the event is dropped and
    /// counted in [`Losses::lost`]; dropping a note-off whose note-on landed
    /// leaves a note stuck, so a caller must report it).
    pub fn push(&mut self, at: impl Into<Stamp>, ump: Ump) -> Result<(), Refused> {
        let when = match at.into() {
            Stamp::NextBlock => When::NextBlock,
            Stamp::Frame(f) => When::Frame(f.get()),
            Stamp::Moment(m) => When::Moment {
                frame: m.frame().get(),
                rate: m.rate().get(),
            },
        };
        self.ring.try_push(Pushed { when, ump }).map_err(|_| {
            self.counts.lost.fetch_add(1, Ordering::Relaxed);
            Refused::Full
        })
    }

    /// Queues every `(stamp, ump)` in order, each as [`push`](Self::push)
    /// does, and answers how many the ring took. A refused one is counted as
    /// lost like any other and the rest are still tried, so the count a
    /// caller reads stays one per event.
    pub fn push_all<S: Into<Stamp>>(
        &mut self,
        events: impl IntoIterator<Item = (S, Ump)>,
    ) -> usize {
        events
            .into_iter()
            .map(|(at, ump)| self.push(at, ump))
            .filter(Result::is_ok)
            .count()
    }

    /// The counts as a view that outlives the writer's thread: read them from
    /// the control side while another thread pushes. The writer has no getters
    /// of its own: the view is the one place that answers.
    #[must_use]
    pub fn counts(&self) -> InletCounts {
        InletCounts(Arc::clone(&self.counts))
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
/// use tutti_graph::Stamp;
/// use tutti_fixtures::midi::m1_note_on;
/// use tutti_ump::{MidiChannel, NoteNumber, U7};
///
/// let (node, mut writer) = EventInletNode::pair(256);
/// let note_on = m1_note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
/// writer.push(Stamp::NextBlock, note_on).unwrap(); // from any one thread
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
                When::Frame(frame) => {
                    if env.due(At::Frame(Frame(frame))) == Due::Late {
                        self.counts.late.fetch_add(1, Ordering::Relaxed);
                        start.get()
                    } else {
                        frame
                    }
                }
                When::Moment { frame, rate } => {
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
