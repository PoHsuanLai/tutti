//! [`NoteEnvelopeNode`]: an ADSR on a MIDI input, sent to control lanes as
//! exact segments (design doc 014 §6).

use tutti_core::{ChannelLayout, SampleRate};
use tutti_graph::{
    Cost, Cx, Event, EventKind, Io, Node, Offset, ParamSegment, Prepare, SegmentShape, Shape,
    Status, Ump,
};
use tutti_mod::{Adsr, SignalKind, SourceId, SourceNode};
use tutti_types::{Frame, SamplePosition, Samples};

use super::PlayWalk;

/// Segments a note envelope sends per block, at most. Several on one frame
/// are one (the last: a lane reads only the latest), so this is 256 frames
/// with a note or a stage boundary in one block. Past it the envelope
/// sends nothing more that block and re-sends where it stands at the next
/// block's first frame.
pub const NOTE_ENVELOPE_EVENT_CAPACITY: u32 = 256;

/// Held notes: one bit per (group, channel, note), 16 × 16 × 128.
const NOTE_WORDS: usize = 16 * 16 * 128 / 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// A MIDI note-on (`true`) or note-off (`false`) and its key, from a MIDI
/// 1.0 or MIDI 2.0 channel-voice message. A MIDI 1.0 note-on at velocity 0
/// is a note-off; MIDI 2.0 has no such spelling.
fn note(e: &Event) -> Option<(bool, usize)> {
    let EventKind::Midi(Ump(w)) = e.kind else {
        return None;
    };
    let w = w[0];
    let (mt, status) = (w >> 28, (w >> 20) & 0xf);
    let on = match (mt, status) {
        (0x2, 0x9) => w & 0x7f != 0,
        (0x2 | 0x4, 0x8) => false,
        (0x4, 0x9) => true,
        _ => return None,
    };
    // Group, channel, note: the top 15 bits of the key.
    let key = (((w >> 24) & 0xf) << 11) | (((w >> 16) & 0xf) << 7) | ((w >> 8) & 0x7f);
    Some((on, key as usize))
}

/// A **note envelope**: an ADSR driven by its MIDI input, sent raw in
/// `[0, 1]` as segments addressed `ParamAddr::Id(source.0)` for every
/// control lane that reads `source`.
///
/// - **Exact, no grid.** It sends one segment at each note-on and note-off
///   and at the attack's end, each from the level there: a linear attack
///   to 1 over `attack`, a linear decay to `sustain` over `decay`, held,
///   and a linear release to 0 over `release`, each length rounded to whole
///   frames (`Seconds::to_samples`). So the peak lands on the frame
///   `attack` after the note-on, and the sustain level `decay` after that;
///   a segment holds its end, so the sustain and the release's end need no
///   segment of their own. A zero-length stage is skipped on its frame.
///   Linear stages are exact through a lane, whose pieces are linear
///   between its points (an input's arrival and each event among them).
/// - **Mono, last-note priority, retrigger.** Every note-on restarts the
///   attack, from the level there (no jump to 0, so no click). A note-off
///   releases only when it lifts the last held note: lifting one of
///   several leaves the envelope where it is, sounding for the note still
///   held. A note-off for a note not held does nothing. Notes are told
///   apart by group, channel and note number.
/// - **Release from the level there**: a note-off mid-attack releases from
///   the attack's current level, over the full `release`.
/// - **It resets at play start**: on the frame the play clock restarts,
///   every held note is forgotten and the envelope is idle at 0; it sends
///   a 0 there unless it was idle already. A note-on on that same frame (a
///   clip's first note) then starts from 0.
///
/// - **It sends only on events**, so a lane that starts reading it while it
///   holds a sustain (a newly wired route) would read 0 until its next note
///   or stage boundary. An [`EventKind::Resync`] on its input closes that:
///   it re-sends where it stands, resumed, on the event's frame (doc 014
///   §6). The [`Modulation`](crate::Modulation) reconciler schedules one
///   whenever a lane's program newly reads the envelope.
///
/// Insert it `ForkByClone(node)`: it shares nothing with a clone, and a
/// fork's (the node as inserted, reset) starts idle, as a live play start
/// leaves it.
#[derive(Clone, Debug)]
pub struct NoteEnvelopeNode {
    source: u32,
    adsr: Adsr,
    /// Attack, decay and release in frames at the prepared rate.
    attack: Samples,
    decay: Samples,
    release: Samples,
    sustain: f32,
    held: [u64; NOTE_WORDS],
    holding: u32,
    stage: Stage,
    /// The segment last sent and the frame it was sent on: the level.
    level: Option<(ParamSegment, Frame)>,
    /// The frame the current stage ends on (the attack's, the decay's, the
    /// release's), if it does.
    boundary: Option<Frame>,
    /// The play clock's epoch at the last frame read; `None` while fresh.
    epoch: Option<u32>,
    /// Segments were refused this block: re-send `level` at the next
    /// block's first frame.
    owed: bool,
}

impl NoteEnvelopeNode {
    /// A note envelope of `adsr`, sending as `source`.
    pub fn new(source: SourceId, adsr: Adsr) -> Self {
        let mut node = Self {
            source: source.0,
            adsr,
            attack: Samples(0),
            decay: Samples(0),
            release: Samples(0),
            sustain: 0.0,
            held: [0; NOTE_WORDS],
            holding: 0,
            stage: Stage::Idle,
            level: None,
            boundary: None,
            epoch: None,
            owed: false,
        };
        node.lengths(SampleRate::DEFAULT);
        node
    }

    /// The node `decl` describes (what `tutti_mod::compile` gives each
    /// note envelope): `None` when `decl` is not a note envelope. The rate
    /// comes at `prepare`.
    pub fn from_decl(decl: &SourceNode) -> Option<Self> {
        match decl.kind {
            SignalKind::NoteEnvelope { adsr } => Some(Self::new(decl.id, adsr)),
            SignalKind::Follower { .. } => None,
        }
    }

    /// The source id its segments are addressed to.
    pub fn source(&self) -> SourceId {
        SourceId(self.source)
    }

    fn lengths(&mut self, rate: SampleRate) {
        self.attack = self.adsr.attack.to_samples(rate);
        self.decay = self.adsr.decay.to_samples(rate);
        self.release = self.adsr.release.to_samples(rate);
        let s = self.adsr.sustain.get();
        // A NaN sustain is 0: the envelope never sends a NaN.
        self.sustain = if s >= 0.0 { s.min(1.0) } else { 0.0 };
    }

    /// The level at frame `at`.
    fn level_at(&self, at: Frame) -> f32 {
        self.level.map_or(0.0, |(seg, sent)| {
            seg.raw_value_after(at.since(sent).unwrap_or(Samples(0)))
        })
    }

    fn segment(&self, from: f32, to: f32, length: Samples) -> ParamSegment {
        ParamSegment::foreign(self.source, from, to, Samples(0), SegmentShape::Linear)
            .resumed(SamplePosition(0.0), SamplePosition(length.get() as f64))
    }

    /// Enters `stage` on frame `at` from level `from`, through any
    /// zero-length stage, and sends its segment.
    fn enter(&mut self, out: &mut Out<'_, '_>, mut stage: Stage, at: Frame, mut from: f32) {
        loop {
            let (to, length, next) = match stage {
                Stage::Attack => (1.0, self.attack, Stage::Decay),
                Stage::Decay => (self.sustain, self.decay, Stage::Sustain),
                Stage::Release => (0.0, self.release, Stage::Idle),
                Stage::Sustain | Stage::Idle => unreachable!("entered only as a boundary"),
            };
            if length.get() == 0 {
                // Skipped on its frame: the next stage starts at its end.
                from = to;
                stage = next;
                if matches!(stage, Stage::Sustain | Stage::Idle) {
                    self.send(out, at, self.segment(to, to, Samples(0)));
                    self.stage = stage;
                    self.boundary = None;
                    return;
                }
                continue;
            }
            self.send(out, at, self.segment(from, to, length));
            self.stage = stage;
            self.boundary = Some(at + length);
            return;
        }
    }

    /// The current stage ends on frame `at`.
    fn cross(&mut self, out: &mut Out<'_, '_>, at: Frame) {
        self.boundary = None;
        match self.stage {
            Stage::Attack => self.enter(out, Stage::Decay, at, 1.0),
            // The segment in flight has arrived and holds its end.
            Stage::Decay => self.stage = Stage::Sustain,
            Stage::Release => self.stage = Stage::Idle,
            Stage::Idle | Stage::Sustain => {}
        }
    }

    fn on_note(&mut self, out: &mut Out<'_, '_>, at: Frame, on: bool, key: usize) {
        let (word, bit) = (key / 64, 1u64 << (key % 64));
        let was = self.held[word] & bit != 0;
        if on {
            if !was {
                self.held[word] |= bit;
                self.holding += 1;
            }
            let from = self.level_at(at);
            self.enter(out, Stage::Attack, at, from);
        } else if was {
            self.held[word] &= !bit;
            self.holding -= 1;
            if self.holding == 0 && self.stage != Stage::Release && self.stage != Stage::Idle {
                let from = self.level_at(at);
                self.enter(out, Stage::Release, at, from);
            }
        }
    }

    /// Idle at 0 on frame `at`, every note forgotten: a play start.
    fn idle(&mut self, out: &mut Out<'_, '_>, at: Frame) {
        if self.holding > 0 {
            self.held = [0; NOTE_WORDS];
            self.holding = 0;
        }
        self.boundary = None;
        if self.stage != Stage::Idle {
            self.send(out, at, self.segment(0.0, 0.0, Samples(0)));
            self.stage = Stage::Idle;
        }
    }

    /// Re-sends the level on frame `at`, as the segment in flight resumed
    /// there (the same trajectory, so a lane already reading it hears no
    /// change): after a refused segment, and on an [`EventKind::Resync`].
    /// Before its first segment it stands idle at 0 and sends that: a lane
    /// may hold the last value of a node this one replaced.
    fn resend(&mut self, out: &mut Out<'_, '_>, at: Frame) {
        let seg = match self.level {
            Some((seg, sent)) => {
                let along = at.since(sent).map_or(0.0, |s| s.get() as f64);
                seg.resumed(seg.position() + SamplePosition(along), seg.length())
            }
            None => self.segment(0.0, 0.0, Samples(0)),
        };
        self.send(out, at, seg);
    }

    /// Sends `seg` on frame `at` (one per frame: a later one on the same
    /// frame replaces it), and makes it the level.
    fn send(&mut self, out: &mut Out<'_, '_>, at: Frame, seg: ParamSegment) {
        self.level = Some((seg, at));
        let i = at.since(out.base).map_or(0, Samples::get);
        out.put(i, seg);
        if out.refused {
            self.owed = true;
        }
    }
}

/// A block's output: the segment waiting on its frame, so a later one on
/// the same frame replaces it, written once the walk moves past it.
struct Out<'w, 'a> {
    writer: &'w mut tutti_graph::EventWriter<'a>,
    base: Frame,
    len: Samples,
    pending: Option<(usize, ParamSegment)>,
    refused: bool,
}

impl Out<'_, '_> {
    fn put(&mut self, i: usize, seg: ParamSegment) {
        if self.pending.is_some_and(|(p, _)| p != i) {
            self.flush();
        }
        self.pending = Some((i, seg));
    }

    fn flush(&mut self) {
        if let Some((i, seg)) = self.pending.take() {
            let pushed = Offset::new(i, self.len)
                .is_some_and(|at| self.writer.push(Event::segment(at, seg)).is_ok());
            self.refused |= !pushed;
        }
    }
}

impl Node for NoteEnvelopeNode {
    /// No audio; one event input (MIDI), one event output (the segments).
    ///
    /// `Light`: it does work only on a note, a stage boundary or a play
    /// start, a few comparisons and one segment each.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(1, 1)
            .with_event_capacity(NOTE_ENVELOPE_EVENT_CAPACITY)
            .with_cost(Cost::Light)
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.lengths(prepare.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let len = io.frames();
        if len == 0 {
            return Status::Modified;
        }
        let base = cx.env.frame;
        let walk = PlayWalk::new(cx, len);
        let events = io.events(0).as_slice();
        let mut out = Out {
            writer: io.event_out(0),
            base,
            len: cx.env.block_len,
            pending: None,
            refused: false,
        };
        if std::mem::take(&mut self.owed) {
            // Refused last block: where it stands now, as a resumed segment.
            self.resend(&mut out, base);
        }
        let mut starts = walk.starts(cx, len, self.epoch).peekable();
        let (mut ev, mut from) = (0usize, 0usize);
        loop {
            let start = starts.peek().copied();
            let boundary = self
                .boundary
                .map(|b| (b.get().saturating_sub(base.get()) as usize).max(from))
                .filter(|&i| i < len);
            let note_at = events.get(ev).map(|e| e.offset.index());
            let Some(p) = [start, boundary, note_at].into_iter().flatten().min() else {
                break;
            };
            let at = base + Samples(p);
            if start == Some(p) {
                starts.next();
                self.idle(&mut out, at);
            }
            if self.boundary.is_some_and(|b| b <= at) {
                self.cross(&mut out, at);
            }
            while let Some(e) = events.get(ev).filter(|e| e.offset.index() == p) {
                if let Some((on, key)) = note(e) {
                    self.on_note(&mut out, at, on, key);
                } else if e.kind == EventKind::Resync {
                    // A lane newly reads it: where it stands, resumed.
                    self.resend(&mut out, at);
                }
                ev += 1;
            }
            from = p + 1;
        }
        out.flush();
        self.owed |= out.refused;
        self.epoch = Some(walk.at(cx, len - 1).epoch);
        Status::Modified
    }

    fn reset(&mut self) {
        self.held = [0; NOTE_WORDS];
        self.holding = 0;
        self.stage = Stage::Idle;
        self.level = None;
        self.boundary = None;
        self.epoch = None;
        self.owed = false;
    }
}
