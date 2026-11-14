//! `I24` is three bytes and keeps the range-checked contract.

use tutti_buffers::{I24, I24OutOfRange};
use tutti_fixtures::{RangeContract, check_range_contract};

#[test]
fn i24_occupies_three_bytes_and_a_slice_of_them_is_packed() {
    assert_eq!(size_of::<I24>(), 3);
    assert_eq!(size_of::<[I24; 4]>(), 12);
}

#[test]
fn i24_keeps_the_range_checked_contract() {
    check_range_contract(&RangeContract {
        new: I24::new,
        get: I24::get,
        of: Some(I24::of),
        valid: &[
            -8_388_608, -8_388_607, -256, -1, 0, 1, 255, 65_536, 8_388_607,
        ],
        invalid: &[-8_388_609, 8_388_608, i32::MIN, i32::MAX],
    });
}

#[test]
fn i24_range_is_minus_two_to_the_23_through_two_to_the_23_minus_one() {
    assert_eq!(I24::MIN.get(), -(1 << 23));
    assert_eq!(I24::MAX.get(), (1 << 23) - 1);
    assert_eq!(I24::ZERO.get(), 0);
}

#[test]
fn i24_refusal_carries_the_refused_raw() {
    assert_eq!(I24::try_from(1 << 23), Err(I24OutOfRange(1 << 23)));
    assert!(I24OutOfRange(9_000_000).to_string().contains("9000000"));
}

#[test]
fn i24_bytes_are_little_endian_two_complement() {
    assert_eq!(I24::of(0x01_02_03).to_le_bytes(), [0x03, 0x02, 0x01]);
    assert_eq!(I24::of(-1).to_le_bytes(), [0xff, 0xff, 0xff]);
    assert_eq!(I24::MIN.to_le_bytes(), [0x00, 0x00, 0x80]);
    assert_eq!(I24::from_le_bytes([0x00, 0x00, 0x80]), I24::MIN);
}

#[test]
fn i24_orders_by_value_not_by_bytes() {
    assert!(I24::of(-1) < I24::of(0));
    assert!(I24::MIN < I24::of(-1));
    assert!(I24::of(256) > I24::of(255));
}
