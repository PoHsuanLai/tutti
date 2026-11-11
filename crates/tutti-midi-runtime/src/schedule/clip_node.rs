//! [`MidiClipNode`]: a MIDI clip as a graph node, playing its events
//! out of an event port.
//!
//! The node reads the transport from its block's [`Env`](tutti_graph::Env), not from a shared
//! timeline: every block it asks, segment by segment, which of its events
//! playback reaches inside the segment, and writes each on its frame. It keeps
//! **no play cursor**. Which events fall in a block is a function of the
//! block's transport and the clip, answered by a binary search over the
//! sorted events and the engine's one beat→frame rule ([`Env::due`](tutti_graph::Env::due)), so a
//! seek, a loop wrap or a tempo change inside a block needs no bookkeeping,
//! and an offline fork plays the clip on its render's `Env`.
//!
//! What it does keep is the set of notes it has started and not ended
//! (a [`NoteLedger`]): when playback stops, jumps (a seek, a loop wrap), or the
//! clip is replaced, those notes would hang in whatever it feeds, so it ends
//! each with a note-off on the frame the jump happens, and then returns the
//! hold pedals (CC 64, 66, 67, 69) and the pitch bend it left off their
//! normal position to it, on the same frame (MIDI 1.0 Detailed Spec p. 31,
//! Stop; the pedals after the offs because a pedal down takes priority over
//! Note Off, A-5). It writes only through
//! the ledger's [`Sending`], which pays a note-off the port refused before
//! anything else goes out.
//!
//! A key (group, channel, note number) is held or not (the MIDI convention),
//! by one note at a time, as a DAW plays notes of one key that overlap (M2-104
//! Appendix C.2 would allow several, told apart by a Detach, which a clip does
//! not carry): a note-on of a key it already holds ends the sounding note
//! first, on the same frame, and the new note owns the key. From then on only
//! the owning note's note-off ends the key; any other note's note-off for it
//! is swallowed. So what it feeds never has
//! two of its voices on one key, a note nested inside another ends at its
//! own end, overlaps of any depth leave the newest note sounding to its
//! end, and nothing is left sounding.
//!
//! Which note-off is a note's is decided when the clip is set: a note-off
//! belongs to the earliest-listed note-on of its key that is at or before
//! it in time and not already taken, in the order the events are given. A
//! host that lists each note's on and off together gets its notes exactly.
//! A list already in time order (a parsed file) pairs notes of one key
//! first on, first off, which is right for notes that overlap and, for a
//! note nested inside a longer one, keeps the inner note sounding to the
//! outer's end: a time-ordered list cannot tell those two apart. A note-off
//! that belongs to no note-on is written as it comes, and ends whatever note
//! holds its key.
//!
//! The clip itself is published to the node with [`ArcLend`]
//! ([`MidiClipControls::set_events`]); the node reads it once per block.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_graph::{
    Cx, EventDensity, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io,
    MIN_EVENT_PORT_CAPACITY, Node, NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_protocol::Translator;
use tutti_rt::{ArcLend, Reader};
use tutti_ump::Ump;

use crate::ledger::{Class, NoteKey, NoteLedger, Sending};

use super::timed::TimedMidiEvent;
use super::walk::{Beated, Visit, Walk};

/// Keys a clip node tracks at once, held or refused. One group's 16 channels
/// of 128 notes is 2,048, so a clip on one group never fills it; a note-on
/// that finds it full is withheld and counted, never dropped silently. At 12
/// bytes a key the table is 24 KiB, allocated with the node.
const LEDGER_CAPACITY: usize = 2048;

/// The events per frame a [`MidiClipNode`]'s port is sized for, which with
/// its block makes its declared [`event_capacity`](Shape::event_capacity)
/// ([`EventDensity::capacity`]): one a frame, from 256 events for a short
/// block to 8,192 (`MAX_EVENT_PORT_CAPACITY`, the engine's default largest
/// block). A clip's events are on distinct frames
/// but for a chord, which the floor covers; 48,000 events a second is a
/// 16-channel MPE recording with a bend, a pressure and a slide each
/// millisecond on every channel.
///
/// Memory, with the note-off room (`note_off_room`: capacity, an eighth and 8)
/// at 32 bytes an event: the port's slot is 9.5 KB at 256 events, 37 KB at
/// 1,024 and 295 KB at 8,192 (slots are shared where lifetimes allow). Each
/// PDC delay on the port adds a FIFO of `3.75 ×` that many entries of 40
/// bytes and an output slot of 32, 72 bytes an entry: 81 KB, 314 KB and 2.5 MB
/// for a delay no longer than the block, more for a longer one (`⌈len /
/// block⌉ + 2` blocks of the rate, not 3).
///
/// A block that reaches more has the rest refused, and the graph counts them
/// (`Editor::take_event_losses`); a note-off the port refuses stays owed and
/// goes out in a following block, and the note-offs have room past the
/// capacity ([`note_off_room`](tutti_graph::note_off_room)).
pub const CLIP_EVENT_DENSITY: EventDensity = EventDensity::per_frame(1);

/// A published clip: its events sorted by beat, and a generation that tells
/// the node a new clip replaced the one it was playing.
struct ClipEvents {
    generation: u64,
    events: Box<[ClipEvent]>,
}

/// One event of a clip, with the note it starts or ends.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ClipEvent {
    timed: TimedMidiEvent,
    /// For a note-on, its own id; for a note-off, the id of the note-on it
    /// ends; `None` for a note-off that ends none, and for any other message.
    note: Option<ClipNote>,
}

/// A note's id: the index of its note-on in the sorted clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClipNote(NonZeroU32);

impl ClipNote {
    fn from_index(index: u32) -> Self {
        Self(NonZeroU32::new(index + 1).expect("clip index below u32::MAX"))
    }

    fn index(self) -> u32 {
        self.0.get() - 1
    }
}

/// What the node and its controls share.
struct Shared {
    /// What the audio thread reads, once per block, through the node's
    /// reader.
    cell: Arc<ArcLend<ClipEvents>>,
    /// Serializes `set`'s compare, generation and publish (`ArcLend` has no
    /// compare-and-publish), so two control threads cannot each publish
    /// against a stale read. The value is the cell's own: the control side
    /// reads it with `ArcLend::current`. The node never takes it.
    publish: Mutex<()>,
    next_generation: AtomicU64,
}

impl Shared {
    fn new(events: Box<[ClipEvent]>) -> Arc<Self> {
        let first = Arc::new(ClipEvents {
            generation: 0,
            events,
        });
        Arc::new(Self {
            cell: Arc::new(ArcLend::from_arc(Arc::clone(&first))),
            publish: Mutex::new(()),
            next_generation: AtomicU64::new(1),
        })
    }

    fn current(&self) -> Arc<ClipEvents> {
        self.cell.current()
    }
}

fn class_of(timed: &TimedMidiEvent) -> Class {
    Class::of(timed.event)
}

/// `events` through one [`Translator`], in beat order (design 017 §4: a clip
/// is translated as it is loaded, so the graph carries MIDI 2.0 from it as
/// from a wire), each translated event at its source's beat and in its
/// source's place in the list, which the note pairing below reads. An event
/// the translator absorbs (an RPN select, Data Entry, Bank Select) is gone; a
/// select or Data Entry that ends a run is replaced by the run's controller.
/// A beat that is not finite is dropped here, as [`sorted`] would. Control
/// thread.
fn translated(events: impl IntoIterator<Item = TimedMidiEvent>) -> Vec<TimedMidiEvent> {
    let events: Vec<TimedMidiEvent> = events.into_iter().collect();
    let mut order: Vec<usize> = (0..events.len())
        .filter(|&i| events[i].beat.get().is_finite())
        .collect();
    // Stable: events on one beat are translated in list order.
    order.sort_by(|&a, &b| events[a].beat.get().total_cmp(&events[b].beat.get()));
    let mut out: Vec<Option<Ump>> = vec![None; events.len()];
    let mut translator = Translator::new();
    for i in order {
        // The translator sends at most one packet per packet fed.
        translator.feed(events[i].event, |u| out[i] = Some(u));
    }
    events
        .iter()
        .zip(out)
        .filter_map(|(e, u)| u.map(|u| TimedMidiEvent::new(e.beat, u)))
        .collect()
}

/// `events` with each note-off paired to its note-on (see the module docs),
/// sorted by beat, stably (events on one beat keep their order: a note-off
/// before a note-on of the same note stays first). A beat that is not finite
/// has no frame, and is dropped. A note's id is its note-on's index in the
/// sorted clip, so two clips with the same events and the same pairing are
/// equal. Control thread.
fn sorted(events: impl IntoIterator<Item = TimedMidiEvent>) -> Box<[ClipEvent]> {
    let mut v: Vec<(u32, ClipEvent)> = events
        .into_iter()
        .filter(|e| e.beat.get().is_finite())
        .enumerate()
        .map(|(i, timed)| (i as u32, ClipEvent { timed, note: None }))
        .collect();
    // Each key's note-ons in list order, and a cursor past the taken ones at
    // its head (so notes that do not overlap pair in one step each).
    let mut ons: HashMap<NoteKey, (Vec<u32>, usize)> = HashMap::new();
    for (i, (_, e)) in v.iter().enumerate() {
        if let Class::On(key) = class_of(&e.timed) {
            ons.entry(key).or_default().0.push(i as u32);
        }
    }
    let mut taken = vec![false; v.len()];
    for i in 0..v.len() {
        let Class::Off(key) = class_of(&v[i].1.timed) else {
            continue;
        };
        let Some((list, first)) = ons.get_mut(&key) else {
            continue;
        };
        while *first < list.len() && taken[list[*first] as usize] {
            *first += 1;
        }
        let beat = v[i].1.timed.beat.get();
        let found = list[*first..]
            .iter()
            .copied()
            .find(|&o| !taken[o as usize] && v[o as usize].1.timed.beat.get() <= beat);
        if let Some(o) = found {
            taken[o as usize] = true;
            let id = Some(ClipNote::from_index(o));
            v[o as usize].1.note = id;
            v[i].1.note = id;
        }
    }
    // A note-on no note-off ends is a note too.
    for (i, (_, e)) in v.iter_mut().enumerate() {
        if e.note.is_none() && matches!(class_of(&e.timed), Class::On(_)) {
            e.note = Some(ClipNote::from_index(i as u32));
        }
    }
    v.sort_by(|a, b| a.1.timed.beat.get().total_cmp(&b.1.timed.beat.get()));
    // List index → sorted index, to renumber each note by its sorted note-on.
    let mut to_sorted = vec![0u32; v.len()];
    for (k, &(i, _)) in v.iter().enumerate() {
        to_sorted[i as usize] = k as u32;
    }
    v.into_iter()
        .map(|(_, mut e)| {
            e.note = e
                .note
                .map(|id| ClipNote::from_index(to_sorted[id.index() as usize]));
            e
        })
        .collect()
}

/// The handle a host keeps on an inserted [`MidiClipNode`]: replace or clear
/// the clip it plays. Control thread.
#[derive(Clone)]
pub struct MidiClipControls {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for MidiClipControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiClipControls").finish_non_exhaustive()
    }
}

impl MidiClipControls {
    /// Play `events` from the next block on. The node ends every note the
    /// previous clip left sounding, on that block's first frame. Events equal
    /// to the clip's (in beat order, each note-off paired to the same
    /// note-on) change nothing: no note is cut.
    pub fn set_events(&self, events: impl IntoIterator<Item = TimedMidiEvent>) {
        let events = sorted(translated(events));
        let _serial = self
            .shared
            .publish
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let current = self.shared.current();
        if current.events == events {
            return;
        }
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        let clip = Arc::new(ClipEvents { generation, events });
        self.shared.cell.publish(clip);
    }

    /// Play nothing (and end what is sounding).
    pub fn clear(&self) {
        self.set_events([]);
    }

    /// Frees the clips the node has let go of since the last
    /// [`set_events`](Self::set_events) (one a block still held when it was
    /// replaced), without publishing; returns how many are still held.
    /// Control thread, from the owner's tick (a session's `Reclaim` phase
    /// sweeps its own clips); with nothing retired it is a lock and a length
    /// check.
    pub fn reclaim(&self) -> usize {
        self.shared.cell.reclaim()
    }

    /// How many events the clip holds.
    pub fn event_count(&self) -> usize {
        self.shared.current().events.len()
    }
}

/// A MIDI clip in the graph: an event source with one MIDI event output, no
/// audio. See the module docs.
///
/// ```
/// use tutti_types::{Beat, SampleRate, Samples};
/// use tutti_graph::NodeKey;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
/// use tutti_fixtures::midi::note_on;
/// use tutti_ump::{MidiChannel, MidiGroup};
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let on = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
/// let clip = MidiClipNode::new([TimedMidiEvent::new(Beat(1.0), on)]);
/// let controls = editor.insert(NodeKey(1), "clip", clip);
/// assert_eq!(controls.event_count(), 1);
/// ```
pub struct MidiClipNode {
    shared: Arc<Shared>,
    /// A reader of `shared.cell`, registered in [`over`](Self::over): a
    /// fork's node is built over its own cell and registers there.
    clip: Reader<ClipEvents>,
    /// The generation of the clip this node last played.
    generation: u64,
    play: Play,
    /// The declared event capacity, from the block the node was prepared for.
    capacity: u32,
}

impl std::fmt::Debug for MidiClipNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiClipNode")
            .field("clip", &self.clip)
            .field("generation", &self.generation)
            .field("play", &self.play)
            .finish_non_exhaustive()
    }
}

/// Where playback was, and what it left sounding.
#[derive(Debug)]
struct Play {
    walk: Walk,
    held: NoteLedger<ClipNote>,
}

impl MidiClipNode {
    /// A node playing `events` (in any order: they are sorted by beat).
    pub fn new(events: impl IntoIterator<Item = TimedMidiEvent>) -> Self {
        Self::over(Shared::new(sorted(translated(events))))
    }

    /// A node over `shared`, reading it through a reader registered here.
    /// Control thread: registering may allocate.
    fn over(shared: Arc<Shared>) -> Self {
        let generation = shared.current().generation;
        Self {
            clip: shared.cell.reader(),
            shared,
            generation,
            play: Play {
                walk: Walk::default(),
                held: NoteLedger::new(LEDGER_CAPACITY),
            },
            capacity: MIN_EVENT_PORT_CAPACITY,
        }
    }
}

impl Beated for ClipEvent {
    fn beat(&self) -> f64 {
        self.timed.beat.get()
    }
}

/// The clip's side of the walk: send each event, and end what is held
/// wherever playback stops or jumps (a seek, a loop wrap).
struct Playing<'a, 'w> {
    sending: Sending<'a, 'w, ClipNote>,
}

impl Visit<ClipEvent> for Playing<'_, '_> {
    fn stop(&mut self, at: Option<Offset>) {
        // Stopped (or no usable tempo): nothing plays, and nothing may keep
        // sounding or stay held down.
        if let Some(o) = at {
            self.sending.release_all(o);
        }
    }

    fn jump(&mut self, at: Option<Offset>, _beat: f64, _fpb: f64, _events: &[ClipEvent]) {
        if let Some(o) = at {
            self.sending.release_all(o);
        }
    }

    fn event(&mut self, at: Offset, e: &ClipEvent) {
        self.sending.send(at, e.timed.event, e.note);
    }
}

impl Node for MidiClipNode {
    /// No audio, one MIDI event output of [`CLIP_EVENT_DENSITY`] per frame of
    /// the block it was prepared for.
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(self.capacity)
            // Event-only: a walk over the block's events, nothing per frame
            // (measured about 0.3 µs a block at any length; a gain's 256
            // frames, 0.2).
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.capacity = CLIP_EVENT_DENSITY.capacity(p);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let clip = self.clip.lend();
        let Some(first) = cx.env.offset(0) else {
            return Status::Modified;
        };
        let replaced = std::mem::replace(&mut self.generation, clip.generation) != clip.generation;
        let Play { walk, held } = &mut self.play;
        let mut playing = Playing {
            sending: held.block(io.event_out(0), first),
        };
        if replaced {
            playing.sending.release_all(first);
        }
        for (start, seg) in cx.env.segments() {
            walk.segment(&clip.events, cx.env, start, &seg, &mut playing);
        }
        Status::Modified
    }

    /// Forget what was sounding and where playback was: a reset starts the
    /// node over, and whatever it fed is reset with it.
    fn reset(&mut self) {
        self.play.held.forget();
        self.play.walk.forget();
    }
}

impl IntoNode for MidiClipNode {
    type Controls = MidiClipControls;

    fn into_parts(self) -> NodeParts<MidiClipControls> {
        let controls = MidiClipControls {
            shared: Arc::clone(&self.shared),
        };
        let fork = ClipFork {
            shared: Arc::clone(&self.shared),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

/// A clip node's fork: a fresh node over the clip as it is when the fork is
/// taken, so an export plays what was there at its start while the live clip
/// is edited. The fork reads its render's `Env`.
struct ClipFork {
    shared: Arc<Shared>,
}

impl ForkSource for ClipFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        let clip = self.shared.current();
        let shared = Arc::new(Shared {
            cell: Arc::new(ArcLend::from_arc(Arc::clone(&clip))),
            publish: Mutex::new(()),
            next_generation: AtomicU64::new(0),
        });
        Ok(Forked::new(Box::new(MidiClipNode::over(shared))))
    }
}

#[cfg(test)]
mod tests;
