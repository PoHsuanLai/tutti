//! [`MidiClipNode`]: a MIDI clip as a graph node, playing its events
//! out of an event port.
//!
//! The node reads the transport from its block's [`Env`], not from a shared
//! timeline: every block it asks, segment by segment, which of its events
//! playback reaches inside the segment, and writes each on its frame. It keeps
//! **no play cursor**. Which events fall in a block is a function of the
//! block's transport and the clip, answered by a binary search over the
//! sorted events and the engine's one beat→frame rule ([`Env::due`]), so a
//! seek, a loop wrap or a tempo change inside a block needs no bookkeeping,
//! and an offline fork plays the clip on its render's `Env`.
//!
//! What it does keep is the set of notes it has started and not ended
//! ([`HeldNotes`]): when playback stops, jumps (a seek, a loop wrap), or the
//! clip is replaced, those notes would hang in whatever it feeds, so it ends
//! each with a note-off on the frame the jump happens.
//!
//! A key is held or not (the MIDI convention), by one note at a time, as a
//! DAW plays notes of one key that overlap: a note-on of a key it already
//! holds ends the sounding note first, on the same frame, and the new note
//! owns the key. From then on only the owning note's note-off ends the key;
//! any other note's note-off for it is swallowed. So what it feeds never has
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_core::{ArcLend, ChannelLayout, Reader};
use tutti_graph::{
    Cx, Event, EventWriter, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts,
    Offset, Prepare, Shape, Status,
};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

use super::timed::TimedMidiEvent;
use super::walk::{Beated, Visit, Walk};

/// The most events a [`MidiClipNode`] writes to its port in one block, its
/// declared [`event_capacity`](Shape::event_capacity). A block that reaches
/// more (a dense clip in a long block) has the rest refused, and the graph
/// counts them (`Executor::dropped_events`); note-offs the node owes are
/// retried the next block.
pub const CLIP_EVENT_CAPACITY: u32 = 256;

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
    /// ends; [`NO_NOTE`] for a note-off that ends none, and for any other
    /// message. An id is the note-on's index in the sorted clip.
    note: u32,
}

/// The [`ClipEvent::note`] of an event that belongs to no note.
const NO_NOTE: u32 = u32::MAX;

/// What the node and its controls share.
struct Shared {
    /// What the audio thread reads, once per block, through the node's
    /// reader.
    cell: Arc<ArcLend<ClipEvents>>,
    /// The clip last published, for the control side (a fork's snapshot, a
    /// count). Control thread only.
    current: Mutex<Arc<ClipEvents>>,
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
            current: Mutex::new(first),
            next_generation: AtomicU64::new(1),
        })
    }

    fn current(&self) -> Arc<ClipEvents> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
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
        .map(|(i, timed)| {
            (
                i as u32,
                ClipEvent {
                    timed,
                    note: NO_NOTE,
                },
            )
        })
        .collect();
    // Each key's note-ons in list order, and a cursor past the taken ones at
    // its head (so notes that do not overlap pair in one step each).
    let mut ons: HashMap<(usize, usize, u32), (Vec<u32>, usize)> = HashMap::new();
    for (i, (_, e)) in v.iter().enumerate() {
        if let Some((g, c, n, true)) = note_of(&e.timed.event) {
            ons.entry((g, c, n)).or_default().0.push(i as u32);
        }
    }
    let mut taken = vec![false; v.len()];
    for i in 0..v.len() {
        let Some((g, c, n, false)) = note_of(&v[i].1.timed.event) else {
            continue;
        };
        let Some((list, first)) = ons.get_mut(&(g, c, n)) else {
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
            v[o as usize].1.note = o;
            v[i].1.note = o;
        }
    }
    // A note-on no note-off ends is a note too.
    for (i, (_, e)) in v.iter_mut().enumerate() {
        if e.note == NO_NOTE && matches!(note_of(&e.timed.event), Some((.., true))) {
            e.note = i as u32;
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
            if e.note != NO_NOTE {
                e.note = to_sorted[e.note as usize];
            }
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

impl MidiClipControls {
    /// Play `events` from the next block on. The node ends every note the
    /// previous clip left sounding, on that block's first frame. Events equal
    /// to the clip's (in beat order, each note-off paired to the same
    /// note-on) change nothing: no note is cut.
    pub fn set_events(&self, events: impl IntoIterator<Item = TimedMidiEvent>) {
        let events = sorted(events);
        let mut current = self
            .shared
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if current.events == events {
            return;
        }
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        let clip = Arc::new(ClipEvents { generation, events });
        *current = Arc::clone(&clip);
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
/// use tutti_core::{Beat, NodeKey, SampleRate, Samples};
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
/// use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let on = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
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
}

/// Where playback was, and what it left sounding.
struct Play {
    walk: Walk,
    held: HeldNotes,
}

impl MidiClipNode {
    /// A node playing `events` (in any order: they are sorted by beat).
    pub fn new(events: impl IntoIterator<Item = TimedMidiEvent>) -> Self {
        Self::over(Shared::new(sorted(events)))
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
                held: HeldNotes::new(),
            },
        }
    }
}

impl Beated for ClipEvent {
    fn beat(&self) -> f64 {
        self.timed.beat.get()
    }
}

/// The clip's side of the walk: write each event, and end what is held
/// wherever playback stops or jumps (a seek, a loop wrap).
struct Playing<'a, 'w> {
    held: &'a mut HeldNotes,
    out: &'a mut EventWriter<'w>,
}

impl Visit<ClipEvent> for Playing<'_, '_> {
    fn stop(&mut self, at: Option<Offset>) {
        // Stopped (or no usable tempo): nothing plays, and nothing may keep
        // sounding.
        if let Some(o) = at {
            self.held.release(self.out, o);
        }
    }

    fn jump(&mut self, at: Option<Offset>, _beat: f64, _fpb: f64, _events: &[ClipEvent]) {
        if let Some(o) = at {
            self.held.release(self.out, o);
        }
    }

    fn event(&mut self, at: Offset, e: &ClipEvent) {
        self.held.play(self.out, at, &e.timed.event, e.note);
    }
}

impl Node for MidiClipNode {
    /// No audio, one MIDI event output of [`CLIP_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(CLIP_EVENT_CAPACITY)
            // Event-only: a walk over the block's events, nothing per frame
            // (measured about 0.3 µs a block at any length; a gain's 256
            // frames, 0.2).
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let clip = self.clip.lend();
        let out = io.event_out(0);
        if self.play.held.owed {
            if let Some(o) = cx.env.offset(0) {
                self.play.held.release(out, o);
            }
        }
        if clip.generation != self.generation {
            self.generation = clip.generation;
            if let Some(o) = cx.env.offset(0) {
                self.play.held.release(out, o);
            }
        }
        let Play { walk, held } = &mut self.play;
        let mut playing = Playing { held, out };
        for (start, seg) in cx.env.segments() {
            walk.segment(&clip.events, cx.env, start, &seg, &mut playing);
        }
        Status::Modified
    }

    /// Forget what was sounding and where playback was: a reset starts the
    /// node over, and whatever it fed is reset with it.
    fn reset(&mut self) {
        self.play.held.clear();
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
            current: Mutex::new(clip),
            next_generation: AtomicU64::new(0),
        });
        Ok(Forked::new(Box::new(MidiClipNode::over(shared))))
    }
}

/// The notes a node has started and not ended, per group, channel and note
/// number: what it owes a note-off when playback jumps.
struct HeldNotes {
    /// `bits[group * 16 + channel]`, bit `note`.
    bits: Box<[u128; 256]>,
    /// The note that owns each held key, `owner[(group * 16 + channel) *
    /// 128 + note]`: only its note-off ends the key. Read only while the
    /// key's bit is set; allocated once, with the node.
    owner: Box<[u32]>,
    count: usize,
    /// A release the port refused part of: the rest goes out at the next
    /// block's first frame, and every block's after, until none is held.
    owed: bool,
}

/// A MIDI channel-voice note message's `(group, channel, note)` and whether
/// it starts a note (`Some(true)`) or ends one (`Some(false)`). A MIDI 1.0
/// note-on at velocity 0 ends one; a MIDI 2.0 note-on never does.
fn note_of(e: &MidiEvent) -> Option<(usize, usize, u32, bool)> {
    let w = e.data[0];
    let mt = w >> 28;
    if mt != 0x2 && mt != 0x4 {
        return None;
    }
    let status = (w >> 20) & 0xf;
    let on = match status {
        0x9 => mt == 0x4 || w & 0x7f != 0,
        0x8 => false,
        _ => return None,
    };
    let group = ((w >> 24) & 0xf) as usize;
    let channel = ((w >> 16) & 0xf) as usize;
    Some((group, channel, (w >> 8) & 0x7f, on))
}

impl HeldNotes {
    fn new() -> Self {
        Self {
            bits: Box::new([0; 256]),
            owner: vec![NO_NOTE; 256 * 128].into_boxed_slice(),
            count: 0,
            owed: false,
        }
    }

    fn track(&mut self, e: &MidiEvent) {
        let Some((g, c, n, on)) = note_of(e) else {
            return;
        };
        let (slot, bit) = (&mut self.bits[g * 16 + c], 1u128 << n);
        match (on, *slot & bit != 0) {
            (true, false) => {
                *slot |= bit;
                self.count += 1;
            }
            (false, true) => {
                *slot &= !bit;
                self.count -= 1;
            }
            _ => {}
        }
    }

    /// Write `e`, of note `note` ([`ClipEvent::note`]), at `at`, keeping one
    /// note per key, the one that last started it: a note-on of a key this
    /// holds ends the sounding note first, on the same frame, and takes the
    /// key; a note-off ends the key only when its note owns it, and is
    /// swallowed otherwise (its note was ended when a newer one took the
    /// key, or never started). A note-off of no note is written as it comes.
    ///
    /// A refused write leaves the state as the port has it: a refused
    /// note-off leaves the key held by its owner (the port is full, and the
    /// note-on after it is refused and counted too), a refused note-on
    /// leaves the key unheld.
    fn play(&mut self, out: &mut EventWriter<'_>, at: Offset, e: &MidiEvent, note: u32) {
        if let Some((g, c, n, on)) = note_of(e) {
            let (i, bit) = (g * 16 + c, 1u128 << n);
            let key = i * 128 + n as usize;
            let held = self.bits[i] & bit != 0;
            if on {
                if held {
                    let off = MidiEvent::note_off(
                        MidiGroup::new(g as u8),
                        MidiChannel::new(c as u8),
                        n as u8,
                        0,
                    );
                    if out.push(Event::midi(at, off.data)).is_err() {
                        let _ = out.push(Event::midi(at, e.data));
                        return;
                    }
                    if out.push(Event::midi(at, e.data)).is_ok() {
                        self.owner[key] = note;
                    } else {
                        self.bits[i] &= !bit;
                        self.count -= 1;
                    }
                } else if out.push(Event::midi(at, e.data)).is_ok() {
                    self.bits[i] |= bit;
                    self.count += 1;
                    self.owner[key] = note;
                }
                return;
            }
            if note != NO_NOTE {
                if held && self.owner[key] == note && out.push(Event::midi(at, e.data)).is_ok() {
                    self.bits[i] &= !bit;
                    self.owner[key] = NO_NOTE;
                    self.count -= 1;
                }
                return;
            }
        }
        if out.push(Event::midi(at, e.data)).is_ok() {
            self.track(e);
        }
    }

    /// End every held note at `at`. A note-off the port refuses stays held,
    /// and goes out the next time.
    fn release(&mut self, out: &mut EventWriter<'_>, at: Offset) {
        if self.count == 0 {
            self.owed = false;
            return;
        }
        for (i, slot) in self.bits.iter_mut().enumerate() {
            while *slot != 0 {
                let n = slot.trailing_zeros();
                let off = MidiEvent::note_off(
                    MidiGroup::new((i / 16) as u8),
                    MidiChannel::new((i % 16) as u8),
                    n as u8,
                    0,
                );
                if out.push(Event::midi(at, off.data)).is_err() {
                    self.owed = true;
                    return;
                }
                *slot &= !(1u128 << n);
                self.owner[i * 128 + n as usize] = NO_NOTE;
                self.count -= 1;
            }
        }
        self.owed = false;
    }

    fn clear(&mut self) {
        self.bits.fill(0);
        self.owner.fill(NO_NOTE);
        self.count = 0;
        self.owed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::Beat;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// **A clip a block still held when `set_events` replaced it is freed by
    /// the next `reclaim`**, with no further publish.
    ///
    /// Mutation (run): `MidiClipControls::reclaim` returning 0 without
    /// reclaiming → the first clip is still alive, fails.
    #[test]
    fn reclaim_frees_a_clip_a_block_held_across_its_replacement() {
        let on = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
        let mut node = MidiClipNode::new([TimedMidiEvent::new(Beat(1.0), on)]);
        let controls = MidiClipControls {
            shared: Arc::clone(&node.shared),
        };
        let gone = Arc::downgrade(&node.shared.current());
        {
            // A block in progress across the replacement.
            let _held = node.clip.lend();
            controls.clear();
        }
        assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
        assert_eq!(controls.reclaim(), 0);
        assert!(gone.upgrade().is_none(), "freed by the reclaim");
    }
}
