use super::*;

/// **Every index below 32 is a bit, every other is refused by every
/// door**, and the raw comes back out unchanged.
///
/// No serde here, so `tutti_fixtures::check_range_contract` (which needs
/// it, and is a tutti crate this one may not depend on) is not used.
///
/// Mutation (run): `new` with `index <= REQUEST_BITS` -> 32 is accepted
/// and `1 << 32` overflows `mask`; the test fails at index 32.
#[test]
fn the_range_is_held_at_every_door() {
    for raw in 0..REQUEST_BITS {
        let bit = RequestBit::new(raw).expect("in range");
        assert_eq!(bit, RequestBit::of(raw));
        assert_eq!(RequestBit::try_from(raw), Ok(bit));
        assert_eq!(u8::from(bit), raw);
        assert_eq!(bit.get(), raw);
    }
    for raw in [REQUEST_BITS, REQUEST_BITS + 1, u8::MAX] {
        assert_eq!(RequestBit::new(raw), None);
        assert_eq!(RequestBit::try_from(raw).unwrap_err().get(), raw);
        assert!(
            RequestBit::try_from(raw)
                .unwrap_err()
                .to_string()
                .contains(&raw.to_string())
        );
        assert!(std::panic::catch_unwind(|| RequestBit::of(raw)).is_err());
    }
}
