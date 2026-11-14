//! One check that a value's JSON spelling is pinned in both directions.

use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

/// Panics when `value` does not serialise to exactly `json`, or when `json`
/// does not deserialise back to `value`.
pub fn check_round_trip<T>(value: &T, json: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    assert_eq!(
        serde_json::to_string(value).unwrap(),
        json,
        "{value:?} serialises to a different document"
    );
    assert_eq!(
        &serde_json::from_str::<T>(json).unwrap(),
        value,
        "{json} deserialises to a different value"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct P {
        pub a: u8,
    }

    #[test]
    fn a_value_and_its_document_pass() {
        check_round_trip(&P { a: 3 }, r#"{"a":3}"#);
    }

    #[test]
    #[should_panic(expected = "serialises to a different document")]
    fn a_different_document_fails() {
        check_round_trip(&P { a: 3 }, r#"{"a":4}"#);
    }

    #[test]
    #[should_panic(expected = "deserialises to a different value")]
    fn a_document_that_reads_back_differently_fails() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Lossy {
            #[serde(skip_deserializing)]
            pub a: u8,
        }
        check_round_trip(&Lossy { a: 3 }, r#"{"a":3}"#);
    }
}
