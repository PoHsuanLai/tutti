// `unit!` defines one quantity: a `#[repr(transparent)]` newtype and the
// abilities listed after the colon, and nothing else. A unit has exactly the
// abilities it names, so the list is the type's whole algebra; what a type
// declines is written beside it, as a comment, with the method that replaces it.
//
//     unit! {
//         /// docs
//         pub struct Db(pub f32): Unit, Ordered, Bounded, Additive, Signed;
//     }
//
// The struct form picks the representation from the raw type. `f32` and `f64`
// give a float quantity: derives `Copy, Clone, Debug, PartialEq, Default`,
// serde `transparent` under `serde`, `Reflect` under `bevy` (registered under
// the path `tutti_types::value::units`, which scene files name, whichever file
// defines the type). Any other raw type
// gives an integer quantity: those derives plus `Eq, PartialOrd, Ord, Hash`,
// serde `transparent`, no reflection.
//
// Each ability below is one `(@ability Name ..)` arm, with the traits or
// methods it gives. `Affine(Dur)` is the only one that takes an argument.

macro_rules! unit {
    // Float quantities.
    (
        $(#[$m:meta])* $v:vis struct $name:ident($fv:vis f32): $($abilities:tt)*
    ) => {
        unit!(@float $(#[$m])* $v struct $name($fv f32): $($abilities)*);
    };
    (
        $(#[$m:meta])* $v:vis struct $name:ident($fv:vis f64): $($abilities:tt)*
    ) => {
        unit!(@float $(#[$m])* $v struct $name($fv f64): $($abilities)*);
    };
    (
        @float
        $(#[$m:meta])* $v:vis struct $name:ident($fv:vis $raw:ty):
        $($ability:ident $(($arg:ident))?),* $(,)?;
    ) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Debug, PartialEq, Default)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(transparent))]
        #[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
        #[cfg_attr(feature = "bevy", type_path = "tutti_types::value::units")]
        $v struct $name($fv $raw);
        $(unit!(@ability $ability $name, $raw $(, $arg)?);)*
    };

    // Integer quantities.
    (
        $(#[$m:meta])* $v:vis struct $name:ident($fv:vis $raw:ty):
        $($ability:ident $(($arg:ident))?),* $(,)?;
    ) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(transparent))]
        $v struct $name($fv $raw);
        $(unit!(@ability $ability $name, $raw $(, $arg)?);)*
    };

    // `new`, `get`, `Unit`, and the `From` and `Display` abilities below.
    (@ability Unit $name:ident, $raw:ty) => {
        impl $name {
            /// Wraps a raw float already denominated in this unit.
            ///
            /// No conversion is performed — see the type's own docs for the
            /// range it expects and for any named converter that reaches it
            /// from another unit.
            #[inline]
            pub const fn new(v: $raw) -> Self { Self(v) }

            /// Returns the raw float, discarding the unit.
            ///
            /// Reach for this at a boundary the unit types do not cross (C ABI,
            /// IPC, WIT), not to do arithmetic the type deliberately omits.
            #[inline]
            pub const fn get(self) -> $raw { self.0 }
        }

        impl Unit for $name {
            type Raw = $raw;
            #[inline]
            fn from_raw(v: $raw) -> Self { Self(v) }
            #[inline]
            fn to_raw(self) -> $raw { self.0 }
        }

        unit!(@ability From $name, $raw);
        unit!(@ability Display $name, $raw);
    };

    // `From<raw>` and `From<Self> for raw`.
    (@ability From $name:ident, $raw:ty) => {
        impl From<$raw> for $name {
            #[inline]
            fn from(v: $raw) -> Self { Self(v) }
        }

        impl From<$name> for $raw {
            #[inline]
            fn from(v: $name) -> $raw { v.0 }
        }
    };

    // `Display`, as the raw number.
    (@ability Display $name:ident, $raw:ty) => {
        impl core::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Display::fmt(&self.0, f)
            }
        }
    };

    // `PartialOrd` by magnitude, not `Ord`: a float's `NaN` denies totality, as
    // it does for the raw number. (Integer quantities derive `Ord`.)
    (@ability Ordered $name:ident, $raw:ty) => {
        impl PartialOrd for $name {
            #[inline]
            fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
                self.0.partial_cmp(&other.0)
            }
        }
    };

    // `min`, `max` and `clamp` in the unit's own type, replacing the
    // `Unit(x.get().clamp(a, b))` idiom.
    (@ability Bounded $name:ident, $raw:ty) => {
        impl $name {
            /// Returns the smaller of the two, in this unit.
            #[inline]
            #[must_use]
            pub fn min(self, other: Self) -> Self {
                Self(<$raw>::min(self.0, other.0))
            }
            /// Returns the larger of the two, in this unit.
            #[inline]
            #[must_use]
            pub fn max(self, other: Self) -> Self {
                Self(<$raw>::max(self.0, other.0))
            }
            /// Clamps into `lo..=hi`, in this unit.
            ///
            /// # Panics
            ///
            /// Panics if `lo > hi`, matching `f32::clamp` / `f64::clamp`.
            #[inline]
            #[must_use]
            pub fn clamp(self, lo: Self, hi: Self) -> Self {
                Self(<$raw>::clamp(self.0, lo.0, hi.0))
            }
        }
    };

    // A magnitude that composes with itself: `Add`, `Sub`, `AddAssign` and
    // `SubAssign`, `T ± T -> T`.
    (@ability Additive $name:ident, $raw:ty) => {
        impl core::ops::Add for $name {
            type Output = Self;
            #[inline]
            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }
        impl core::ops::Sub for $name {
            type Output = Self;
            #[inline]
            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }
        impl core::ops::AddAssign for $name {
            #[inline]
            fn add_assign(&mut self, rhs: Self) {
                self.0 += rhs.0;
            }
        }
        impl core::ops::SubAssign for $name {
            #[inline]
            fn sub_assign(&mut self, rhs: Self) {
                self.0 -= rhs.0;
            }
        }
    };

    // A signed quantity, where negation reverses its direction: `Neg`.
    (@ability Signed $name:ident, $raw:ty) => {
        impl core::ops::Neg for $name {
            type Output = Self;
            #[inline]
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }
    };

    // Scaling by a bare scalar: `Mul<raw>`, `Div<raw>` and their assigns.
    // Never for a logarithmic unit.
    (@ability Scalable $name:ident, $raw:ty) => {
        impl core::ops::Mul<$raw> for $name {
            type Output = Self;
            #[inline]
            fn mul(self, k: $raw) -> Self {
                Self(self.0 * k)
            }
        }
        impl core::ops::Div<$raw> for $name {
            type Output = Self;
            #[inline]
            fn div(self, k: $raw) -> Self {
                Self(self.0 / k)
            }
        }
        impl core::ops::MulAssign<$raw> for $name {
            #[inline]
            fn mul_assign(&mut self, k: $raw) {
                self.0 *= k;
            }
        }
        impl core::ops::DivAssign<$raw> for $name {
            #[inline]
            fn div_assign(&mut self, k: $raw) {
                self.0 /= k;
            }
        }
    };

    // The ratio of two commensurable magnitudes, a bare number:
    // `Div<Self> -> raw`.
    (@ability Ratio $name:ident, $raw:ty) => {
        impl core::ops::Div for $name {
            type Output = $raw;
            #[inline]
            fn div(self, rhs: Self) -> $raw {
                self.0 / rhs.0
            }
        }
    };

    // Modular remainder within a repeating span: `Rem`.
    (@ability Modular $name:ident, $raw:ty) => {
        impl core::ops::Rem for $name {
            type Output = Self;
            #[inline]
            fn rem(self, rhs: Self) -> Self {
                Self(self.0 % rhs.0)
            }
        }
    };

    // The affine relationship between a position and its displacement `$dur`:
    // `pos - pos`, `pos ± dur` and their assigns. There is no `pos + pos`:
    // adding two positions has no meaning without an origin.
    (@ability Affine $pos:ident, $raw:ty, $dur:ident) => {
        impl core::ops::Sub<$pos> for $pos {
            type Output = $dur;
            #[inline]
            fn sub(self, rhs: $pos) -> $dur {
                $dur(self.0 - rhs.0)
            }
        }
        impl core::ops::Add<$dur> for $pos {
            type Output = $pos;
            #[inline]
            fn add(self, rhs: $dur) -> $pos {
                $pos(self.0 + rhs.0)
            }
        }
        impl core::ops::Sub<$dur> for $pos {
            type Output = $pos;
            #[inline]
            fn sub(self, rhs: $dur) -> $pos {
                $pos(self.0 - rhs.0)
            }
        }
        impl core::ops::AddAssign<$dur> for $pos {
            #[inline]
            fn add_assign(&mut self, rhs: $dur) {
                self.0 += rhs.0;
            }
        }
        impl core::ops::SubAssign<$dur> for $pos {
            #[inline]
            fn sub_assign(&mut self, rhs: $dur) {
                self.0 -= rhs.0;
            }
        }
    };
}
