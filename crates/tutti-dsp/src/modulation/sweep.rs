//! [`Sweep`] and [`Stagger`]: the construction constants of a chorus or flanger, each
//! checked where it is made.

use tutti_types::{Feedback, MAX_BUS_CHANNELS, Phase, PhaseIncrement, Seconds};

use crate::MaxDelay;

/// What a swept delay is built to: the delay its LFO sweeps around, how far the sweep may
/// reach, the line it must stay inside, and the most feedback it applies.
///
/// A chorus sits at a long base delay (10 ms) so the copy reads as a second voice, a
/// flanger at a short one (1 ms) so the copy combs against the original and the sweep
/// moves the notches.
///
/// A range-checked value (CLAUDE.md, "Range-checked values"): [`new`](Self::new) refuses
/// what the line could not hold, and never clamps. It states the **domain** the family is
/// grown over ([`Grows`](crate::Grows), the `MaxDelay`): the longest delay the sweep reaches
/// is `base + depth_max`, which must fit in `max_delay`. The swept delay used to be a bare
/// `base` and `max` that a transposition made silently clamp at the line's end.
///
/// The durations are [`Seconds`], and none is a control but the depth the sweep is told
/// ([`ModDelayControls::depth`](crate::ModDelayControls::depth)), which the sweep clamps
/// into `depth_min..=depth_max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sweep {
    base: Seconds,
    max_delay: MaxDelay,
    depth_min: Seconds,
    depth_max: Seconds,
    feedback_max: Feedback,
}

/// A refused [`Sweep`]: the raws it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SweepError {
    /// The base delay is not finite or is negative.
    #[error("the base delay is {0:?}: it must be finite and not negative")]
    Base(Seconds),
    /// The depth range is not finite, has a negative end, or its minimum is above its
    /// maximum.
    #[error("the depth range {min:?}..={max:?} is not finite, not negative and in order")]
    Depth {
        /// The refused least depth.
        min: Seconds,
        /// The refused greatest depth.
        max: Seconds,
    },
    /// The sweep's longest delay is past the line: `base + depth_max > max_delay`.
    #[error("the sweep reaches {base:?} + {depth_max:?}, past the longest delay {max_delay:?}")]
    Reach {
        /// The base delay.
        base: Seconds,
        /// The greatest depth.
        depth_max: Seconds,
        /// The line's longest delay.
        max_delay: Seconds,
    },
    /// The most feedback is not in `0..=`[`Feedback::MAX_STABLE`].
    #[error("the most feedback is {0:?}: it must be finite, in 0..=MAX_STABLE")]
    FeedbackMax(Feedback),
}

impl Sweep {
    /// A sweep around `base`, its depth in `depth_min..=depth_max`, held in a line of
    /// `max_delay`, applying at most `feedback_max`.
    ///
    /// # Errors
    ///
    /// [`SweepError::Base`], [`Depth`](SweepError::Depth), [`Reach`](SweepError::Reach) for
    /// a sweep past the line, and [`FeedbackMax`](SweepError::FeedbackMax).
    pub const fn new(
        base: Seconds,
        max_delay: MaxDelay,
        depth_min: Seconds,
        depth_max: Seconds,
        feedback_max: Feedback,
    ) -> Result<Self, SweepError> {
        let (b, lo, hi) = (base.get(), depth_min.get(), depth_max.get());
        if !(b.is_finite() && b >= 0.0) {
            return Err(SweepError::Base(base));
        }
        if !(lo.is_finite() && lo >= 0.0 && hi.is_finite() && lo <= hi) {
            return Err(SweepError::Depth {
                min: depth_min,
                max: depth_max,
            });
        }
        if b + hi > max_delay.get().get() {
            return Err(SweepError::Reach {
                base,
                depth_max,
                max_delay: max_delay.get(),
            });
        }
        let f = feedback_max.0;
        if !(f.is_finite() && f >= 0.0 && f <= Feedback::MAX_STABLE.0) {
            return Err(SweepError::FeedbackMax(feedback_max));
        }
        Ok(Self {
            base,
            max_delay,
            depth_min,
            depth_max,
            feedback_max,
        })
    }

    /// A sweep for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`new`](Self::new) refuses it.
    #[must_use]
    pub const fn of(
        base: Seconds,
        max_delay: MaxDelay,
        depth_min: Seconds,
        depth_max: Seconds,
        feedback_max: Feedback,
    ) -> Self {
        match Self::new(base, max_delay, depth_min, depth_max, feedback_max) {
            Ok(s) => s,
            Err(_) => panic!("a sweep stays inside its line"),
        }
    }

    /// Chorus: a 10 ms base delay in a 50 ms line, the depth clamped to `0..=40 ms`.
    /// The long base delay is what separates chorus from flanger: the copy sits far
    /// enough behind to read as a second voice.
    pub const CHORUS: Self = Self::of(
        Seconds(0.01),
        MaxDelay::of(Seconds(0.05)),
        Seconds(0.0),
        Seconds(0.04),
        Feedback::MAX_STABLE,
    );

    /// Flanger: a 1 ms base delay in a 20 ms line, the depth clamped to `0.1..=10 ms`.
    /// The short base delay lands the copy close enough to interfere with the original,
    /// producing the comb notches whose sweep is the flanger's signature.
    pub const FLANGER: Self = Self::of(
        Seconds(0.001),
        MaxDelay::of(Seconds(0.02)),
        Seconds(0.0001),
        Seconds(0.01),
        Feedback::MAX_STABLE,
    );

    /// The chorus of a General MIDI synthesizer's chorus send, as SoundFont players have
    /// it: a 2 ms centre delay in a 5 ms line, the depth up to 1.9 ms either side, and
    /// **no feedback at all** (`feedback_max` is [`Feedback::NONE`]), which is what lets
    /// its node declare a finite ceiling: what it outputs was in its input at most
    /// `max_delay` ago.
    pub const GENERAL_MIDI_CHORUS: Self = Self::of(
        Seconds(0.002),
        MaxDelay::of(Seconds(0.005)),
        Seconds(0.0),
        Seconds(0.0019),
        Feedback::NONE,
    );

    /// The centre delay the LFO sweeps around.
    #[must_use]
    pub const fn base(&self) -> Seconds {
        self.base
    }

    /// The line's longest delay: the [`Domain`](crate::Domain) the sweep is grown over.
    #[must_use]
    pub const fn max_delay(&self) -> MaxDelay {
        self.max_delay
    }

    /// The least depth the sweep takes: a smaller one reads as it.
    #[must_use]
    pub const fn depth_min(&self) -> Seconds {
        self.depth_min
    }

    /// The greatest depth the sweep takes: a larger one reads as it, which is what keeps
    /// the sweep inside the line (chorus) or inside comb-filter range (flanger).
    #[must_use]
    pub const fn depth_max(&self) -> Seconds {
        self.depth_max
    }

    /// The most feedback the sweep applies, whatever it is asked for: a larger one reads as
    /// it and one below zero as zero. [`Feedback::NONE`] makes the line a pure delay, with
    /// no recirculation.
    #[must_use]
    pub const fn feedback_max(&self) -> Feedback {
        self.feedback_max
    }
}

/// How each channel's sweep is staggered from channel 0's, in LFO cycles.
///
/// A range-checked value (CLAUDE.md, "Range-checked values"): every offset is finite, and
/// every channel's offset is wrapped into one cycle ([`Phase`]'s own wrap): at construction
/// for [`each`](Self::each), and in [`offset`](Self::offset) for a [`step`](Self::step), whose
/// channel `c` is `c` times it. A stagger that names
/// one offset per channel ([`each`](Self::each)) is checked against the bus it is built for
/// by [`ModDelayFamily::new`](crate::ModDelayFamily::new).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stagger {
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// Channel `c` runs `c` times this ahead of channel 0, wrapped.
    Step(PhaseIncrement),
    /// One wrapped offset for each of the first `count` channels.
    Each {
        count: usize,
        offsets: [PhaseIncrement; MAX_BUS_CHANNELS],
    },
}

/// A refused [`Stagger`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum StaggerError {
    /// An offset is not finite.
    #[error("the offset of channel {channel} is {offset:?}, not finite")]
    NotFinite {
        /// The channel.
        channel: usize,
        /// The refused offset.
        offset: PhaseIncrement,
    },
    /// More offsets than a bus has channels.
    #[error("{count} offsets: a bus has at most {max} channels")]
    TooMany {
        /// What was given.
        count: usize,
        /// The most a bus holds.
        max: usize,
    },
    /// The offsets are not one for each channel.
    #[error("{got} offsets on a {width}-channel sweep, which takes {width}")]
    Length {
        /// The bus's width.
        width: usize,
        /// What was given.
        got: usize,
    },
}

impl Stagger {
    /// Channel `c` runs `c` times `step` ahead of channel 0, wrapped into one cycle: at
    /// width 2 the step is the right channel's offset. `None` when `step` is not finite.
    #[must_use]
    pub fn step(step: PhaseIncrement) -> Option<Self> {
        if !step.get().is_finite() {
            return None;
        }
        Some(Self {
            kind: Kind::Step(step),
        })
    }

    /// A step for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `step` is not finite.
    #[must_use]
    pub fn of_step(step: PhaseIncrement) -> Self {
        match Self::step(step) {
            Some(s) => s,
            None => panic!("a stagger step is finite"),
        }
    }

    /// One offset for each channel, channel 0's first (usually zero).
    ///
    /// # Errors
    ///
    /// [`StaggerError::NotFinite`] or [`TooMany`](StaggerError::TooMany).
    pub fn each(offsets: &[PhaseIncrement]) -> Result<Self, StaggerError> {
        if offsets.len() > MAX_BUS_CHANNELS {
            return Err(StaggerError::TooMany {
                count: offsets.len(),
                max: MAX_BUS_CHANNELS,
            });
        }
        let mut all = [PhaseIncrement(0.0); MAX_BUS_CHANNELS];
        for (channel, &offset) in offsets.iter().enumerate() {
            if !offset.0.is_finite() {
                return Err(StaggerError::NotFinite { channel, offset });
            }
            all[channel] = wrap(offset);
        }
        Ok(Self {
            kind: Kind::Each {
                count: offsets.len(),
                offsets: all,
            },
        })
    }

    /// How many channels it names an offset for, or `None` for a [`step`](Self::step),
    /// which fits any width.
    #[must_use]
    pub const fn channels(&self) -> Option<usize> {
        match self.kind {
            Kind::Step(_) => None,
            Kind::Each { count, .. } => Some(count),
        }
    }

    /// Channel `c`'s offset in cycles, wrapped into `[0, 1)`: `c` times the step, or the
    /// offset it was given (zero for a channel an [`each`](Self::each) names none for).
    #[must_use]
    pub fn offset(&self, channel: usize) -> PhaseIncrement {
        match self.kind {
            Kind::Step(step) => wrap(PhaseIncrement(step.0 * channel as f32)),
            Kind::Each { offsets, .. } => {
                offsets.get(channel).copied().unwrap_or(PhaseIncrement(0.0))
            }
        }
    }
}

/// `x` wrapped into one cycle by [`Phase`]'s own wrap.
fn wrap(x: PhaseIncrement) -> PhaseIncrement {
    PhaseIncrement(Phase::START.advance(x).get())
}
