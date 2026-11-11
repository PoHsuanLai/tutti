//! The address types read back only what they would construct, and write the
//! bare number. Run with `--features serde`.
#![cfg(feature = "serde")]

use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup};

fn check<T>(what: &str, max: u8, make: fn(u8) -> Option<T>, get: fn(&T) -> u8)
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
        // The message names what was refused, the number and the limit.
        let message = err.to_string();
        for part in [what.to_string(), raw.to_string(), max.to_string()] {
            assert!(message.contains(&part), "{message} lacks {part}");
        }
    }
    // Not a `u8` at all.
    assert!(serde_json::from_str::<T>("-1").is_err());
    assert!(serde_json::from_str::<T>("256").is_err());
    assert!(serde_json::from_str::<T>("\"3\"").is_err());
}

#[test]
fn a_number_above_the_range_is_refused() {
    check("channel", 15, MidiChannel::new, |v| v.get());
    check("group", 15, MidiGroup::new, |v| v.get());
    check("controller number", 127, CCNumber::new, |v| v.get());
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
