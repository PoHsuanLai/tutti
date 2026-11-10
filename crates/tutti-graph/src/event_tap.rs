//! Event taps: copy-out points for one event port of the compiled plan.
//!
//! The event sibling of [`tap`](crate::TapReader). An event tap is declared on
//! the [`Topology`](crate::Topology) as "copy this event output"
//! ([`EventTapSpec`](crate::EventTapSpec)); the compiler emits one
//! [`Op::EventTap`](crate::Op::EventTap) per tap whose port exists, and, for
//! an aligned tap, the same delay an event edge into an output-time sink would
//! get ([`DelayKey::EventTap`](crate::DelayKey::EventTap)). A tap that is not
//! in the topology compiles to nothing. Opening one refuses a port that does
//! not exist then ([`MissingSource::EventPort`](crate::MissingSource)); a port
//! that goes missing later (a node replaced by one with fewer event outputs, a
//! removed node) is read as no events: the commit succeeds, the reader is not
//! told, and nothing counts as lost.
//!
//! What crosses is [`EventKind::Midi`](crate::EventKind) only, each as a
//! [`TapEvent`]: the packet and the absolute [`Frame`] it was due on (the
//! block's frame plus its offset, after any alignment delay). Ramps, segments,
//! harmony and resync are the graph's own control, and are neither copied nor
//! counted.
//!
//! **The frames are on the clock of the rate the reader was opened at**
//! ([`EventTapReader::rate`]). A re-prepare at another rate moves the graph's
//! clock to the same wall time on a new scale; the reader of a tap opened
//! before it keeps the old figure, so whoever re-prepares reopens the tap
//! (`tutti-session`'s `Graph::reopen_event_taps`), and one reader's frames are
//! always on one scale.
//!
//! The ring is `ringbuf`'s single-producer, single-consumer heap ring, already
//! this crate's dependency for the editor-to-executor queue; no new lock-free
//! structure. The producer is owned by the executor (it rides a commit in, as
//! an audio tap's does), the consumer by the caller, so the audio thread takes
//! no lock and frees nothing: a producer left behind by a plan that no longer
//! names it is retired with the old state, on the control side.
//!
//! Dropping the [`EventTapReader`] first is safe. The tap then finds no reader
//! and writes nothing, and the ring is freed with the writer when the tap is
//! closed ([`Editor::close_event_tap`](crate::Editor::close_event_tap)) or
//! reaped ([`Editor::reap_event_taps`](crate::Editor::reap_event_taps)), on
//! the control side. Dropping the reader last frees the ring on the reader's
//! thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_rt::RtScope;
use tutti_types::{Frame, SampleRate};
use tutti_ump::Ump;

use crate::EventTapId;
use crate::event::{EventKind, note_off_room};
use crate::node::Env;
use crate::slots::EventSlots;

/// One event a tap copied out: the packet, and the frame of the graph's clock
/// it was due on.
///
/// The frame is `Env::frame_at(offset)` of the block the tap's op ran in, so
/// it keeps the spacing between events whatever the block sizes were, orders
/// events across taps and against the transport's `At::Frame`, and is a
/// `u64`: it does not wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapEvent {
    /// The frame it was due on, on the clock of [`EventTapReader::rate`].
    pub frame: Frame,
    /// The MIDI packet.
    pub ump: Ump,
}

// An event tap's element is what the ring holds by value: a frame and a
// packet, 24 bytes. Asserted here rather than discovered in a ring's size.
const _: () = assert!(std::mem::size_of::<TapEvent>() == 24);

/// The executor's end of an event tap: the ring producer.
///
/// One thread writes and one reads, which is why the occupancy count below is
/// safe: a ring's vacancy only grows under the reader, so the occupancy the
/// writer takes can only be an overestimate. An event may be refused that the
/// reader had just made room for, and none is taken past the ring.
pub struct EventTapWriter {
    ring: HeapProd<TapEvent>,
    /// The events held before anything but a note-off is refused.
    capacity: usize,
    /// Events the ring refused, shared with the reader.
    lost: Arc<AtomicU64>,
}

impl std::fmt::Debug for EventTapWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventTapWriter")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl Drop for EventTapWriter {
    fn drop(&mut self) {
        RtScope::check_outside("an event tap writer");
    }
}

/// [`Op::EventTap`](crate::Op::EventTap): every MIDI event of event slot `src`
/// into the tap's ring, stamped with its frame in the block `env` describes.
///
/// A non-note-off is refused once `capacity` events are held, a note-off once
/// the ring (`note_off_room(capacity)`) is full, and each refusal is counted
/// once. With no reader there is nothing to write or count. Allocates nothing.
#[inline]
pub(crate) fn event_tap_op<E: EventSlots + ?Sized>(
    tap: &mut EventTapWriter,
    events: &E,
    src: u32,
    env: &Env,
) {
    if !tap.ring.read_is_held() {
        return;
    }
    let total = tap.ring.capacity().get();
    let mut lost = 0u64;
    for event in events.ev(src) {
        let EventKind::Midi(ump) = event.kind else {
            continue;
        };
        let held = total - tap.ring.vacant_len();
        if held >= tap.capacity && (held >= total || !ump.is_note_off()) {
            lost += 1;
            continue;
        }
        let pushed = tap.ring.try_push(TapEvent {
            frame: env.frame_at(event.offset),
            ump,
        });
        if pushed.is_err() {
            lost += 1;
        }
    }
    if lost > 0 {
        tap.lost.fetch_add(lost, Ordering::Relaxed);
    }
}

/// The caller's end of an event tap: the MIDI events one port carried, each
/// with the frame it was due on.
///
/// One consumer: not `Clone`, and it reads through `&mut self`; a second
/// consumer opens a second tap.
///
/// ```compile_fail
/// fn is_clone<T: Clone>() {}
/// is_clone::<tutti_graph::EventTapReader>();
/// ```
///
/// Drain it from a thread of your own. Dropping it leaves the tap open until
/// the editor closes or reaps it; the audio thread then writes nothing for it.
/// It must not be dropped on the audio thread (debug builds panic there).
pub struct EventTapReader {
    id: EventTapId,
    ring: HeapCons<TapEvent>,
    lost: Arc<AtomicU64>,
    rate: SampleRate,
    /// The editor holds a `Weak` of this: when it dies, the reader is gone.
    alive: Arc<()>,
}

impl std::fmt::Debug for EventTapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventTapReader")
            .field("id", &self.id)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl Drop for EventTapReader {
    fn drop(&mut self) {
        RtScope::check_outside("an event tap reader");
    }
}

impl EventTapReader {
    /// A reader and the writer the executor will fill it with: a ring that
    /// holds [`note_off_room`] of `capacity` events (`capacity` at least one),
    /// of which a non-note-off may take `capacity`; its frames are on the
    /// clock of `rate`.
    pub(crate) fn pair(
        id: EventTapId,
        capacity: usize,
        rate: SampleRate,
    ) -> (Self, EventTapWriter) {
        let capacity = capacity.max(1);
        let lost = Arc::new(AtomicU64::new(0));
        let (prod, cons) = HeapRb::<TapEvent>::new(note_off_room(capacity)).split();
        let reader = Self {
            id,
            ring: cons,
            lost: Arc::clone(&lost),
            rate,
            alive: Arc::new(()),
        };
        (
            reader,
            EventTapWriter {
                ring: prod,
                capacity,
                lost,
            },
        )
    }

    /// Pops events into `out`, oldest first, as many as fit; returns the
    /// count. The rest of `out` is as it was.
    pub fn pop_into(&mut self, out: &mut [TapEvent]) -> usize {
        self.ring.pop_slice(out)
    }

    /// Events readable now.
    #[must_use]
    pub fn available(&self) -> usize {
        self.ring.occupied_len()
    }

    /// Events the executor could not push since the last call, and clears the
    /// count: nonzero is a gap in what was read.
    pub fn take_lost(&self) -> u64 {
        self.lost.swap(0, Ordering::Relaxed)
    }

    /// The tap's id, the one
    /// [`Editor::close_event_tap`](crate::Editor::close_event_tap) takes.
    #[must_use]
    pub fn id(&self) -> EventTapId {
        self.id
    }

    /// The sample rate of the clock every popped frame is on: the graph's
    /// rate when the tap was opened. A re-prepare at another rate does not
    /// change it; the tap is reopened instead.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// A handle to see whether this reader is still alive: its strong count
    /// is zero once the reader is dropped.
    #[must_use]
    pub fn watch(&self) -> Weak<()> {
        Arc::downgrade(&self.alive)
    }
}
