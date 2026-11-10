//! [`Ceiling`]: the limits a node builds a design to, and the [`Range`] of a
//! family's own control in it.

use tutti_types::{Db, SampleRate, Seconds};

/// A family's own limit on a control, the part of a [`Ceiling`] only the family
/// knows (a reverb's longest decay, a delay's longest time, `()` for a filter).
pub trait Range: Copy {
    /// The range a node builds to when nothing says otherwise.
    const DEFAULT: Self;

    /// Whether this range can be built to.
    fn valid(&self) -> bool;
}

impl Range for () {
    const DEFAULT: Self = ();

    fn valid(&self) -> bool {
        true
    }
}

/// The longest decay a reverb's control may ask for: a [`Range`] of
/// [`Ceiling`].
///
/// A value is finite, above zero and at most [`CAP`](Self::CAP): bounds sized from
/// it become a realization's allocations, so it is capped (a 60 s decay at
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

impl Range for MaxDecay {
    const DEFAULT: Self = MaxDecay::DEFAULT;

    fn valid(&self) -> bool {
        Self::new(self.0).is_some()
    }
}

/// The limits a node builds a design to: the rate, the family's [`Range`] of a
/// control and the level the tail is measured to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ceiling<R> {
    rate: SampleRate,
    floor: Db,
    range: R,
}

impl<R: Range> Ceiling<R> {
    /// The default tail floor.
    pub const FLOOR: Db = Db(-60.0);

    /// A ceiling, or `None` when the rate is not positive and finite, the range
    /// is not [`valid`](Range::valid), or the floor is not finite and below 0 dB.
    #[must_use]
    pub fn new(rate: SampleRate, range: R, floor: Db) -> Option<Self> {
        let ok = rate.get().is_finite()
            && rate.get() > 0.0
            && range.valid()
            && floor.get().is_finite()
            && floor.get() < 0.0;
        ok.then_some(Self { rate, floor, range })
    }

    /// The defaults at `rate` ([`Range::DEFAULT`], [`FLOOR`](Self::FLOOR)), or
    /// `None` for a rate that is not positive and finite.
    #[must_use]
    pub fn at(rate: SampleRate) -> Option<Self> {
        Self::new(rate, R::DEFAULT, Self::FLOOR)
    }

    /// The rate.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The family's range.
    #[must_use]
    pub fn range(&self) -> R {
        self.range
    }

    /// The level the tail is measured to.
    #[must_use]
    pub fn floor(&self) -> Db {
        self.floor
    }
}

impl Ceiling<MaxDecay> {
    /// The longest decay a control may ask for.
    #[must_use]
    pub fn max_decay(&self) -> Seconds {
        self.range.get()
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tutti_fixtures::{RangeContract, check_range_contract};

    use super::*;

    // The shape `check_range_contract` checks is serde's; `MaxDecay` has none
    // in the library (it was never persisted), so the check runs on the same
    // `try_from` shape written here.
    impl Serialize for MaxDecay {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for MaxDecay {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            Self::try_from(Seconds::deserialize(d)?).map_err(serde::de::Error::custom)
        }
    }

    /// `MaxDecay` keeps the range-checked shape of CLAUDE.md: `new` refuses and
    /// never clamps, `TryFrom` agrees and carries the refused raw.
    ///
    /// Mutations (run): dropping the `s > 0.0` test accepts 0 and -1; `<= CAP`
    /// -> `<= 61.0` accepts 60.001; dropping `is_finite` accepts infinity (the
    /// cap test alone would refuse it, so the NaN row fails instead).
    #[test]
    fn a_max_decay_keeps_the_range_checked_shape() {
        check_range_contract(&RangeContract {
            new: MaxDecay::new,
            get: MaxDecay::get,
            of: None,
            valid: &[Seconds::new(0.001), Seconds::new(8.0), MaxDecay::CAP],
            invalid: &[
                Seconds::new(0.0),
                Seconds::new(-1.0),
                Seconds::new(60.001),
                Seconds::new(1e9),
                Seconds::new(f64::NAN),
                Seconds::new(f64::INFINITY),
            ],
        });
    }

    /// A ceiling refuses what it cannot hold and never clamps.
    ///
    /// Mutations (run): dropping the `floor.get() < 0.0` test accepts a 0 dB
    /// floor and fails the last row; dropping `range.valid()` accepts a range
    /// that is not valid (`()` always is, so the row of a hand-built bad range
    /// fails).
    #[test]
    fn a_ceiling_refuses_what_it_cannot_hold() {
        #[derive(Clone, Copy)]
        struct Never;
        impl Range for Never {
            const DEFAULT: Self = Never;
            fn valid(&self) -> bool {
                false
            }
        }
        let rate = SampleRate(48_000.0);
        let (decay, floor) = (MaxDecay::DEFAULT, Db(-60.0));
        let c = Ceiling::new(rate, decay, floor).unwrap();
        assert_eq!(
            (c.rate(), c.range(), c.max_decay(), c.floor()),
            (rate, decay, Seconds::new(8.0), floor)
        );
        assert_eq!(Ceiling::at(rate), Some(c));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                Ceiling::<MaxDecay>::at(SampleRate(bad)).is_none(),
                "rate {bad}"
            );
        }
        assert!(Ceiling::new(rate, Never, floor).is_none());
        assert!(Ceiling::<()>::at(rate).is_some());
        for bad in [0.0, 3.0, f32::NAN, f32::NEG_INFINITY] {
            assert!(Ceiling::new(rate, decay, Db(bad)).is_none(), "floor {bad}");
        }
    }
}
