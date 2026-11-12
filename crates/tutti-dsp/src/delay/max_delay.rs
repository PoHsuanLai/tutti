//! [`MaxDelay`]: the longest delay a delay line is built to hold, the [`Domain`] of a
//! delay's time.

use tutti_types::{SampleRate, Seconds};

use crate::Domain;

/// The longest delay a delay's time control may ask for: a [`Domain`] of
/// [`Reach`](crate::Reach), and the size of the line a [`DelayFamily`](crate::DelayFamily)
/// allocates.
///
/// A range-checked value (CLAUDE.md, "Range-checked values"): finite, above zero and at
/// most [`CAP`](Self::CAP). [`new`](Self::new) refuses anything else and never clamps: the
/// maximum becomes an allocation (a line per channel), so a NaN, a negative span or a
/// finite one of 1e30 seconds must not reach it (it used to abort the process).
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct MaxDelay(Seconds);

/// A refused longest delay: the raw it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} is not a longest delay: it must be above zero and at most {cap:?}", cap = MaxDelay::CAP)]
pub struct MaxDelayError(pub Seconds);

impl MaxDelay {
    /// The longest delay of a node built without one: 2 s.
    pub const DEFAULT: Self = Self(Seconds(2.0));
    /// The most [`new`](Self::new) accepts: 10 s. A line of that length at 192 kHz is
    /// 1.92 million frames of `f32`, so 16 channels hold 123 MB: the cap is what a
    /// capacity the node allocates at the device's rate can be.
    pub const CAP: Seconds = Seconds(10.0);

    /// A longest delay of `seconds`, or `None` when it is not finite, not above zero or
    /// above [`CAP`](Self::CAP). Never clamps.
    #[must_use]
    pub const fn new(seconds: Seconds) -> Option<Self> {
        let s = seconds.get();
        if s.is_finite() && s > 0.0 && s <= Self::CAP.get() {
            Some(Self(seconds))
        } else {
            None
        }
    }

    /// A longest delay for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`new`](Self::new) refuses `seconds`.
    #[must_use]
    pub const fn of(seconds: Seconds) -> Self {
        match Self::new(seconds) {
            Some(m) => m,
            None => panic!("a longest delay is above zero and at most ten seconds"),
        }
    }

    /// The longest delay, in seconds.
    #[must_use]
    pub const fn get(self) -> Seconds {
        self.0
    }

    /// The frames a line holds for it at `rate`: `ceil(max * rate)`, from the exact
    /// product. Saturated at 16.7 million frames: a rate no device has must not make an
    /// allocation that aborts.
    #[must_use]
    pub fn frames(self, rate: SampleRate) -> usize {
        self.0.to_samples_ceil(rate).get().min(MAX_FRAMES)
    }
}

/// The most frames a line is allocated for whatever the rate: 16.7 million, over 87 s
/// at 192 kHz, far past the 1.92 million that [`MaxDelay::CAP`] asks for at any rate a
/// device runs at. It guards only an absurd rate, since a prepare must not abort.
pub(crate) const MAX_FRAMES: usize = 1 << 24;

impl Default for MaxDelay {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<Seconds> for MaxDelay {
    type Error = MaxDelayError;

    fn try_from(seconds: Seconds) -> Result<Self, MaxDelayError> {
        Self::new(seconds).ok_or(MaxDelayError(seconds))
    }
}

impl From<MaxDelay> for Seconds {
    fn from(max: MaxDelay) -> Seconds {
        max.0
    }
}

impl Domain for MaxDelay {
    const DEFAULT: Self = MaxDelay::DEFAULT;
}
