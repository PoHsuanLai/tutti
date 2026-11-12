//! [`Reach`]: the limits a node builds a family to, and the [`Domain`] of a
//! family's own control in it.

use tutti_types::{Db, SampleRate, Seconds};

/// A family's own limit on a control, `D ⊆ C` of design 024 section 2.7.2: the part of
/// a [`Reach`] only the family knows (a reverb's longest decay, a delay's longest time, `()` for a filter).
pub trait Domain: Copy {
    /// The domain a node builds to when nothing says otherwise.
    const DEFAULT: Self;

    /// Whether this domain can be built to.
    fn valid(&self) -> bool;
}

impl Domain for () {
    const DEFAULT: Self = ();

    fn valid(&self) -> bool {
        true
    }
}

/// The longest decay a reverb's control may ask for: a [`Domain`] of
/// [`Reach`].
///
/// A value is finite, above zero and at most [`CAP`](Self::CAP): a capacity sized from
/// it becomes a runner's allocations, so it is capped (a 60 s decay at
/// 192 kHz is already some 11.5 M frames of line to hold).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaxDecay(Seconds);

/// A refused longest decay: the raw it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} is not a longest decay: it must be above zero and at most {cap:?}", cap = MaxDecay::CAP)]
pub struct MaxDecayError(pub Seconds);

impl MaxDecay {
    /// The longest decay a control may ask for by default.
    pub const DEFAULT: MaxDecay = MaxDecay(Seconds::new(8.0));
    /// The most [`new`](Self::new) accepts.
    pub const CAP: Seconds = Seconds::new(60.0);

    /// A longest decay of `seconds`, or `None` when it is not finite, not
    /// above zero or above [`CAP`](Self::CAP). Never clamps.
    #[must_use]
    pub fn new(seconds: Seconds) -> Option<Self> {
        let s = seconds.get();
        (s.is_finite() && s > 0.0 && s <= Self::CAP.get()).then_some(Self(seconds))
    }

    /// The longest decay.
    #[must_use]
    pub fn get(self) -> Seconds {
        self.0
    }
}

impl TryFrom<Seconds> for MaxDecay {
    type Error = MaxDecayError;

    fn try_from(seconds: Seconds) -> Result<Self, MaxDecayError> {
        Self::new(seconds).ok_or(MaxDecayError(seconds))
    }
}

impl From<MaxDecay> for Seconds {
    fn from(decay: MaxDecay) -> Seconds {
        decay.0
    }
}

impl Domain for MaxDecay {
    const DEFAULT: Self = MaxDecay::DEFAULT;

    fn valid(&self) -> bool {
        Self::new(self.0).is_some()
    }
}

/// The limits a node builds a family to: the rate, the family's [`Domain`] of a
/// control and the level the tail is measured to; the argument of
/// [`Grows::reach`](super::Grows::reach).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reach<D> {
    rate: SampleRate,
    floor: Db,
    domain: D,
}

impl<D: Domain> Reach<D> {
    /// The default tail floor.
    pub const FLOOR: Db = Db(-60.0);

    /// A reach, or `None` when the rate is not positive and finite, the domain
    /// is not [`valid`](Domain::valid), or the floor is not finite and below 0 dB.
    #[must_use]
    pub fn new(rate: SampleRate, domain: D, floor: Db) -> Option<Self> {
        let ok = rate.get().is_finite()
            && rate.get() > 0.0
            && domain.valid()
            && floor.get().is_finite()
            && floor.get() < 0.0;
        ok.then_some(Self {
            rate,
            floor,
            domain,
        })
    }

    /// The defaults at `rate` ([`Domain::DEFAULT`], [`FLOOR`](Self::FLOOR)), or
    /// `None` for a rate that is not positive and finite.
    #[must_use]
    pub fn at(rate: SampleRate) -> Option<Self> {
        Self::new(rate, D::DEFAULT, Self::FLOOR)
    }

    /// The rate.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The family's domain.
    #[must_use]
    pub fn domain(&self) -> D {
        self.domain
    }

    /// The level the tail is measured to.
    #[must_use]
    pub fn floor(&self) -> Db {
        self.floor
    }
}

impl Reach<MaxDecay> {
    /// The longest decay a control may ask for.
    #[must_use]
    pub fn max_decay(&self) -> Seconds {
        self.domain.get()
    }
}

#[cfg(test)]
mod tests;
