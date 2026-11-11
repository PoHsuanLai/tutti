//! [`ClockMap`]: the pure core that converts an OS time to a graph frame, and
//! back (design 017, §5 "Timing at the edge").
//!
//! The OS stamps a MIDI event with its own clock (an ALSA queue time, a CoreMIDI
//! host time); the graph counts frames. A `ClockMap` holds one **reading** of
//! both clocks at the same moment, `(OS instant, frame)`, and the rate, and
//! answers both ways. It has no clock of its own: every instant is an argument,
//! so every figure below is checkable by arithmetic.
//!
//! # The OS time is an [`Instant`]
//!
//! `std::time::Instant` is the OS's monotonic clock on every target, so the map
//! is tied to none. A backend whose timestamps are on another scale (CoreMIDI's
//! host time, an ALSA queue's own zero) converts them to an `Instant` where it
//! reads them, by pairing one reading of its scale with `Instant::now()`.
//!
//! # The rule
//!
//! The slope is the nominal rate, never fitted. The two clocks' real rates
//! differ by a small relative error `ε` (tens of ppm for crystals), which a
//! fitted slope would estimate from jittery readings. Instead the map follows
//! the *offset*: each new reading `(os, frame)` is compared with where the
//! anchor predicts that frame to be, and the anchor moves **one eighth** of the
//! way to the reading ([`GAIN`]), at the new frame. So:
//!
//! - one late reading (a block's wake-up delayed by `j`) moves the map by `j/8`;
//! - a constant `ε` with readings `T` apart settles to a lag of `8·ε·T`;
//! - a reading further than [`RESYNC`] from the prediction is not jitter (a
//!   device restart, a suspend): the map re-anchors on it exactly.
//!
//! **Bound.** Between readings, a query `Δ` away from the latest anchor is off
//! by at most `ε·(8·T + Δ)` plus the rounding of half a frame, plus the
//! attenuated jitter `j/8`. With `ε = 100 ppm`, `T = 10 ms` and `Δ = 10 ms`
//! that is under 2 µs, a tenth of a frame at 48 kHz. A one-sided jitter (a
//! wake-up is always late, never early) biases the map late by its mean over 8;
//! a reader that must remove it takes its readings at a fixed point of the
//! device callback.
//!
//! # What it refuses
//!
//! A reading whose OS time or frame is earlier than the anchor's is
//! [`Observed::Rejected`], and so is a second reading at the same frame at
//! another time: the map is unchanged. Before the first reading nothing is
//! known: a map exists only from one ([`ClockMap::new`]), and a query is
//! extrapolated from it in either direction, to `None` only where the answer
//! would be before frame 0 (or, for a frame, beyond what an `Instant` holds). A rate change
//! ([`set_rate`](ClockMap::set_rate)) moves the anchor's frame to the same
//! instant at the new rate, as the executor does to its clock on a re-prepare
//! ([`Frame::rescaled`]).

use std::time::{Duration, Instant};

use tutti_types::{Frame, Moment, SampleRate};

/// How far the anchor moves toward a reading, as `1 / GAIN`.
pub const GAIN: u32 = 8;

/// A reading further than this from the prediction re-anchors the map exactly
/// instead of being followed.
pub const RESYNC: Duration = Duration::from_millis(20);

/// Both clocks at the same moment: an OS instant and the graph's frame then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockReading {
    os: Instant,
    frame: Frame,
}

impl ClockReading {
    /// The graph was at `frame` when the OS clock read `os`.
    #[must_use]
    pub const fn new(os: Instant, frame: Frame) -> Self {
        Self { os, frame }
    }

    /// The OS instant.
    #[must_use]
    pub const fn os(self) -> Instant {
        self.os
    }

    /// The graph's frame.
    #[must_use]
    pub const fn frame(self) -> Frame {
        self.frame
    }
}

/// What [`ClockMap::observe`] did with a reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Observed {
    /// The reading was within [`RESYNC`] of the prediction and moved the map
    /// by `1 / GAIN` of the difference.
    Followed,
    /// The reading was further than [`RESYNC`] off: the map is anchored on it.
    Resynced,
    /// The reading is earlier than the anchor in OS time or in frames, or names
    /// the anchor's frame at another time: the map is unchanged.
    Rejected,
}

/// OS time to graph frame and back. The rule and its bound are in the module docs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockMap {
    rate: SampleRate,
    anchor: ClockReading,
}

impl ClockMap {
    /// A map at `rate` from one reading; `None` if `rate` is not finite and
    /// positive.
    #[must_use]
    pub fn new(rate: SampleRate, at: ClockReading) -> Option<Self> {
        valid(rate).then_some(Self { rate, anchor: at })
    }

    /// The rate frames are counted at.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The anchor: the reading the map extrapolates from.
    #[must_use]
    pub fn anchor(&self) -> ClockReading {
        self.anchor
    }

    /// The frame the graph is at when the OS clock reads `os`, to the nearest
    /// frame (half away from zero); `None` before frame 0.
    #[must_use]
    pub fn frame_at(&self, os: Instant) -> Option<Frame> {
        let seconds = match os.checked_duration_since(self.anchor.os) {
            Some(after) => after.as_secs_f64(),
            None => -self.anchor.os.duration_since(os).as_secs_f64(),
        };
        let frames = (self.anchor.frame.get() as f64 + seconds * self.rate.get()).round();
        (frames >= 0.0).then_some(Frame(frames as u64))
    }

    /// [`frame_at`](Self::frame_at) with the rate it is counted at: what the
    /// event inlet's `push_at` takes.
    #[must_use]
    pub fn moment_at(&self, os: Instant) -> Option<Moment> {
        self.frame_at(os).map(|f| Moment::new(f, self.rate))
    }

    /// The OS instant the graph is at `frame`; `None` where that is beyond
    /// what an `Instant` holds. The inverse of
    /// [`frame_at`](Self::frame_at) to within half a frame.
    #[must_use]
    pub fn os_at(&self, frame: Frame) -> Option<Instant> {
        let (ahead, frames) = if frame >= self.anchor.frame {
            (true, frame.get() - self.anchor.frame.get())
        } else {
            (false, self.anchor.frame.get() - frame.get())
        };
        let span = Duration::try_from_secs_f64(frames as f64 / self.rate.get()).ok()?;
        if ahead {
            self.anchor.os.checked_add(span)
        } else {
            self.anchor.os.checked_sub(span)
        }
    }

    /// Takes a new reading, by the rule in the module docs.
    pub fn observe(&mut self, at: ClockReading) -> Observed {
        if at == self.anchor {
            return Observed::Followed;
        }
        if at.os < self.anchor.os || at.frame <= self.anchor.frame {
            return Observed::Rejected;
        }
        let Some(predicted) = self.os_at(at.frame) else {
            return Observed::Rejected;
        };
        let (late, error) = match at.os.checked_duration_since(predicted) {
            Some(late) => (true, late),
            None => (false, predicted.duration_since(at.os)),
        };
        if error > RESYNC {
            self.anchor = at;
            return Observed::Resynced;
        }
        let step = error / GAIN;
        let os = if late {
            predicted.checked_add(step)
        } else {
            predicted.checked_sub(step)
        };
        // `step` is under `RESYNC`; only an `Instant` at its origin cannot
        // subtract it, and the reading itself is then the better anchor.
        self.anchor = ClockReading::new(os.unwrap_or(at.os), at.frame);
        Observed::Followed
    }

    /// Counts frames at `rate` from now on, the anchor moved to the same
    /// instant at the new rate (a re-prepare at another rate). Returns `false`,
    /// and changes nothing, if `rate` is not finite and positive.
    pub fn set_rate(&mut self, rate: SampleRate) -> bool {
        if !valid(rate) {
            return false;
        }
        self.anchor.frame = self.anchor.frame.rescaled(self.rate, rate);
        self.rate = rate;
        true
    }
}

fn valid(rate: SampleRate) -> bool {
    rate.get().is_finite() && rate.get() > 0.0
}

#[cfg(test)]
mod tests;
