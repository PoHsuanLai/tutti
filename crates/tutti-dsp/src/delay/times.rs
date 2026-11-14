//! [`ChannelTimes`]: the delay times of the channels past the first.

use tutti_types::Seconds;

use super::routing::MAX_WIDTH;

/// What a delay's channels past the first are delayed by.
///
/// A delay has one **time control**, channel 0's (and the one a modulation lane drives:
/// a lane drives every channel). What the other channels do with it is a construction
/// constant, not a control:
///
/// - [`linked`](Self::linked): every channel follows the control, so the channels track
///   (the default);
/// - [`each`](Self::each): the channels past the first are delayed by these times, which
///   do not move. A stereo ping-pong's right channel is the usual use.
///
/// A range-checked value: every time is finite and
/// not negative, and never clamped. That they fit the delay's width and its
/// [`MaxDelay`](crate::MaxDelay) is the family's to check, where it is built
/// ([`DelayFamily::with_times`](crate::DelayFamily::with_times)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelTimes {
    count: usize,
    times: [Seconds; MAX_WIDTH - 1],
    linked: bool,
}

/// A refused set of channel times.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelTimesError {
    /// A time is not finite or is negative.
    #[error("the time of channel {channel} is {time:?}: it must be finite and not negative")]
    NotATime {
        /// The channel (1 is the first past the control's).
        channel: usize,
        /// The refused time.
        time: Seconds,
    },
    /// More times than a bus has channels past the first.
    #[error("{count} channel times: a bus has at most {max} past the first")]
    TooMany {
        /// What was given.
        count: usize,
        /// The most a bus holds.
        max: usize,
    },
    /// The times are not one for each channel past the first.
    #[error("{got} channel times on a {width}-channel delay, which takes {expected}")]
    Length {
        /// The delay's width.
        width: usize,
        /// Its channels past the first.
        expected: usize,
        /// What was given.
        got: usize,
    },
    /// A time is past the delay's longest.
    #[error("the time of channel {channel} is {time:?}, past the longest delay {max:?}")]
    Past {
        /// The channel.
        channel: usize,
        /// The refused time.
        time: Seconds,
        /// The delay's longest.
        max: Seconds,
    },
}

impl ChannelTimes {
    /// Every channel follows the time control.
    #[must_use]
    pub const fn linked() -> Self {
        Self {
            count: 0,
            times: [Seconds(0.0); MAX_WIDTH - 1],
            linked: true,
        }
    }

    /// The channels past the first are delayed by `times`, channel 1 first.
    ///
    /// # Errors
    ///
    /// [`ChannelTimesError::NotATime`] or [`TooMany`](ChannelTimesError::TooMany).
    pub fn each(times: &[Seconds]) -> Result<Self, ChannelTimesError> {
        if times.len() > MAX_WIDTH - 1 {
            return Err(ChannelTimesError::TooMany {
                count: times.len(),
                max: MAX_WIDTH - 1,
            });
        }
        let mut all = [Seconds(0.0); MAX_WIDTH - 1];
        for (i, &t) in times.iter().enumerate() {
            if !(t.get().is_finite() && t.get() >= 0.0) {
                return Err(ChannelTimesError::NotATime {
                    channel: i + 1,
                    time: t,
                });
            }
            all[i] = t;
        }
        Ok(Self {
            count: times.len(),
            times: all,
            linked: false,
        })
    }

    /// Whether every channel follows the time control.
    #[must_use]
    pub(crate) const fn is_linked(&self) -> bool {
        self.linked
    }

    /// The fixed times of the channels past the first (empty when
    /// [`linked`](Self::linked)).
    #[must_use]
    pub fn fixed(&self) -> &[Seconds] {
        &self.times[..self.count]
    }
}

impl Default for ChannelTimes {
    fn default() -> Self {
        Self::linked()
    }
}
