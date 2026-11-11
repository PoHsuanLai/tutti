//! [`Frame`] — an absolute position on the engine's frame clock — and [`At`],
//! the "when" every scheduled command has to state.
//!
//! # Why a position type beside `Samples`
//!
//! [`Samples`] is a **count**: a latency, a ring length, a block length. A
//! frame on the timeline is a **position**, and the two have different
//! algebras, exactly as `Beat` and `BeatDuration` do: a position plus a count
//! is a position, the distance between two positions is a count, and two
//! positions do not add. Carrying both as bare integers is how an
//! off-by-a-block bug gets written — a block-relative offset handed to code
//! that wanted an absolute frame, or the reverse — and nothing notices until
//! a note lands one block late.
//!
//! `u64` rather than `usize`: `Samples` is `usize`, which on a 32-bit target
//! wraps after about a day at 48 kHz. An engine left running is a position
//! that has to survive that.
//!
//! The block-relative half of the split (an *offset* into the current block,
//! valid only inside it) belongs to whatever owns the block; the only
//! conversion between the two goes through that block's environment.
//!
//! # Algebra
//!
//! - `Frame + Samples → Frame` ([`Add`]): advance a position.
//!   Saturating — the counter cannot overflow in practice, and wrapping a
//!   timeline position to zero would replay the session from the top.
//! - The distance between two frames is [`since`](Frame::since), a *checked*
//!   subtraction returning `Option<Samples>`: "how long ago" is only a count
//!   when it is not negative, and the caller has to say what a negative
//!   answer means.
//!
//! Omitted on purpose (the ledger is in the tests, and enforced by the
//! `compile_fail` examples on [`Frame`]):
//!
//! - `Frame + Frame` — two positions do not add.
//! - `Frame - Frame` — see [`since`](Frame::since).
//! - `Frame + u64` / `Frame + usize` — a bare operand could be a count, an
//!   offset or a channel; make the caller say `Samples(n)`.
//! - `Frame * k` — scaling a position depends on where zero is.

use core::ops::{Add, AddAssign};

use super::samples::Samples;
use super::units::Beat;

unit! {
    /// An absolute frame position on the engine's clock: frames rendered since it
    /// started. See the [module docs](self).
    ///
    /// Two positions do not add, and do not subtract with `-`:
    ///
    /// ```compile_fail
    /// use tutti_types::Frame;
    /// let _ = Frame(10) + Frame(20);
    /// ```
    ///
    /// ```compile_fail
    /// use tutti_types::Frame;
    /// let _ = Frame(20) - Frame(10);
    /// ```
    ///
    /// A bare integer is not a frame count either:
    ///
    /// ```compile_fail
    /// use tutti_types::Frame;
    /// let _ = Frame(10) + 5u64;
    /// ```
    ///
    /// What does compile says what it means:
    ///
    /// ```
    /// use tutti_types::{Frame, Samples};
    /// let start = Frame(48_000);
    /// let later = start + Samples(512);
    /// assert_eq!(later.since(start), Some(Samples(512)));
    /// assert_eq!(start.since(later), None); // not a count: it is in the past
    /// ```
    pub struct Frame(pub u64): Display;
}

impl Frame {
    /// The first frame the clock renders.
    pub const ZERO: Frame = Frame(0);

    /// Wraps a raw position already denominated in frames.
    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw position.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The frame at the same wall-clock time at a rate `ratio` times this
    /// one's (`new / old`), to the nearest frame, half away from zero. What the
    /// executor does to its clock when a re-prepare changes the rate; `f64`
    /// holds a frame exactly up to 2^53, about 5 900 years at 48 kHz.
    ///
    /// ```
    /// use tutti_types::Frame;
    /// assert_eq!(Frame(256).scaled(44_100.0 / 48_000.0), Frame(235));
    /// ```
    #[inline]
    #[must_use]
    pub fn scaled(self, ratio: f64) -> Frame {
        Frame((self.0 as f64 * ratio).round() as u64)
    }

    /// The frame at the same wall-clock time at rate `to`, this one being a
    /// position at rate `from`: [`scaled`](Self::scaled) by `to / from`.
    #[inline]
    #[must_use]
    pub fn rescaled(self, from: super::SampleRate, to: super::SampleRate) -> Frame {
        self.scaled(to.get() / from.get())
    }

    /// Returns how many frames `earlier` is before `self`, or `None` when it is not
    /// before (it is later) or the distance does not fit a `Samples` on this
    /// target.
    ///
    /// The named replacement for `Frame - Frame`, which the module docs omit:
    /// an unsigned subtraction either wraps or saturates, and both hide the
    /// ordering bug that produced a negative distance.
    #[inline]
    pub fn since(self, earlier: Frame) -> Option<Samples> {
        let d = self.0.checked_sub(earlier.0)?;
        usize::try_from(d).ok().map(Samples)
    }
}

/// A position advanced by a count. Saturating: see the module docs.
impl Add<Samples> for Frame {
    type Output = Frame;
    #[inline]
    fn add(self, rhs: Samples) -> Frame {
        // `usize` fits in `u64` on every target Rust supports; the fallback
        // is the saturation the operator promises anyway.
        Frame(
            self.0
                .saturating_add(u64::try_from(rhs.0).unwrap_or(u64::MAX)),
        )
    }
}

impl AddAssign<Samples> for Frame {
    #[inline]
    fn add_assign(&mut self, rhs: Samples) {
        *self = *self + rhs;
    }
}

/// A position on the engine's clock and the rate it was counted at: an instant
/// that stays the same instant when the rate changes.
///
/// The engine's frame clock counts frames **at the current rate**, and a
/// re-prepare at another rate moves it to the same wall-clock time at the new
/// one ([`Frame::rescaled`]). A bare [`Frame`] read before such a change and
/// used after it names the wrong instant; a `Moment` carries the rate it was
/// read at, so whoever resolves it can rescale it.
///
/// ```
/// use tutti_types::{Frame, Moment, SampleRate, Samples};
/// let m = Moment::new(Frame(48_000), SampleRate(48_000.0));
/// let later = m.after(Samples(480));
/// assert_eq!((later.frame(), later.rate()), (Frame(48_480), SampleRate(48_000.0)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moment {
    frame: Frame,
    rate: super::SampleRate,
}

impl Moment {
    /// `frame`, counted at `rate`.
    #[inline]
    #[must_use]
    pub const fn new(frame: Frame, rate: super::SampleRate) -> Self {
        Self { frame, rate }
    }

    /// The frame, at [`rate`](Self::rate).
    #[inline]
    #[must_use]
    pub const fn frame(self) -> Frame {
        self.frame
    }

    /// The rate the frame was counted at.
    #[inline]
    #[must_use]
    pub const fn rate(self) -> super::SampleRate {
        self.rate
    }

    /// `frames` later, at the same rate.
    #[inline]
    #[must_use]
    pub fn after(self, frames: Samples) -> Self {
        Self {
            frame: self.frame + frames,
            rate: self.rate,
        }
    }
}

/// When a scheduled command takes effect.
///
/// Every control-thread command that is meant to happen *during playback* —
/// a scheduled event or parameter ramp, and play / stop / seek on a
/// transport — states its time as one of these. There is
/// **no untimed overload**: "whenever the next block starts" is spelled
/// [`NextBlock`](At::NextBlock), a visible, greppable choice rather than a
/// default nobody chose.
///
/// A time already in the past when the command reaches the audio thread is
/// not dropped: it lands at the start of the next block and is counted as
/// late by whoever resolves it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    /// An absolute position on the engine's frame clock.
    ///
    /// That clock counts frames at the **current** rate, and a re-prepare at
    /// another rate moves it to the same wall-clock time at the new one. It is
    /// not a count of frames rendered: a sender that counts those lands early
    /// or late after a rate change by what the rescale dropped. Read the clock
    /// where it is published (`InstrumentHandle::clock`).
    Frame(Frame),
    /// A musical position, resolved against the transport snapshot of the
    /// block it falls in — so a tempo change before it moves it.
    Beat(Beat),
    /// The first frame of the next block the engine renders.
    NextBlock,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advancing adds the count, and saturates instead of wrapping.
    #[test]
    fn advancing_adds_and_saturates() {
        assert_eq!(Frame(100) + Samples(28), Frame(128));
        let mut f = Frame(u64::MAX - 1);
        f += Samples(5);
        assert_eq!(f, Frame(u64::MAX));
    }

    /// `since` is a checked distance: a later frame is not "some frames ago".
    #[test]
    fn since_is_checked() {
        assert_eq!(Frame(600).since(Frame(88)), Some(Samples(512)));
        assert_eq!(Frame(88).since(Frame(88)), Some(Samples(0)));
        assert_eq!(Frame(88).since(Frame(600)), None);
    }

    /// What is deliberately *absent*: `Frame + Frame`, `Frame - Frame`,
    /// `Frame + u64`, `Frame * k` (see the module docs). The first three are
    /// enforced by the `compile_fail` examples on [`Frame`]; this entry keeps
    /// the ledger beside the operators, as `samples.rs` does. Adding an
    /// omitted operator means deleting its entry here and its example there.
    #[test]
    fn omitted_operators_are_documented() {}
}
