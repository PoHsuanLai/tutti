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

/// `MaxDecay` keeps the range-checked shape: `new` refuses and
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

/// A reach refuses what it cannot hold and never clamps.
#[test]
fn a_reach_refuses_what_it_cannot_hold() {
    let rate = SampleRate::SR_48K;
    let (decay, floor) = (MaxDecay::DEFAULT, Db(-60.0));
    let c = Reach::new(rate, decay, floor).unwrap();
    assert_eq!(
        (c.rate(), c.domain(), c.max_decay(), c.floor()),
        (rate, decay, Seconds::new(8.0), floor)
    );
    assert_eq!(Reach::at(rate), c);
    for bad in [0.0, 3.0, f32::NAN, f32::NEG_INFINITY] {
        assert!(Reach::new(rate, decay, Db(bad)).is_none(), "floor {bad}");
    }
}
