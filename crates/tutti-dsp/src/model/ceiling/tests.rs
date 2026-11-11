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
