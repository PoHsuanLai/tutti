use tutti_fixtures::{RangeContract, check_range_contract};

use super::*;

/// **Every index below 32 is a bit, every other is refused by every
/// door**, and the raw comes back out unchanged. The refusal carries the raw
/// in `get` and in its message.
#[test]
fn the_range_is_held_at_every_door() {
    let valid: Vec<u8> = (0..REQUEST_BITS).collect();
    let invalid = [REQUEST_BITS, REQUEST_BITS + 1, u8::MAX];
    check_range_contract(&RangeContract {
        new: RequestBit::new,
        get: RequestBit::get,
        of: Some(RequestBit::of),
        valid: &valid,
        invalid: &invalid,
    });
    for raw in invalid {
        let err = RequestBit::try_from(raw).unwrap_err();
        assert_eq!(err.get(), raw);
        assert!(err.to_string().contains(&raw.to_string()));
    }
}
