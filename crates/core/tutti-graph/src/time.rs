//! Time inside a block: [`Offset`], and [`Due`], what a scheduled time means
//! for the block being rendered.
//!
//! Doc 013 §6, "In the type system", item 1. There are two kinds of time in a
//! graph and they must not mix:
//!
//! - a **[`Frame`]** (`tutti-types`) is an absolute position on the
//!   executor's clock — [`Env::frame`](crate::Env::frame) is the one at the
//!   start of the block;
//! - an **[`Offset`]** is a position *inside the current block*, and means
//!   nothing outside it.
//!
//! The off-by-a-block bug is handing one where the other was meant: an event
//! stamped with an absolute frame lands (or is refused) wherever that number
//! happens to fall, and an offset treated as a frame schedules something at
//! the start of the session. Both are separate types, and the only
//! conversions are through the block's [`Env`](crate::Env):
//! [`offset_of`](crate::Env::offset_of) (checked — a frame outside the block
//! has no offset) and [`frame_at`](crate::Env::frame_at).
//!
//! An `Offset` carries no block identity, so one taken from a long block can
//! still be too large for the next, shorter one: the places that accept
//! events ([`EventWriter`](crate::EventWriter),
//! [`SortedEvents::new`](crate::SortedEvents::new)) keep checking against the
//! block they are in. What the type removes is the *kind* confusion, which no
//! runtime check can see because both are just numbers.
//!
//! The `compile_fail` example on [`Offset`] is mutation-checked: giving
//! `Event::midi` an `impl Into<Offset>` parameter and adding
//! `impl From<Frame> for Offset` makes it compile, and the doctest fails.

use tutti_types::{At, Beat, Frame, Latency, Samples};

use crate::node::{Env, Transport, TransportChanges};

/// A position inside the current block: frames from its first frame.
///
/// Created only checked against a block length —
/// [`Offset::new`], [`Env::offset`](crate::Env::offset),
/// [`Env::offset_of`](crate::Env::offset_of) or
/// [`Io::offset`](crate::Io::offset) — or as [`Offset::ZERO`], which every
/// block has. There is no conversion from a [`Frame`], so an absolute frame
/// cannot be used as one:
///
/// ```compile_fail
/// use tutti_graph::{Env, Event};
/// fn note_on_now(env: &Env) -> Event {
///     Event::midi(env.frame, [0x2090_3c64, 0, 0, 0]) // a Frame is not an Offset
/// }
/// ```
///
/// The conversion is through the block's environment, and it is checked:
///
/// ```
/// use tutti_graph::{Env, Event, Offset};
/// fn note_on_at(env: &Env, when: tutti_types::Frame) -> Option<Event> {
///     let at: Offset = env.offset_of(when)?; // `None` outside this block
///     Some(Event::midi(at, [0x2090_3c64, 0, 0, 0]))
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Offset(u32);

impl Offset {
    /// The block's first frame — valid in every block, since a block holds at
    /// least one frame.
    pub const ZERO: Offset = Offset(0);

    /// Frame `index` of a block `block_len` long, or `None` when it is past
    /// the end.
    pub fn new(index: usize, block_len: Samples) -> Option<Offset> {
        if index < block_len.get() {
            u32::try_from(index).ok().map(Offset)
        } else {
            None
        }
    }

    /// The offset as a count of frames from the block's start.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The offset as a slice index into the block's buffers.
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// An offset not (yet) checked against a block: the relative spacing of
    /// flushed events, before they are clamped into the block that delivers
    /// them ([`clamp_to`](Self::clamp_to)), and positions the crate's own
    /// executors computed inside a block they know the length of.
    pub(crate) const fn raw(v: u32) -> Offset {
        Offset(v)
    }

    /// This offset, or the block's last frame when it is past the end.
    pub(crate) fn clamp_to(self, frames: usize) -> Offset {
        debug_assert!(frames > 0, "a block holds at least one frame");
        Offset(self.0.min(frames.saturating_sub(1) as u32))
    }
}

impl std::fmt::Display for Offset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// What a scheduled time ([`At`]) means for the block being rendered — see
/// [`Env::due`](crate::Env::due).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// It falls inside this block, at this offset.
    In(Offset),
    /// It is already past. Whoever resolves it delivers it at
    /// [`Offset::ZERO`] of this block and counts it as late — a late command
    /// is never dropped. For a frame: the frame is before the block. For a
    /// beat: continuous playback crossed it before this block (see
    /// [`Playhead`]).
    Late,
    /// It falls after this block — or, for a beat, the playhead has not
    /// reached it by continuous playback (stopped, not there yet, or a seek
    /// or loop jumped over it).
    NotYet,
}

/// Which beats the transport has **crossed by continuous playback** — the
/// history a beat-timed command needs to tell "late" from "jumped over".
///
/// The rule it implements (doc 013 §6): an `At::Beat(b)` fires when the
/// playhead reaches or crosses `b` through continuous playback, and a loop
/// wrap that lands at or after `b` counts as reaching it. A seek, or a loop
/// that jumps *over* `b`, does not fire it: it stays pending until the
/// playhead reaches it, or it is cancelled. It is late only when continuous
/// playback crossed it before the command was resolved.
///
/// Fed one [`Env`] per block with [`observe`](Self::observe). A block
/// continues the previous one when the loop is the same and its start beat
/// is where the previous block could have arrived: advanced, from the
/// previous block's start, by **between** its length at the previous block's
/// tempo and its length at this block's tempo — **± one frame** — and
/// wrapped by the loop (unmoved when stopped). The host reports one tempo
/// per block, so a tempo that changed inside the previous block — a step at
/// any offset, or any monotonic ramp — moved the playhead by an amount in
/// that range; and a frame of slack absorbs a host's `f32` beat (a step of
/// ~8e-6 beat at beat 100). Anything else — a seek, a loop change, a first
/// block — starts a new run, and a new run has crossed nothing yet.
///
/// **What counts as crossed.** Without a loop wrap, `[anchor, now)`: where
/// the run started, up to the playhead. Once the run has wrapped its loop,
/// `[min(anchor, loop start), now)` — the part of the loop *ahead* of the
/// playhead is not crossed, even though an earlier pass went through it,
/// because this pass will reach it again: a beat there waits for that,
/// rather than firing now as late.
#[derive(Clone, Copy, Debug, Default)]
pub struct Playhead {
    prev: Option<Env>,
    /// Where the current continuous run started.
    anchor: Option<f64>,
    /// Whether the run has wrapped its loop at least once.
    wrapped: bool,
}

/// Continuity slack when no tempo is moving to measure a frame by, in beats.
const STILL: f64 = 1e-9;

impl Playhead {
    /// A playhead with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the block about to be rendered.
    ///
    /// A block with [transport changes](Env::changes) is recorded segment by
    /// segment, as if each were a block of its own: a start or a tempo change
    /// continues the run, a seek begins a new one at its target. So "crossed"
    /// after a mid-block seek means crossed since the seek.
    pub fn observe(&mut self, env: &Env) {
        for (_, segment) in env.segments() {
            self.observe_segment(&segment);
        }
    }

    fn observe_segment(&mut self, env: &Env) {
        let now = env.transport.beat.get();
        let continues = self.prev.and_then(|p| {
            if p.transport.looping != env.transport.looping {
                return None;
            }
            p.arrives_at(now, env.transport.tempo.get())
        });
        match continues {
            Some(wrapped) => self.wrapped |= wrapped,
            None => {
                self.anchor = Some(now);
                self.wrapped = false;
            }
        }
        self.prev = Some(*env);
    }

    /// Whether continuous playback crossed `beat` before the block last
    /// observed.
    pub fn crossed(&self, beat: f64) -> bool {
        let (Some(anchor), Some(env)) = (self.anchor, self.prev) else {
            return false;
        };
        let now = env.transport.beat.get();
        match env.transport.looping {
            Some(l) if self.wrapped => beat >= anchor.min(l.start.get()) && beat < now,
            _ => beat >= anchor && beat < now,
        }
    }
}

impl Env {
    /// The block's end: the first frame after it.
    pub fn end(&self) -> Frame {
        self.frame + self.block_len
    }

    /// Frame `index` of this block, when it is inside it.
    pub fn offset(&self, index: usize) -> Option<Offset> {
        Offset::new(index, self.block_len)
    }

    /// Every offset of this block, in order — for a node that walks its block
    /// frame by frame and stamps what it emits.
    ///
    /// Borrows nothing, so it can drive a loop that writes through the `Io`.
    pub fn offsets(&self) -> impl Iterator<Item = Offset> + 'static {
        // A block is at most `MaxBlock` long, which `Io` holds to a slice
        // length; `u32` covers any block a node can be handed.
        (0..self.block_len.get() as u32).map(Offset)
    }

    /// Where absolute frame `frame` falls in this block, or `None` when it is
    /// before the block's first frame or at or after its end.
    pub fn offset_of(&self, frame: Frame) -> Option<Offset> {
        Offset::new(frame.since(self.frame)?.get(), self.block_len)
    }

    /// The absolute frame at `offset` of this block.
    pub fn frame_at(&self, offset: Offset) -> Frame {
        self.frame + Samples(offset.index())
    }

    /// Resolve a scheduled time against this block.
    ///
    /// - [`At::NextBlock`] is this block's first frame.
    /// - [`At::Frame`] is exact: [`Due::In`] its offset when it is inside
    ///   the block, [`Due::Late`] when it is before it.
    /// - [`At::Beat`] is resolved against this block's transport snapshot:
    ///   [`Due::In`] the **first frame at or after** the beat when playback
    ///   reaches it inside this block — including after a loop wrap in the
    ///   block (rounded up — a frame that starts before the beat has not
    ///   reached it — within a millionth of a frame, so float rounding cannot
    ///   push a beat that is exactly on a frame to the next one) — and
    ///   [`Due::NotYet`] otherwise: stopped, not reached, or behind the
    ///   playhead. Whether a beat behind the playhead is *late* depends on
    ///   how the playhead got past it, which one block cannot say; that is
    ///   [`Playhead::crossed`].
    pub fn due(&self, at: At) -> Due {
        match at {
            At::NextBlock => Due::In(Offset::ZERO),
            // Compared as `u64`, not through `Frame::since`: `since` returns
            // `None` both for "before" and for a distance too far to be a
            // `Samples` on a 32-bit target, and only the first is late.
            At::Frame(f) if f < self.frame => Due::Late,
            At::Frame(f) => {
                let d = f.get() - self.frame.get();
                if d < self.block_len.get() as u64 {
                    Due::In(Offset(d as u32))
                } else {
                    Due::NotYet
                }
            }
            // Segment by segment: the transport a beat is resolved against is
            // the one in force where playback reaches it, so a beat just
            // after a mid-block start lands inside this block.
            At::Beat(b) => self
                .segments()
                .find_map(|(start, segment)| match segment.beat_due(b.get()) {
                    Due::In(k) => Some(Due::In(Offset(start.0 + k.0))),
                    _ => None,
                })
                .unwrap_or(Due::NotYet),
        }
    }

    /// The block cut at its [transport changes](Self::changes): each piece's
    /// first offset, and the piece as an `Env` of its own (its `frame`,
    /// length and transport, with no changes). A block with no change is one
    /// piece, the block itself. Pieces are never empty; a change at or past
    /// the block's end cuts nothing.
    pub fn segments(&self) -> impl Iterator<Item = (Offset, Env)> + '_ {
        let changes = self.changes.as_slice();
        let len = self.block_len.get();
        (0..=changes.len()).filter_map(move |i| {
            let start = if i == 0 {
                0
            } else {
                changes[i - 1].at.index().min(len)
            };
            let end = changes.get(i).map_or(len, |c| c.at.index().min(len));
            let transport = if i == 0 {
                self.transport
            } else {
                changes[i - 1].to
            };
            (end > start).then(|| {
                (
                    Offset(start as u32),
                    Env {
                        frame: self.frame + Samples(start),
                        sample_rate: self.sample_rate,
                        block_len: Samples(end - start),
                        transport,
                        changes: TransportChanges::NONE,
                    },
                )
            })
        })
    }

    /// The transport at frame `offset` of this block: the change in force
    /// there (or the block's own transport), with its beat advanced to
    /// `offset` at its tempo while it rolls, wrapping at its loop.
    ///
    /// Closed form, in `f64`: a host that accumulates its beat frame by frame
    /// (tutti-core's `TransportClock`) agrees to rounding, not to the bit. The
    /// block-start beat and every change's beat are the host's own figures.
    pub fn transport_at(&self, offset: Offset) -> Transport {
        let (start, mut t) = self
            .changes
            .as_slice()
            .iter()
            .rev()
            .find(|c| c.at <= offset)
            .map_or((0, self.transport), |c| (c.at.index(), c.to));
        let Some(fpb) = self.frames_per_beat_at(t.tempo.get()).filter(|_| t.playing) else {
            return t;
        };
        let pos = t.beat.get() + (offset.index() - start) as f64 / fpb;
        t.beat = Beat(match t.looping {
            Some(l) if t.beat.get() < l.end.get() && l.start.get() < l.end.get() => {
                let (ls, le) = (l.start.get(), l.end.get());
                if pos < le {
                    pos
                } else {
                    ls + (pos - ls).rem_euclid(le - ls)
                }
            }
            _ => pos,
        });
        t
    }

    /// Whether this block, with its tempo changing to `next_tempo` at some
    /// point inside it, could have left the transport at beat `next` — and
    /// if so, whether it wrapped its loop on the way. See [`Playhead`] for
    /// the rule. No allocation: the next block's start is unwrapped against
    /// the loop candidate by candidate.
    fn arrives_at(&self, next: f64, next_tempo: f64) -> Option<bool> {
        let t = &self.transport;
        let from = t.beat.get();
        let len = self.block_len.get() as f64;
        let rolls = |tempo: f64| self.frames_per_beat_at(tempo).filter(|_| t.playing);
        let (a, b) = (rolls(t.tempo.get()), rolls(next_tempo));
        // The distance range, in beats: nothing when stopped.
        let (lo, hi) = match (a, b) {
            (None, None) => (0.0, 0.0),
            (Some(f), None) | (None, Some(f)) => (len / f, len / f),
            (Some(f), Some(g)) => ((len / f).min(len / g), (len / f).max(len / g)),
        };
        // One frame of slack, in beats, at the faster of the two tempos (a
        // frame is fewer beats at the slower), whether or not it rolls.
        let slack = [t.tempo.get(), next_tempo]
            .into_iter()
            .filter_map(|tempo| self.frames_per_beat_at(tempo))
            .map(|fpb| 1.0 / fpb)
            .fold(STILL, f64::max);
        let fits = |x: f64| x - from >= lo - slack && x - from <= hi + slack;
        if fits(next) {
            return Some(false);
        }
        match t.looping {
            // A wrap leaves the playhead inside the loop: a `next` outside
            // it was reached some other way (a seek), however the distances
            // happen to line up.
            Some(l)
                if from < l.end.get()
                    && l.start.get() < l.end.get()
                    && next >= l.start.get() - slack
                    && next < l.end.get() + slack =>
            {
                let (start, end) = (l.start.get(), l.end.get());
                let period = end - start;
                // Wrapped k + 1 times: unwrapped, `next` sat at
                // `end + (next - start) + k * period`.
                let mut x = end + (next - start);
                while x - from <= hi + slack {
                    if fits(x) {
                        return Some(true);
                    }
                    x += period;
                }
                None
            }
            _ => None,
        }
    }

    /// Frames per beat while the transport rolls, `None` when it does not.
    fn frames_per_beat(&self) -> Option<f64> {
        self.frames_per_beat_at(self.transport.tempo.get())
            .filter(|_| self.transport.playing)
    }

    /// Frames per beat at `tempo` and this block's rate, when both are
    /// usable, whether or not the transport rolls.
    fn frames_per_beat_at(&self, tempo: f64) -> Option<f64> {
        let rate = self.sample_rate.get();
        // `is_finite` and `> 0.0` together also refuse a NaN, which no
        // comparison would.
        let usable = tempo.is_finite() && tempo > 0.0 && rate.is_finite() && rate > 0.0;
        // Raw `f64` beats and frames, on purpose: `BeatDuration::to_seconds`
        // lands in `Seconds`, which is `f32` and cannot resolve a frame an
        // hour into a session (CLAUDE.md, "where the types stop").
        usable.then(|| rate * 60.0 / tempo)
    }

    /// Where a scheduled command lands in the block of a sink whose inputs
    /// arrive `arrival` late — the PDC rule for commands.
    ///
    /// An `At::Frame(F)` means **timeline** frame `F`. A sink with compiled
    /// arrival latency `a` receives the audio of timeline frame `F` at its own
    /// frame `F + a`, so the command lands there too; landing at `F` would be
    /// early against the node's own audio by exactly `a`. An `At::Beat` is
    /// resolved against the transport to its timeline frame first (and
    /// rewritten in place to that `At::Frame`, since the landing may fall in
    /// a later block), then shifted the same way. `At::NextBlock` names no
    /// timeline position to align with: it lands at the start of the block,
    /// uncompensated.
    ///
    /// A beat is late when `playhead` (already fed this block) says
    /// continuous playback crossed it before this block.
    pub(crate) fn due_at_arrival(&self, at: &mut At, arrival: Latency, playhead: &Playhead) -> Due {
        match *at {
            At::NextBlock => Due::In(Offset::ZERO),
            At::Frame(f) => self.due(At::Frame(f + arrival.samples())),
            At::Beat(b) => match self.due(*at) {
                Due::In(k) => {
                    *at = At::Frame(self.frame_at(k));
                    self.due_at_arrival(at, arrival, playhead)
                }
                _ if playhead.crossed(b.get()) => Due::Late,
                other => other,
            },
        }
    }

    fn beat_due(&self, beat: f64) -> Due {
        let Some(frames_per_beat) = self.frames_per_beat() else {
            return Due::NotYet;
        };
        let now = self.transport.beat.get();
        // A beat behind the playhead by less than a frame (minus the
        // tolerance) falls due on this block's first frame. It is the exact
        // complement of the ahead side: the previous block resolved a beat
        // `d` frames before its end to offset `ceil(len - d - TOLERANCE)`,
        // which is inside that block only when `d >= 1 - TOLERANCE`. So a
        // beat between a block's last frame and its end (or a rounding error
        // behind an accumulated playhead) is due here, not late, and every
        // beat lands exactly once.
        let beat = if beat < now && (now - beat) * frames_per_beat < 1.0 - TOLERANCE {
            now
        } else {
            beat
        };
        let ahead = match self.transport.looping {
            Some(l) if now < l.end.get() && l.start.get() < l.end.get() => {
                let (start, end) = (l.start.get(), l.end.get());
                if beat >= now && beat < end {
                    beat - now
                } else if beat >= start && beat < now {
                    // Behind the playhead, inside the loop: reached again
                    // after the wrap — if that falls in this block.
                    (end - now) + (beat - start)
                } else {
                    // Past the loop's end (never reached while the loop
                    // holds), or behind both loop and playhead.
                    return Due::NotYet;
                }
            }
            _ if beat >= now => beat - now,
            _ => return Due::NotYet,
        };
        // Up to `f64` rounding in the product, a beat exactly on a frame
        // boundary lands on that frame rather than the next: a transport
        // position is itself an accumulated `f64`, and without the tolerance
        // a beat computed as `frame / frames_per_beat` could miss its own
        // frame by one. A millionth of a frame is far below anything musical.
        const TOLERANCE: f64 = 1e-6;
        let k = (ahead * frames_per_beat - TOLERANCE).ceil().max(0.0);
        if k < self.block_len.get() as f64 {
            Due::In(Offset(k as u32))
        } else {
            Due::NotYet
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{LoopRange, Transport};
    use tutti_types::{Beat, Bpm, Latency, SampleRate};

    fn env(frame: u64, len: usize, transport: Transport) -> Env {
        Env {
            frame: Frame(frame),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(len),
            transport,
            changes: crate::node::TransportChanges::NONE,
        }
    }

    /// An offset exists only inside its block.
    ///
    /// Mutation: `index <= block_len` in `Offset::new` → the one-past-the-end
    /// offset is accepted → fails.
    #[test]
    fn offsets_are_checked_against_the_block() {
        assert_eq!(Offset::new(0, Samples(1)), Some(Offset::ZERO));
        assert_eq!(Offset::new(63, Samples(64)).map(Offset::get), Some(63));
        assert_eq!(Offset::new(64, Samples(64)), None);
        assert_eq!(Offset::raw(90).clamp_to(64), Offset::raw(63));
        assert_eq!(Offset::raw(9).clamp_to(64), Offset::raw(9));
    }

    /// `offset_of` and `frame_at` are inverses inside the block, and a frame
    /// on either side of it has no offset.
    ///
    /// Mutation: in `offset_of`, drop the block-length check (build the
    /// offset from the distance alone) → the frame at the block's end gets
    /// an offset → fails.
    #[test]
    fn frames_and_offsets_convert_only_through_the_block() {
        let e = env(1000, 64, Transport::default());
        assert_eq!(e.offset_of(Frame(999)), None);
        assert_eq!(e.offset_of(Frame(1000)), Some(Offset::ZERO));
        assert_eq!(e.offset_of(Frame(1063)).map(Offset::get), Some(63));
        assert_eq!(e.offset_of(Frame(1064)), None);
        for o in e.offsets() {
            assert_eq!(e.offset_of(e.frame_at(o)), Some(o));
        }
        assert_eq!(e.offsets().count(), 64);
        assert_eq!(e.end(), Frame(1064));
    }

    /// Frame times: inside, before and after the block.
    ///
    /// Mutation: map a past frame to `NotYet` instead of `Late` → fails.
    #[test]
    fn a_frame_is_due_exactly_late_or_not_yet() {
        let e = env(1000, 64, Transport::default());
        assert_eq!(e.due(At::Frame(Frame(1010))), Due::In(Offset::raw(10)));
        assert_eq!(e.due(At::Frame(Frame(999))), Due::Late);
        assert_eq!(e.due(At::Frame(Frame(1064))), Due::NotYet);
        assert_eq!(e.due(At::NextBlock), Due::In(Offset::ZERO));
    }

    /// Beats resolve against the transport: at 120 BPM and 48 kHz a beat is
    /// 24 000 frames, so beat 1.5 is 36 000 frames after beat 0.
    ///
    /// Mutation: round down (`floor`) instead of up → the beat a quarter
    /// frame in lands one frame early → fails. Mutation: ignore `playing`
    /// → the stopped case resolves → fails.
    #[test]
    fn a_beat_is_resolved_against_the_transport() {
        let rolling = |beat: f64| Transport {
            playing: true,
            tempo: Bpm(120.0),
            beat: Beat(beat),
            looping: None,
        };
        // The block starting at frame 35 990 starts at beat 35 990 / 24 000.
        let e = env(35_990, 64, rolling(35_990.0 / 24_000.0));
        assert_eq!(e.due(At::Beat(Beat(1.5))), Due::In(Offset::raw(10)));
        // A beat a quarter frame into frame 10 is first reached at frame 11.
        let quarter = (36_000.25 - 0.0) / 24_000.0;
        assert_eq!(e.due(At::Beat(Beat(quarter))), Due::In(Offset::raw(11)));
        // Behind the playhead: one block cannot call it late (that needs
        // the `Playhead`'s history).
        assert_eq!(e.due(At::Beat(Beat(1.0))), Due::NotYet);
        assert_eq!(e.due(At::Beat(Beat(2.0))), Due::NotYet);
        let stopped = env(
            35_990,
            64,
            Transport {
                playing: false,
                ..rolling(35_990.0 / 24_000.0)
            },
        );
        assert_eq!(stopped.due(At::Beat(Beat(1.5))), Due::NotYet);
    }

    /// PDC for commands: a frame lands `arrival` later; a beat is resolved to
    /// its timeline frame in the block it falls in, rewritten to that frame,
    /// and lands `arrival` later — here in the next block. `NextBlock` is not
    /// shifted.
    ///
    /// Mutation: in `due_at_arrival`, return the beat's own offset without
    /// the shift → lands in this block at 10 → fails.
    #[test]
    fn a_command_lands_its_arrival_after_its_timeline_time() {
        let a = Latency::new(Samples(60));
        let ph = Playhead::new();
        let e = env(1000, 64, Transport::default());
        let mut at = At::Frame(Frame(1000));
        assert_eq!(e.due_at_arrival(&mut at, a, &ph), Due::In(Offset::raw(60)));
        let mut next = At::NextBlock;
        assert_eq!(e.due_at_arrival(&mut next, a, &ph), Due::In(Offset::ZERO));
        let t = Transport {
            playing: true,
            tempo: Bpm(120.0),
            beat: Beat(1000.0 / 24_000.0),
            looping: None,
        };
        let e = env(1000, 64, t);
        let mut beat = At::Beat(Beat(1010.0 / 24_000.0));
        assert_eq!(
            e.due_at_arrival(&mut beat, a, &ph),
            Due::NotYet,
            "1070 is next block"
        );
        assert_eq!(
            beat,
            At::Frame(Frame(1010)),
            "resolved while it fell in this block"
        );
        let next_block = env(1064, 64, t);
        assert_eq!(
            next_block.due_at_arrival(&mut beat, a, &ph),
            Due::In(Offset::raw(6))
        );
    }

    /// Looping: a passed beat inside the loop comes round again after the
    /// wrap; one past the loop end waits; one before loop and playhead waits
    /// too (only the `Playhead` can call it late).
    ///
    /// Mutation: drop the wrap branch (treat every beat behind the playhead
    /// as not yet) → the wrapped beat is `NotYet` → fails.
    #[test]
    fn a_looping_transport_reaches_a_passed_beat_after_the_wrap() {
        // 120 BPM at 48 kHz: 24 000 frames a beat, so a 64-frame block
        // covers 1/375 of a beat. Loop [4, 8); the block starts 10 frames
        // before the end.
        let spb = 1.0 / 24_000.0;
        let t = Transport {
            playing: true,
            tempo: Bpm(120.0),
            beat: Beat(8.0 - 10.0 * spb),
            looping: Some(LoopRange {
                start: Beat(4.0),
                end: Beat(8.0),
            }),
        };
        let e = env(0, 64, t);
        // Beat 4 plus 5 frames: 10 frames to the wrap, then 5 more.
        assert_eq!(
            e.due(At::Beat(Beat(4.0 + 5.0 * spb))),
            Due::In(Offset::raw(15))
        );
        assert_eq!(e.due(At::Beat(Beat(9.0))), Due::NotYet);
        assert_eq!(e.due(At::Beat(Beat(2.0))), Due::NotYet);
    }

    /// A far-future frame is not late, even where the distance does not fit
    /// a `Samples`.
    ///
    /// Mutation: resolve frames through `Frame::since` (`None` → `Late`)
    /// → on a 32-bit target the far frame reads as late. On a 64-bit host
    /// the distance fits, so this checks the before/after split directly.
    #[test]
    fn a_far_frame_is_not_late() {
        let e = env(1000, 64, Transport::default());
        assert_eq!(e.due(At::Frame(Frame(u64::MAX))), Due::NotYet);
        assert_eq!(e.due(At::Frame(Frame(999))), Due::Late);
    }

    /// Blocks at 120 BPM, 48 kHz, each `len` frames, starting at `beat`.
    fn block(beat: f64, len: usize, playing: bool, looping: Option<LoopRange>) -> Env {
        env(
            0,
            len,
            Transport {
                playing,
                tempo: Bpm(120.0),
                beat: Beat(beat),
                looping,
            },
        )
    }

    /// The playhead's crossed set: continuous playback crosses, a seek
    /// starts over, a stop keeps the run, and a wrap crosses the loop.
    ///
    /// Mutation: in `Playhead::observe`, never extend a run (always reset)
    /// → nothing is ever crossed → fails. Mutation: ignore `wrapped` in
    /// `crossed` → the beat inside the loop behind the anchor is not
    /// crossed after the wrap → fails.
    #[test]
    fn the_playhead_crosses_only_by_continuous_playback() {
        let mut ph = Playhead::new();
        ph.observe(&block(1.0, 2400, true, None));
        assert!(!ph.crossed(1.0), "a new run has crossed nothing");
        ph.observe(&block(1.1, 2400, true, None));
        assert!(ph.crossed(1.05) && !ph.crossed(1.1) && !ph.crossed(0.9));
        // Stopped: no motion, the run holds.
        ph.observe(&block(1.2, 64, false, None));
        ph.observe(&block(1.2, 64, true, None));
        assert!(ph.crossed(1.15));
        // A seek forward: a new run; what it jumped over is not crossed.
        ph.observe(&block(3.0, 64, true, None));
        assert!(!ph.crossed(2.0) && !ph.crossed(1.15));
        // Looping [4, 8): start at 3.5, before the loop, in half-beat
        // blocks; play through the loop, wrap, and on to 4.5.
        let l = Some(LoopRange {
            start: Beat(4.0),
            end: Beat(8.0),
        });
        let mut ph = Playhead::new();
        for b in [3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5] {
            ph.observe(&block(b, 12_000, true, l));
        }
        assert!(ph.crossed(7.2) && !ph.crossed(7.7), "before the wrap");
        ph.observe(&block(4.0, 12_000, true, l));
        ph.observe(&block(4.5, 12_000, true, l));
        assert!(ph.crossed(3.7), "before the loop: never reached again");
        assert!(ph.crossed(4.2), "this pass, behind the playhead");
        assert!(
            !ph.crossed(4.6) && !ph.crossed(7.7),
            "ahead of the playhead: the last pass crossed it, this one will reach it"
        );
        assert!(!ph.crossed(8.5) && !ph.crossed(3.0));
    }

    /// Continuity survives what hosts actually send: a beat position rounded
    /// to `f32` (a step of ~8e-6 beat at beat 100), and a tempo ramped
    /// inside each block (the block-start tempo then misses the next start
    /// by frames, the ramp estimate does not).
    ///
    /// Mutation: continuity within 1e-6 beat (the old rule) → the `f32`
    /// positions break the run and nothing is crossed → fails. Mutation:
    /// accept only the steady advance at the block's own tempo (drop the
    /// next tempo from `arrives_at`'s range) → the ramp and the step break
    /// the run → fails.
    #[test]
    fn continuity_survives_f32_beats_and_tempo_ramps() {
        let at = |beat: f64, tempo: f64| {
            env(
                0,
                512,
                Transport {
                    playing: true,
                    tempo: Bpm(tempo),
                    beat: Beat(beat),
                    looping: None,
                },
            )
        };
        // f32 beats around beat 100 at 120 BPM (512 frames = 0.02133 beat).
        let mut ph = Playhead::new();
        let mut exact = 100.0f64;
        for _ in 0..8 {
            ph.observe(&at(f64::from(exact as f32), 120.0));
            exact += 512.0 / 24_000.0;
        }
        assert!(ph.crossed(100.05), "one run, despite the f32 steps");
        // A ramp from 120 to 180 BPM, 2 BPM per block, linear inside each:
        // each block advances by its average tempo.
        let mut ph = Playhead::new();
        let (mut beat, mut tempo) = (10.0f64, 120.0f64);
        for _ in 0..8 {
            ph.observe(&at(beat, tempo));
            let next = tempo + 2.0;
            beat += 512.0 * 0.5 * (tempo + next) / 60.0 / 48_000.0;
            tempo = next;
        }
        assert!(ph.crossed(10.05), "one run, through the ramp");
        // A step from 120 to 240 BPM at offset 64 of a 256-frame block: the
        // block reports 120, the next 240, and the position moved 64 frames
        // at 120 and 192 at 240. A beat crossed inside it was crossed.
        let mut ph = Playhead::new();
        let e = |beat: f64, tempo: f64| {
            env(
                0,
                256,
                Transport {
                    playing: true,
                    tempo: Bpm(tempo),
                    beat: Beat(beat),
                    looping: None,
                },
            )
        };
        ph.observe(&e(1.0, 120.0));
        let after = 1.0 + 64.0 / 24_000.0 + 192.0 / 12_000.0;
        ph.observe(&e(after, 240.0));
        assert!(ph.crossed(1.01), "the step is not a seek");
        // A real seek still is one.
        ph.observe(&e(after + 0.5, 240.0));
        assert!(!ph.crossed(1.01) && !ph.crossed(after + 0.2));
    }

    /// A seek out of a loop is a seek, even when the distances line up with
    /// some number of wraps. Loop [4, 4.5) at 120 BPM; a 1 200-frame block
    /// (0.05 beat) from 4.45; the next block starts at 3.0, which two whole
    /// loops plus the block's advance would also "explain" arithmetically —
    /// but a wrap leaves the playhead inside the loop, and 3.0 is not.
    ///
    /// Mutation: drop the "inside the loop" guard in `arrives_at` → the seek
    /// reads as continuous (wrapped), the run keeps its old anchor, and the
    /// beat just crossed from 3.0 is not → fails.
    #[test]
    fn a_seek_out_of_a_loop_is_not_a_wrap() {
        let l = Some(LoopRange {
            start: Beat(4.0),
            end: Beat(4.5),
        });
        let at = |beat: f64| block(beat, 1_200, true, l);
        let mut ph = Playhead::new();
        ph.observe(&at(4.45));
        ph.observe(&at(3.0));
        ph.observe(&at(3.05));
        assert!(ph.crossed(3.02), "a new run from 3.0");
        assert!(!ph.crossed(4.46), "not the old one");
    }

    /// Resolution with history: a beat crossed by continuous playback before
    /// the command is seen is late; one jumped over by a seek waits.
    ///
    /// Mutation: in `due_at_arrival`, make every beat behind the playhead
    /// late (ignore `crossed`) → the jumped-over beat is late → fails.
    #[test]
    fn a_beat_is_late_only_when_playback_crossed_it() {
        let mut ph = Playhead::new();
        ph.observe(&block(1.0, 2400, true, None));
        let e = block(1.1, 2400, true, None);
        ph.observe(&e);
        let mut crossed = At::Beat(Beat(1.05));
        assert_eq!(
            e.due_at_arrival(&mut crossed, Latency::ZERO, &ph),
            Due::Late
        );
        let seek = block(3.0, 64, true, None);
        ph.observe(&seek);
        let mut jumped = At::Beat(Beat(2.0));
        assert_eq!(
            seek.due_at_arrival(&mut jumped, Latency::ZERO, &ph),
            Due::NotYet
        );
    }

    fn at(k: usize) -> Offset {
        Offset::new(k, Samples(usize::MAX)).expect("small")
    }

    /// A change list is ordered and distinct; one at the block start or past
    /// the bound is refused; the same offset twice keeps the later transport.
    ///
    /// Mutation: drop the `at < last.at` check → the out-of-order push is
    /// accepted → fails. Push a second entry instead of replacing on an
    /// equal offset → `len` is 2 → fails.
    #[test]
    fn transport_changes_are_ordered_distinct_and_bounded() {
        let playing = Transport {
            playing: true,
            ..Transport::default()
        };
        let mut c = TransportChanges::NONE;
        assert_eq!(
            c.push(Offset::ZERO, playing),
            Err(crate::TransportChangeRejected::AtBlockStart)
        );
        c.push(at(10), Transport::default()).expect("first");
        c.push(at(10), playing).expect("same frame replaces");
        assert_eq!(c.len(), 1);
        assert_eq!(c.as_slice()[0].to, playing, "the later command wins");
        assert_eq!(
            c.push(at(5), playing),
            Err(crate::TransportChangeRejected::OutOfOrder)
        );
        for k in 1..crate::MAX_TRANSPORT_CHANGES {
            c.push(at(10 + k), playing).expect("room");
        }
        assert!(c.is_full());
        assert_eq!(
            c.push(at(1000), playing),
            Err(crate::TransportChangeRejected::Full)
        );
        c.push(
            at(10 + crate::MAX_TRANSPORT_CHANGES - 1),
            Transport::default(),
        )
        .expect("replacing the last needs no room");
    }

    /// `segments` cuts at each change; `transport_at` reads the change in
    /// force and advances it; `due` resolves a beat in the piece that reaches
    /// it.
    ///
    /// Mutation: take the block's own transport in `transport_at` (ignore
    /// changes) → the frame after the start reads stopped → fails. Give a
    /// piece the block's `frame` in `segments` → the frame assertion fails.
    /// Return `k` without adding the piece's start in `due` → fails.
    #[test]
    fn a_start_inside_the_block_is_seen_from_its_frame() {
        // Stopped at beat 2 until frame 100, then rolling from there.
        let mut e = env(
            1_000,
            512,
            Transport {
                beat: Beat(2.0),
                ..Transport::default()
            },
        );
        let rolling = Transport {
            playing: true,
            beat: Beat(2.0),
            ..Transport::default()
        };
        e.changes.push(at(100), rolling).expect("inside");

        let pieces: Vec<(Offset, Env)> = e.segments().collect();
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].0, Offset::ZERO);
        assert_eq!(pieces[0].1.block_len, Samples(100));
        assert_eq!(pieces[1].0, at(100));
        assert_eq!(pieces[1].1.frame, Frame(1_100));
        assert_eq!(pieces[1].1.block_len, Samples(412));

        assert!(!e.transport_at(at(99)).playing);
        assert_eq!(e.transport_at(at(99)).beat, Beat(2.0));
        assert!(e.transport_at(at(100)).playing);
        // 120 BPM at 48 kHz: 24 000 frames a beat.
        assert_eq!(e.transport_at(at(340)).beat, Beat(2.01));

        // Beat 2 is reached on the start's frame; beat 2.01 240 frames later.
        assert_eq!(e.due(At::Beat(Beat(2.0))), Due::In(at(100)));
        assert_eq!(e.due(At::Beat(Beat(2.01))), Due::In(at(340)));
        // Not by the block's own transport alone: it is stopped.
        let mut first = e;
        first.changes = TransportChanges::NONE;
        assert_eq!(first.due(At::Beat(Beat(2.0))), Due::NotYet);
    }

    /// A beat a rounding error behind the playhead is its first frame, not a
    /// crossed beat; one a whole frame behind is not due.
    ///
    /// Mutation: drop the behind-side tolerance in `beat_due` → the first
    /// assertion reads `NotYet` → fails.
    #[test]
    fn a_beat_a_rounding_error_behind_is_the_first_frame() {
        let e = block(1.0 + 1e-12, 64, true, None);
        assert_eq!(e.due(At::Beat(Beat(1.0))), Due::In(Offset::ZERO));
        let e = block(1.0 + 1.0 / 24_000.0, 64, true, None);
        assert_eq!(e.due(At::Beat(Beat(1.0))), Due::NotYet);
    }

    /// A seek inside a rolling block starts a new run: a beat the new run
    /// passed is crossed, one before the seek or jumped over is not.
    ///
    /// Mutation: observe only the block's first piece in `Playhead::observe`
    /// → beat 8.05 (after the seek) is not crossed → fails.
    #[test]
    fn a_seek_inside_the_block_starts_a_new_run() {
        let roll = |beat: f64| Transport {
            playing: true,
            beat: Beat(beat),
            ..Transport::default()
        };
        let mut ph = Playhead::new();
        let mut b = env(0, 4_800, roll(0.0));
        b.changes.push(at(2_400), roll(8.0)).expect("inside");
        ph.observe(&b);
        // The next block continues from the seek: 2 400 frames = 0.1 beat.
        ph.observe(&env(4_800, 4_800, roll(8.1)));
        assert!(ph.crossed(8.05), "crossed after the seek");
        assert!(!ph.crossed(0.05), "before the seek: a different run");
        assert!(!ph.crossed(4.0), "jumped over");
    }
}
