//! [`MidiClipNode`]: a MIDI clip as a native graph node, playing its events
//! out of an event port (doc 013, rewrite item 5).
//!
//! The node reads the transport from its block's [`Env`], not from a shared
//! timeline: every block it asks, segment by segment, which of its events
//! playback reaches inside the segment, and writes each on its frame. It keeps
//! **no play cursor**. Which events fall in a block is a function of the
//! block's transport and the clip, answered by a binary search over the
//! sorted events and the engine's one beat→frame rule ([`Env::due`]), so a
//! seek, a loop wrap or a tempo change inside a block needs no bookkeeping,
//! and an offline fork plays the clip on its render's `Env` with nothing to
//! rebind.
//!
//! What it does keep is the set of notes it has started and not ended
//! ([`HeldNotes`]): when playback stops, jumps (a seek, a loop wrap), or the
//! clip is replaced, those notes would hang in whatever it feeds, so it ends
//! each with a note-off on the frame the jump happens.
//!
//! The clip itself is published to the node with [`RtPublish`]
//! ([`MidiClipControls::set_events`]); the node reads it once per block.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_core::{At, ChannelLayout, RtPublish};
use tutti_graph::{
    Cx, Due, Env, Event, EventWriter, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

use super::snapshot::TimedMidiEvent;

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
    events: Box<[TimedMidiEvent]>,
}

/// What the node and its controls share.
struct Shared {
    /// What the audio thread reads, once per block.
    cell: RtPublish<ClipEvents>,
    /// The clip last published, for the control side (a fork's snapshot, a
    /// count). Control thread only.
    current: Mutex<Arc<ClipEvents>>,
    next_generation: AtomicU64,
}

impl Shared {
    fn new(events: Box<[TimedMidiEvent]>) -> Arc<Self> {
        let first = Arc::new(ClipEvents {
            generation: 0,
            events,
        });
        Arc::new(Self {
            cell: RtPublish::from_arc(Arc::clone(&first)),
            current: Mutex::new(first),
            next_generation: AtomicU64::new(1),
        })
    }

    fn current(&self) -> Arc<ClipEvents> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// `events` sorted by beat, stably (events on one beat keep their order: a
/// note-off before a note-on of the same note stays first), a NaN beat last.
fn sorted(events: impl IntoIterator<Item = TimedMidiEvent>) -> Box<[TimedMidiEvent]> {
    let mut v: Vec<TimedMidiEvent> = events.into_iter().collect();
    v.sort_by(|a, b| a.beat.get().total_cmp(&b.beat.get()));
    v.into_boxed_slice()
}

/// The handle a host keeps on an inserted [`MidiClipNode`]: replace or clear
/// the clip it plays. Control thread.
#[derive(Clone)]
pub struct MidiClipControls {
    shared: Arc<Shared>,
}

impl MidiClipControls {
    /// Play `events` from the next block on. The node ends every note the
    /// previous clip left sounding, on that block's first frame.
    pub fn set_events(&self, events: impl IntoIterator<Item = TimedMidiEvent>) {
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        let clip = Arc::new(ClipEvents {
            generation,
            events: sorted(events),
        });
        let mut current = self
            .shared
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *current = Arc::clone(&clip);
        self.shared.cell.publish(clip);
    }

    /// Play nothing (and end what is sounding).
    pub fn clear(&self) {
        self.set_events([]);
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
    /// The generation of the clip this node last played.
    generation: u64,
    play: Play,
}

/// Where playback was, and what it left sounding.
struct Play {
    /// The beat the next segment starts on if playback is continuous; `None`
    /// when stopped (or before the first block).
    expected: Option<f64>,
    held: HeldNotes,
}

impl MidiClipNode {
    /// A node playing `events` (in any order: they are sorted by beat).
    pub fn new(events: impl IntoIterator<Item = TimedMidiEvent>) -> Self {
        Self::over(Shared::new(sorted(events)))
    }

    fn over(shared: Arc<Shared>) -> Self {
        let generation = shared.current().generation;
        Self {
            shared,
            generation,
            play: Play {
                expected: None,
                held: HeldNotes::new(),
            },
        }
    }
}

impl Play {
    /// Play the part of `seg` (the block's piece from `start`) that its
    /// transport reaches, ending held notes where playback jumps.
    fn segment(
        &mut self,
        events: &[TimedMidiEvent],
        block: &Env,
        start: Offset,
        seg: &Env,
        out: &mut EventWriter<'_>,
    ) {
        let t = seg.transport;
        let len = seg.block_len.get();
        let at = |k: usize| Offset::new(start.index() + k, block.block_len);
        let Some(fpb) = frames_per_beat(seg).filter(|_| t.playing) else {
            // Stopped (or no usable tempo): nothing plays, and nothing may
            // keep sounding.
            if let Some(o) = at(0) {
                self.held.release(out, o);
            }
            self.expected = None;
            return;
        };
        let now = t.beat().get();
        let frame = 1.0 / fpb;
        // A jump is a start more than half a frame away from where the last
        // segment left off (a seek, a wrap at a block edge, a restart).
        let continuous = self.expected.is_some_and(|e| ((now - e) * fpb).abs() < 0.5);
        if !continuous {
            if let Some(o) = at(0) {
                self.held.release(out, o);
            }
        }

        // Where the loop, if the playhead is inside it, wraps in this
        // segment: `len` when it does not.
        let looping = t
            .looping
            .filter(|l| l.start.get() < l.end.get() && now < l.end.get());
        let wrap = looping.map_or(len, |l| {
            let k = tutti_core::first_frame_at_or_after((l.end.get() - now) * fpb).max(0);
            usize::try_from(k).map_or(len, |k| k.min(len))
        });

        // Up to the wrap: a beat less than a frame behind the playhead falls
        // on the first frame (`Env::due`), so the range starts a frame back.
        let hi = now + (wrap as f64 + 1.0) * frame;
        self.emit(events, now - frame, hi, seg, out, &at, |k| k < wrap);
        if let (Some(l), true) = (looping, wrap < len) {
            if let Some(o) = at(wrap) {
                self.held.release(out, o);
            }
            let from = l.start.get();
            let hi = from + ((len - wrap) as f64 + 1.0) * frame;
            self.emit(events, from, hi, seg, out, &at, |k| k >= wrap);
        }

        // Where the next segment starts if nothing jumps.
        let mut next = now + len as f64 * frame;
        if let Some(l) = looping {
            if next >= l.end.get() {
                next = l.start.get() + (next - l.end.get());
            }
        }
        self.expected = Some(next);
    }

    /// Write every event with a beat in `[lo, hi)` that playback reaches in
    /// `seg` at an offset `keep` accepts.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        events: &[TimedMidiEvent],
        lo: f64,
        hi: f64,
        seg: &Env,
        out: &mut EventWriter<'_>,
        at: &impl Fn(usize) -> Option<Offset>,
        keep: impl Fn(usize) -> bool,
    ) {
        let first = events.partition_point(|e| e.beat.get() < lo);
        for e in &events[first..] {
            if e.beat.get() >= hi || e.beat.get().is_nan() {
                break;
            }
            let Due::In(k) = seg.due(At::Beat(e.beat)) else {
                continue;
            };
            if !keep(k.index()) {
                continue;
            }
            let Some(offset) = at(k.index()) else {
                continue;
            };
            if out.push(Event::midi(offset, e.event.data)).is_ok() {
                self.held.track(&e.event);
            }
        }
    }
}

/// Frames per beat in `seg`, when its rate and tempo are usable.
fn frames_per_beat(seg: &Env) -> Option<f64> {
    let (rate, tempo) = (seg.sample_rate.get(), seg.transport.tempo.get());
    let fpb = rate * 60.0 / tempo;
    (rate > 0.0 && tempo > 0.0 && fpb.is_finite()).then_some(fpb)
}

impl Node for MidiClipNode {
    /// No audio, one MIDI event output of [`CLIP_EVENT_CAPACITY`].
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(CLIP_EVENT_CAPACITY)
    }

    fn prepare(&mut self, _p: &Prepare) {}

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let clip = self.shared.cell.read();
        let out = io.event_out(0);
        if clip.generation != self.generation {
            self.generation = clip.generation;
            if let Some(o) = cx.env.offset(0) {
                self.play.held.release(out, o);
            }
        }
        for (start, seg) in cx.env.segments() {
            self.play.segment(&clip.events, cx.env, start, &seg, out);
        }
        Status::Modified
    }

    /// Forget what was sounding and where playback was: a reset starts the
    /// node over, and whatever it fed is reset with it.
    fn reset(&mut self) {
        self.play.held.clear();
        self.play.expected = None;
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
/// is edited. Nothing to rebind: the fork reads its render's `Env`.
struct ClipFork {
    shared: Arc<Shared>,
}

impl ForkSource for ClipFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let clip = self.shared.current();
        let shared = Arc::new(Shared {
            cell: RtPublish::from_arc(Arc::clone(&clip)),
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
    count: usize,
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
            count: 0,
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

    /// End every held note at `at`. A note-off the port refuses stays held,
    /// and goes out the next time.
    fn release(&mut self, out: &mut EventWriter<'_>, at: Offset) {
        if self.count == 0 {
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
                    return;
                }
                *slot &= !(1u128 << n);
                self.count -= 1;
            }
        }
    }

    fn clear(&mut self) {
        self.bits.fill(0);
        self.count = 0;
    }
}
