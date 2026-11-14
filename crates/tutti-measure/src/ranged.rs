//! The range-checked settings of the analysers.
//!
//! Each type holds one raw value its `new` accepts and never clamps, so a
//! config field of that type cannot hold a value its analyser refuses.

/// Defines a range-checked setting: the type, its refused-raw error, and the
/// range-checked shape (`new`, `of`, `get`, `TryFrom`, `From<Self>`).
///
/// `$valid` is the range as a `const` expression over the raw named by
/// `$raw`; a NaN or infinity must fail it.
macro_rules! ranged {
    (
        $(#[$meta:meta])*
        $name:ident($inner:ty), $err:ident, $what:literal, |$raw:ident| $valid:expr
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name($inner);

        #[doc = concat!("A raw value that is not ", $what, ".")]
        #[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
        #[error("{0:?} is not {what}", what = $what)]
        pub struct $err(pub $inner);

        impl $name {
            #[doc = concat!("The setting, or `None` unless the raw is ", $what, ". Never clamps.")]
            #[inline]
            #[must_use]
            pub const fn new($raw: $inner) -> Option<Self> {
                if $valid { Some(Self($raw)) } else { None }
            }

            /// The setting for a literal or a constant. For a value that
            /// arrives from outside, use [`new`](Self::new).
            ///
            /// # Panics
            ///
            /// When the raw is refused by [`new`](Self::new); in a `const` that
            /// is a compile error.
            #[inline]
            #[must_use]
            #[track_caller]
            pub const fn of(raw: $inner) -> Self {
                match Self::new(raw) {
                    Some(value) => value,
                    None => panic!(concat!("not ", $what)),
                }
            }

            /// The raw value.
            #[inline]
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl TryFrom<$inner> for $name {
            type Error = $err;

            #[inline]
            fn try_from(raw: $inner) -> core::result::Result<Self, $err> {
                Self::new(raw).ok_or($err(raw))
            }
        }

        impl From<$name> for $inner {
            #[inline]
            fn from(value: $name) -> $inner {
                value.0
            }
        }
    };
}

pub(crate) use ranged;
