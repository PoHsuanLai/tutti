//! Stopping a render, and watching it (doc 019 section 6).
//!
//! A render is one synchronous call, so a host steps in through one
//! [`Control`]: a [`CancelToken`] the pipeline checks at every point of doc 019
//! section 6.2, and an optional callback that is handed a [`Progress`] (a
//! [`Stage`] and one fraction of the whole render). A render the token
//! stopped reports [`Outcome::Cancelled`], not an error and not a short file.
//!
//! The fraction is the pipeline's: a stage owns a band of `[0, 1]` (the tables
//! below, one place), and a host never weights passes itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_types::Samples;

/// A request to stop a render, shared between the thread that asks and the one
/// that renders. Cheap to clone; every clone is the same token.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token nobody has cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the render to stop. It does at the next point it checks (doc 019
    /// section 6.2); the request cannot be taken back.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether [`cancel`](Self::cancel) was called on this token or a clone.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// What a render is doing.
///
/// A render passes through these in order, skipping those it has no use for:
/// without normalize `Rendering` then `Finalizing`; with it `Rendering`,
/// `Measuring`, `Writing`, `Finalizing`. `Preparing` is the session's (the
/// fork's build); the pipeline starts at `Rendering`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Stage {
    /// Building what the render needs (the session's steps, before the pipeline).
    Preparing,
    /// The graph rendering, and with no normalize the encoding of each block.
    Rendering,
    /// Converting the rate and reading the loudness a normalize gain comes from.
    Measuring,
    /// Applying the gain and encoding what was rendered.
    Writing,
    /// Flushing the converter, checking the fork, and putting the file in place.
    Finalizing,
}

/// Where a render is: its [`Stage`] and how far through the whole of it.
///
/// Read-only and `Copy`. The fraction never decreases within a render, and is
/// exactly `1.0` only in the reading delivered after the last file is in place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    stage: Stage,
    fraction: f32,
    output: Option<usize>,
    frames: Option<(Samples, Samples)>,
}

impl Progress {
    /// The stage the render is in.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// How far through the whole render, in `[0, 1]`.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        self.fraction
    }

    /// The output being measured, written or finalized; `None` while rendering
    /// (one render serves every output).
    #[must_use]
    pub fn output(&self) -> Option<usize> {
        self.output
    }

    /// Frames done and frames in all, for a stage that counts frames. They are
    /// the stage's own units: the graph's frames while rendering, and for
    /// `Measuring` the frames converted and then measured (the conversion is
    /// counted against twice the render's length, an estimate the measurement
    /// replaces with the converted length; with no conversion the stage is
    /// the render's length).
    #[must_use]
    pub fn frames(&self) -> Option<(Samples, Samples)> {
        self.frames
    }
}

/// How a render that was not an error ended.
#[must_use = "a cancelled render produced nothing"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome<T> {
    /// It ran to the end; `T` is what it produced.
    Finished(T),
    /// The token was cancelled, including during the last block. A file
    /// target is untouched: the render was written to a temporary, which is
    /// removed.
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

/// A stage's band of the fraction.
#[derive(Debug, Clone, Copy)]
struct Band {
    stage: Stage,
    from: f32,
    to: f32,
}

const fn band(stage: Stage, from: f32, to: f32) -> Band {
    Band { stage, from, to }
}

/// Doc 019 section 6.1 without normalize: encoding happens during the render.
const STREAMING: [Band; 3] = [
    band(Stage::Preparing, 0.00, 0.05),
    band(Stage::Rendering, 0.05, 0.95),
    band(Stage::Finalizing, 0.95, 1.00),
];

/// Doc 019 section 6.1 with normalize on an output: the render is kept, then
/// measured, then encoded.
const NORMALIZED: [Band; 5] = [
    band(Stage::Preparing, 0.00, 0.03),
    band(Stage::Rendering, 0.03, 0.55),
    band(Stage::Measuring, 0.55, 0.62),
    band(Stage::Writing, 0.62, 0.95),
    band(Stage::Finalizing, 0.95, 1.00),
];

/// Which table of bands a render's fraction follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Weights {
    /// No normalize: [`STREAMING`].
    Streaming,
    /// Normalize on an output: [`NORMALIZED`].
    Normalized,
}

impl Weights {
    fn table(self) -> &'static [Band] {
        match self {
            Self::Streaming => &STREAMING,
            Self::Normalized => &NORMALIZED,
        }
    }

    /// The band of `stage`, or `None` for a stage this render does not have.
    fn band(self, stage: Stage) -> Option<(f32, f32)> {
        self.table()
            .iter()
            .find(|b| b.stage == stage)
            .map(|b| (b.from, b.to))
    }
}

/// Frames between two deliveries of a stage that counts them.
///
/// Doc 019 section 6.1 also asks for 50 ms of wall time ("whichever is
/// later"). That would leave a render faster than 80 kHz of audio reporting
/// less often than every 4096 frames, which the cadence test of the same
/// section forbids, so the frame bound is the rule and no clock is read.
pub(crate) const DELIVERY_FRAMES: usize = 4096;

/// What a render is told by, and tells: a [`CancelToken`] it checks and a
/// callback it reports through. Replaces the `Flow` and `Pass` of the old
/// `_with` twins.
///
/// ```
/// # use tutti_export::{CancelToken, Control, Progress};
/// let token = CancelToken::new();
/// let mut fractions = Vec::new();
/// let mut record = |p: &Progress| fractions.push(p.fraction());
/// let control = Control::new(&token).on_progress(&mut record);
/// # let _ = control;
/// ```
pub struct Control<'a> {
    cancel: Option<&'a CancelToken>,
    progress: Option<&'a mut dyn FnMut(&Progress)>,
    weights: Weights,
    last: Option<Progress>,
}

impl std::fmt::Debug for Control<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("cancelled", &self.is_cancelled())
            .field("reports", &self.progress.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> Control<'a> {
    /// A control that stops when `cancel` does, and reports to nobody.
    #[must_use]
    pub fn new(cancel: &'a CancelToken) -> Self {
        Self {
            cancel: Some(cancel),
            progress: None,
            weights: Weights::Streaming,
            last: None,
        }
    }

    /// The same control, reporting every [`Progress`] to `progress`.
    ///
    /// The callback runs on the rendering thread, between blocks: it should
    /// store the reading and return. It may cancel the token it shares with
    /// this control, and the render stops at the check that follows.
    #[must_use]
    pub fn on_progress(mut self, progress: &'a mut dyn FnMut(&Progress)) -> Self {
        self.progress = Some(progress);
        self
    }

    /// A control with nothing to cancel it and nobody to report to.
    #[must_use]
    pub fn none() -> Control<'static> {
        Control {
            cancel: None,
            progress: None,
            weights: Weights::Streaming,
            last: None,
        }
    }

    /// Whether the token was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_some_and(CancelToken::is_cancelled)
    }

    /// Starts a render that follows `weights`: forgets what an earlier render
    /// delivered, so the fraction restarts.
    pub(crate) fn weigh(&mut self, weights: Weights) {
        self.weights = weights;
        self.last = None;
    }

    /// Enters `stage`: always delivered, at the start of its band.
    pub(crate) fn begin(&mut self, stage: Stage, total: Option<Samples>) {
        self.deliver(stage, 0.0, total.map(|t| (Samples(0), t)));
    }

    /// `done` of `total` frames of `stage`: delivered when the stage is
    /// entered, when it completes and after every [`DELIVERY_FRAMES`] since
    /// the last.
    pub(crate) fn advance(&mut self, stage: Stage, done: Samples, total: Samples) {
        let due = match self.last {
            Some(last) if last.stage == stage => {
                done >= total
                    || last
                        .frames
                        .is_none_or(|(at, _)| done.get() >= at.get() + DELIVERY_FRAMES)
            }
            _ => true,
        };
        if due {
            let within = if total == Samples(0) {
                1.0
            } else {
                done.get() as f32 / total.get() as f32
            };
            self.deliver(stage, within, Some((done, total)));
        }
    }

    /// A point `within` (`[0, 1]`) of a stage that does not count frames,
    /// always delivered.
    pub(crate) fn at(&mut self, stage: Stage, within: f32) {
        self.deliver(stage, within, None);
    }

    /// Delivers the reading for a position in `stage`'s band. The fraction
    /// never falls below the last one delivered.
    fn deliver(&mut self, stage: Stage, within: f32, frames: Option<(Samples, Samples)>) {
        let (from, to) = self.weights.band(stage).unwrap_or_else(|| {
            debug_assert!(false, "{stage:?} is not a stage of {:?}", self.weights);
            (0.0, 0.0)
        });
        let floor = self.last.map_or(0.0, |p| p.fraction);
        // The end of a band is its `to` and nothing near it: `from + (to -
        // from) * 1.0` can round off it, and the last band must land on 1.0.
        let fraction = if within >= 1.0 {
            to
        } else {
            from + (to - from) * within.clamp(0.0, 1.0)
        }
        .max(floor);
        let progress = Progress {
            stage,
            fraction,
            // One output until `render` takes several (doc 019 step 3).
            output: match stage {
                Stage::Preparing | Stage::Rendering => None,
                Stage::Measuring | Stage::Writing | Stage::Finalizing => Some(0),
            },
            frames,
        };
        self.last = Some(progress);
        if let Some(report) = self.progress.as_mut() {
            report(&progress);
        }
    }
}

#[cfg(test)]
mod tests;
