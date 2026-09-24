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

use tutti_types::{At, Frame, Latency, Samples};

use crate::node::Env;

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
/// continues the previous one when its start beat is where the previous
/// block's transport would have arrived (its start, advanced by its length at
/// its tempo and rate, wrapped by its loop; unmoved when stopped), with the
/// same loop. Anything else — a seek, a loop change, a first block — starts a
/// new run, and a new run has crossed nothing yet.
#[derive(Clone, Copy, Debug, Default)]
pub struct Playhead {
    prev: Option<Env>,
    /// Where the current continuous run started.
    anchor: Option<f64>,
    /// Whether the run has wrapped its loop at least once.
    wrapped: bool,
}

/// Continuity tolerance, in beats: far below a frame at any musical tempo
/// (a frame is ~4e-5 of a beat at 120 BPM and 48 kHz), far above the drift
/// of an accumulated `f64` position.
const CONTINUITY: f64 = 1e-6;

impl Playhead {
    /// A playhead with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the block about to be rendered.
    pub fn observe(&mut self, env: &Env) {
        let now = env.transport.beat.get();
        let continues = self.prev.and_then(|p| {
            let (arrives, wrapped) = p.advance();
            ((arrives - now).abs() <= CONTINUITY && p.transport.looping == env.transport.looping)
                .then_some(wrapped)
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
            Some(l) if self.wrapped => beat >= anchor.min(l.start.get()) && beat < l.end.get(),
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
            At::Beat(b) => self.beat_due(b.get()),
        }
    }

    /// Where the transport arrives at the end of this block, and whether it
    /// wrapped its loop on the way.
    fn advance(&self) -> (f64, bool) {
        let t = &self.transport;
        let now = t.beat.get();
        let Some(frames_per_beat) = self.frames_per_beat() else {
            return (now, false);
        };
        let x = now + self.block_len.get() as f64 / frames_per_beat;
        match t.looping {
            Some(l) if now < l.end.get() && l.start.get() < l.end.get() && x >= l.end.get() => {
                let len = l.end.get() - l.start.get();
                (l.start.get() + (x - l.end.get()) % len, true)
            }
            _ => (x, false),
        }
    }

    /// Frames per beat while the transport rolls, `None` when it does not.
    fn frames_per_beat(&self) -> Option<f64> {
        let t = &self.transport;
        let tempo = t.tempo.get();
        let rate = self.sample_rate.get();
        // `is_finite` and `> 0.0` together also refuse a NaN, which no
        // comparison would.
        let moving = tempo.is_finite() && tempo > 0.0 && rate.is_finite() && rate > 0.0;
        // Raw `f64` beats and frames, on purpose: `BeatDuration::to_seconds`
        // lands in `Seconds`, which is `f32` and cannot resolve a frame an
        // hour into a session (CLAUDE.md, "where the types stop").
        (t.playing && moving).then(|| rate * 60.0 / tempo)
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
        // Looping [4, 8): start at 7.5, wrap, keep going.
        let l = Some(LoopRange {
            start: Beat(4.0),
            end: Beat(8.0),
        });
        let mut ph = Playhead::new();
        ph.observe(&block(7.5, 12_000, true, l));
        assert!(!ph.crossed(4.2));
        ph.observe(&block(4.0, 12_000, true, l));
        assert!(ph.crossed(7.7), "before the wrap");
        assert!(ph.crossed(4.2), "the whole loop, once wrapped");
        assert!(!ph.crossed(8.5) && !ph.crossed(3.0));
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
}
