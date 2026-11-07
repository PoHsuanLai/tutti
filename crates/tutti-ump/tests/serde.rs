//! The address types read back only what they would construct, and write the
//! bare number. Run with `--features serde`.
//!
//! Mutation record (planted, seen to fail, restored):
//! - `serde(transparent)` in place of `try_from = "u8", into = "u8"` on any of
//!   the three types -> `a_number_above_the_range_is_refused` (200 and 255
//!   deserialize);
//! - `into = "u8"` dropped from `MidiChannel` -> nothing fails: serde writes a
//!   newtype as its inner value in JSON, so the bytes are the same (an
//!   equivalent mutant).
#![cfg(feature = "serde")]

use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup};

fn check<T>(max: u8, make: fn(u8) -> Option<T>, get: fn(&T) -> u8)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    // 0 and max: accepted, and written as the bare number.
    for raw in [0, max] {
        let value = make(raw).unwrap();
        let text = serde_json::to_string(&value).unwrap();
        assert_eq!(text, raw.to_string());
        let back: T = serde_json::from_str(&text).unwrap();
        assert_eq!(get(&back), raw);
        assert_eq!(back, value);
    }
    // max + 1, 200 and 255: refused, with the range in the message.
    for raw in [max + 1, 200, 255] {
        let err = serde_json::from_str::<T>(&raw.to_string()).unwrap_err();
        assert!(
            err.to_string().contains(&format!("largest value, {max}")),
            "{err}"
        );
    }
    // Not a `u8` at all.
    assert!(serde_json::from_str::<T>("-1").is_err());
    assert!(serde_json::from_str::<T>("256").is_err());
    assert!(serde_json::from_str::<T>("\"3\"").is_err());
}

#[test]
fn a_number_above_the_range_is_refused() {
    check(15, MidiChannel::new, |v| v.get());
    check(15, MidiGroup::new, |v| v.get());
    check(127, CCNumber::new, |v| v.get());
}

#[test]
fn the_bytes_are_the_bare_number() {
    assert_eq!(serde_json::to_string(&MidiChannel::of(3)).unwrap(), "3");
    assert_eq!(serde_json::to_string(&MidiGroup::of(15)).unwrap(), "15");
    assert_eq!(serde_json::to_string(&CCNumber::of(74)).unwrap(), "74");
    assert_eq!(
        serde_json::to_string(&[MidiChannel::of(1), MidiChannel::of(2)]).unwrap(),
        "[1,2]"
    );
}
