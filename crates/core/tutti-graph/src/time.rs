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
    /// is never dropped.
    Late,
    /// It falls after this block (or, for a beat, the transport is not
    /// rolling toward it yet).
    NotYet,
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
    ///   the **first frame at or after** the beat (rounded up — a frame that
    ///   starts before the beat has not reached it — within a millionth of a
    ///   frame, so float rounding cannot push a beat that is exactly on a
    ///   frame to the next one). While the transport is
    ///   stopped, or its tempo is not positive, a beat is [`Due::NotYet`]:
    ///   nothing moves toward it. While looping, a beat inside the loop that
    ///   the playhead has passed is reached again after the wrap (so it is
    ///   not late), a beat at or after the loop's end is never reached while
    ///   the loop holds (so it waits), and one before both the loop and the
    ///   playhead is late.
    pub fn due(&self, at: At) -> Due {
        match at {
            At::NextBlock => Due::In(Offset::ZERO),
            At::Frame(f) => match f.since(self.frame) {
                None => Due::Late,
                Some(d) => Offset::new(d.get(), self.block_len).map_or(Due::NotYet, Due::In),
            },
            At::Beat(b) => self.beat_due(b.get()),
        }
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
    pub(crate) fn due_at_arrival(&self, at: &mut At, arrival: Latency) -> Due {
        match *at {
            At::NextBlock => Due::In(Offset::ZERO),
            At::Frame(f) => self.due(At::Frame(f + arrival.samples())),
            At::Beat(_) => match self.due(*at) {
                Due::In(k) => {
                    *at = At::Frame(self.frame_at(k));
                    self.due_at_arrival(at, arrival)
                }
                other => other,
            },
        }
    }

    fn beat_due(&self, beat: f64) -> Due {
        let t = &self.transport;
        let tempo = t.tempo.get();
        let rate = self.sample_rate.get();
        // `is_finite` and `> 0.0` together also refuse a NaN, which no
        // comparison would.
        let moving = tempo.is_finite() && tempo > 0.0 && rate.is_finite() && rate > 0.0;
        if !t.playing || !moving {
            return Due::NotYet;
        }
        // Raw `f64` beats and frames, on purpose: `BeatDuration::to_seconds`
        // lands in `Seconds`, which is `f32` and cannot resolve a frame an
        // hour into a session (CLAUDE.md, "where the types stop"). The
        // product is a frame count inside this block, so it goes straight
        // back to an `Offset`.
        let frames_per_beat = rate * 60.0 / tempo;
        let now = t.beat.get();
        let ahead = match t.looping {
            Some(l) if now < l.end.get() && l.start.get() < l.end.get() => {
                let (start, end) = (l.start.get(), l.end.get());
                if beat >= now && beat < end {
                    beat - now
                } else if beat >= start && beat < now {
                    // Past, but inside the loop: reached after the wrap.
                    (end - now) + (beat - start)
                } else if beat >= end {
                    return Due::NotYet;
                } else {
                    return Due::Late;
                }
            }
            _ if beat >= now => beat - now,
            _ => return Due::Late,
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
    use tutti_types::{Beat, Bpm, SampleRate};

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
        assert_eq!(e.due(At::Beat(Beat(1.0))), Due::Late);
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
        use tutti_types::Latency;
        let a = Latency::new(Samples(60));
        let e = env(1000, 64, Transport::default());
        let mut at = At::Frame(Frame(1000));
        assert_eq!(e.due_at_arrival(&mut at, a), Due::In(Offset::raw(60)));
        let mut next = At::NextBlock;
        assert_eq!(e.due_at_arrival(&mut next, a), Due::In(Offset::ZERO));
        let t = Transport {
            playing: true,
            tempo: Bpm(120.0),
            beat: Beat(1000.0 / 24_000.0),
            looping: None,
        };
        let e = env(1000, 64, t);
        let mut beat = At::Beat(Beat(1010.0 / 24_000.0));
        assert_eq!(
            e.due_at_arrival(&mut beat, a),
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
            next_block.due_at_arrival(&mut beat, a),
            Due::In(Offset::raw(6))
        );
    }

    /// Looping: a passed beat inside the loop comes round again after the
    /// wrap; one past the loop end waits; one before loop and playhead is
    /// late.
    ///
    /// Mutation: drop the wrap branch (treat every beat behind the playhead
    /// as late) → the wrapped beat is `Late` → fails.
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
        assert_eq!(e.due(At::Beat(Beat(2.0))), Due::Late);
    }
}
