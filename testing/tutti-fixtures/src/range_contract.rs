//! One check that a range-checked type keeps the contract in `CLAUDE.md`
//! ("Range-checked values"). It names none of tutti's types.

use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// What a type's author states: how to build it and which raws it takes.
pub struct RangeContract<'a, T, R> {
    pub new: fn(R) -> Option<T>,
    pub get: fn(T) -> R,
    /// Present when the type has `of`.
    pub of: Option<fn(R) -> T>,
    pub valid: &'a [R],
    pub invalid: &'a [R],
}

/// Panics, naming the raw, when the type breaks the contract.
///
/// For each valid raw, `new`, `get`, `TryFrom`, `Into` and `of` agree and serde
/// round-trips as the bare raw. For each invalid raw, `new` is `None`,
/// `TryFrom` errs, `of` panics and serde refuses. JSON has no NaN or infinity:
/// for those raws serde is not checked, only the constructors.
pub fn check_range_contract<T, R>(c: &RangeContract<'_, T, R>)
where
    T: TryFrom<R> + Into<R> + Copy + PartialEq + Debug + Serialize + DeserializeOwned,
    R: Copy + Debug + PartialEq + Serialize,
    <T as TryFrom<R>>::Error: Debug,
{
    assert!(
        !c.valid.is_empty() && !c.invalid.is_empty(),
        "state both sides of the range"
    );
    for &r in c.valid {
        let t = (c.new)(r).unwrap_or_else(|| panic!("new refused valid {r:?}"));
        assert_eq!((c.get)(t), r, "get(new({r:?}))");
        assert_eq!(T::try_from(r).ok(), Some(t), "try_from({r:?})");
        assert_eq!(Into::<R>::into(t), r, "into({r:?})");
        if let Some(of) = c.of {
            assert_eq!(of(r), t, "of({r:?})");
        }
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(
            json,
            serde_json::to_string(&r).unwrap(),
            "serialises as the bare raw"
        );
        assert_eq!(
            serde_json::from_str::<T>(&json).unwrap(),
            t,
            "serde round trip of {r:?}"
        );
    }
    for &r in c.invalid {
        assert!((c.new)(r).is_none(), "new accepted {r:?}");
        assert!(T::try_from(r).is_err(), "try_from accepted {r:?}");
        if let Some(of) = c.of {
            assert!(
                catch_unwind(AssertUnwindSafe(|| of(r))).is_err(),
                "of accepted {r:?}"
            );
        }
        let json = serde_json::to_string(&r).unwrap();
        if json != "null" {
            assert!(
                serde_json::from_str::<T>(&json).is_err(),
                "deserialize accepted {r:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// Refuses above 9. `Lax` is the same type with one clause broken.
    macro_rules! digit {
        ($name:ident, $limit:expr, $($attr:meta),*) => {
            #[derive(Clone, Copy, Debug, PartialEq, Serialize)]
            $(#[$attr])*
            struct $name(u8);
            impl $name {
                fn new(v: u8) -> Option<Self> {
                    (v <= $limit).then_some(Self(v))
                }
            }
            impl TryFrom<u8> for $name {
                type Error = &'static str;
                fn try_from(v: u8) -> Result<Self, &'static str> {
                    Self::new(v).ok_or("out of range")
                }
            }
            impl From<$name> for u8 {
                fn from(d: $name) -> u8 {
                    d.0
                }
            }
        };
    }

    digit!(Good, 9, derive(Deserialize), serde(try_from = "u8"));
    // Serde bypass: `Deserialize` is derived on the private field.
    digit!(Bypass, 9,);
    // `new` accepts one more than the type documents.
    digit!(Lax, 10, derive(Deserialize), serde(try_from = "u8"));

    impl<'de> Deserialize<'de> for Bypass {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            u8::deserialize(d).map(Self)
        }
    }

    fn contract<T>(new: fn(u8) -> Option<T>, get: fn(T) -> u8) -> RangeContract<'static, T, u8> {
        RangeContract {
            new,
            get,
            of: None,
            valid: &[0, 9],
            invalid: &[10, 255],
        }
    }

    #[test]
    fn a_type_that_keeps_the_contract_passes() {
        check_range_contract(&contract(Good::new, |g| g.0));
    }

    /// Mutation: `Good::new` limit 9 to 10 (`Lax`) fails with "new accepted 10".
    #[test]
    #[should_panic(expected = "new accepted 10")]
    fn a_new_that_accepts_too_much_fails() {
        check_range_contract(&contract(Lax::new, |g| g.0));
    }

    /// Mutation: the `Deserialize` that skips `new` fails with "deserialize
    /// accepted 10".
    #[test]
    #[should_panic(expected = "deserialize accepted 10")]
    fn a_serde_bypass_fails() {
        check_range_contract(&contract(Bypass::new, |g| g.0));
    }
}
