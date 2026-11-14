//! [`ranged!`](crate::ranged!), the one way to write a range-checked type.

/// Defines a range-checked type: a raw value its `new` accepts and never
/// clamps, in the `MidiChannel` shape, and its refused-raw error.
///
/// It emits the type (`Clone, Copy, Debug, PartialEq`, a private field), its
/// error carrying the refused raw (derives `thiserror::Error`, so the calling
/// crate depends on `thiserror`), and `new(raw) -> Option`, a const `of` that
/// panics for a refused literal, `get`, `TryFrom<Raw>` agreeing with `new`, and
/// `From<Self> for Raw`. `$valid` is the range as a `const` expression over the
/// raw named by `$raw`; a NaN or an infinity must fail it.
///
/// It emits no serde. A type that is stored adds it among the attributes,
/// through `try_from` so a document cannot hold a refused raw:
/// `#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(try_from = "Raw", into = "Raw"))]`.
/// Test every type with `tutti_fixtures::check_range_contract`.
///
/// ```
/// tutti_types::ranged! {
///     /// How many voices, `1..=8`.
///     Voices(u8), VoicesOutOfRange, "within 1..=8", |raw| raw >= 1 && raw <= 8
/// }
///
/// assert_eq!(Voices::new(3).map(Voices::get), Some(3));
/// assert_eq!(Voices::new(0), None);
/// assert_eq!(Voices::try_from(9), Err(VoicesOutOfRange(9)));
/// ```
#[macro_export]
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
            #[doc = concat!("The value, or `None` unless the raw is ", $what, ". Never clamps.")]
            #[inline]
            #[must_use]
            pub const fn new($raw: $inner) -> Option<Self> {
                if $valid { Some(Self($raw)) } else { None }
            }

            /// The value for a literal or a constant. For a value that
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
