//! Stopping a render, and watching it.
//!
//! A render is one synchronous call, so the only place a host can step in is
//! between blocks: the driver hands a tick the [`Progress`] after each block,
//! and the tick answers with a [`Flow`]. A render a tick stopped reports
//! [`Outcome::Cancelled`], not an error and not a short file.

use tutti_types::Samples;

/// What a tick tells the render to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Render the next block.
    Continue,
    /// Stop now. The render reports [`Outcome::Cancelled`].
    Stop,
}

/// Which pass of a render a [`Progress`] is about.
///
/// Most renders are one pass. A normalized export is two: the graph renders
/// into memory (pass 0), then the gained audio is written (pass 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    index: u8,
    count: u8,
}

impl Pass {
    /// The only pass of a render that has one.
    pub const ONLY: Self = Self { index: 0, count: 1 };

    /// Pass `index` (from 0) of `count`; `None` unless `index < count`.
    #[must_use]
    pub const fn new(index: u8, count: u8) -> Option<Self> {
        if index < count {
            Some(Self { index, count })
        } else {
            None
        }
    }

    /// This pass, counted from 0.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.index
    }

    /// How many passes the render has.
    #[must_use]
    pub const fn count(self) -> u8 {
        self.count
    }

    /// The first pass of a two-pass (normalized) render.
    pub(crate) const FIRST_OF_TWO: Self = Self { index: 0, count: 2 };
    /// The second pass of a two-pass (normalized) render.
    pub(crate) const SECOND_OF_TWO: Self = Self { index: 1, count: 2 };
}

/// Where a render is, after a block.
///
/// `produced` rises by at least one frame per tick and reaches `total` on the
/// last block of the pass. Both are source-side frames: the render's own
/// length, latency trim and tail included, before any resample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The pass this reading is about.
    pub pass: Pass,
    /// Frames rendered so far in this pass.
    pub produced: Samples,
    /// Frames this pass renders in all.
    pub total: Samples,
}

/// How a render that was not an error ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome<T> {
    /// It ran to the end; `T` is what it produced.
    Finished(T),
    /// A tick stopped it. A file target holds nothing: the file is removed.
    Cancelled,
}

impl<T> Outcome<T> {
    /// What the render produced, or `None` if it was cancelled.
    #[must_use]
    pub fn finished(self) -> Option<T> {
        match self {
            Self::Finished(t) => Some(t),
            Self::Cancelled => None,
        }
    }

    /// The same outcome with a finished value mapped by `f`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Outcome<U> {
        match self {
            Self::Finished(t) => Outcome::Finished(f(t)),
            Self::Cancelled => Outcome::Cancelled,
        }
    }
}

/// The tick a pass reports through: its [`Pass`] and the host's callback.
pub(crate) struct Control<'a> {
    pass: Pass,
    tick: &'a mut dyn FnMut(Progress) -> Flow,
}

impl<'a> Control<'a> {
    pub(crate) fn new(pass: Pass, tick: &'a mut dyn FnMut(Progress) -> Flow) -> Self {
        Self { pass, tick }
    }

    /// The same tick, reporting as `pass`.
    pub(crate) fn pass(&mut self, pass: Pass) -> Control<'_> {
        Control {
            pass,
            tick: &mut *self.tick,
        }
    }

    pub(crate) fn report(&mut self, produced: Samples, total: Samples) -> Flow {
        (self.tick)(Progress {
            pass: self.pass,
            produced,
            total,
        })
    }
}

/// A tick that never stops.
pub(crate) fn never_stop(_: Progress) -> Flow {
    Flow::Continue
}
