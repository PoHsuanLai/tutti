//! Unit newtypes for DSP and transport parameters.
//!
//! Each newtype is `#[repr(transparent)]` around its raw float type (`f32` by
//! default, `f64` for wide-range values like `SampleRate`) — zero-cost at
//! runtime, but distinct at compile time so `Hz` and `Seconds` cannot be
//! swapped by accident. The `Unit` marker trait carries the raw type as an
//! associated type so `Param<U>` can be generic over the unit.

/// Marker trait implemented by every unit newtype.
///
/// `Raw` is the underlying float representation. Most units use `f32`;
/// `SampleRate` uses `f64` because sample rates need a wider range and
/// precision than `f32` comfortably provides.
pub trait Unit: Copy + Clone + core::fmt::Debug + PartialEq {
    type Raw: Copy;
    fn from_raw(v: Self::Raw) -> Self;
    fn to_raw(self) -> Self::Raw;
}

macro_rules! unit_newtype {
    // Default form: f32-backed newtype.
    ($(#[$m:meta])* $name:ident) => {
        unit_newtype!($(#[$m])* $name, f32);
    };

    // Parameterized form: choose the raw float type.
    ($(#[$m:meta])* $name:ident, $raw:ty) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Debug, PartialEq, Default)]
        pub struct $name(pub $raw);

        impl $name {
            #[inline]
            pub const fn new(v: $raw) -> Self { Self(v) }

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

        impl From<$raw> for $name {
            #[inline]
            fn from(v: $raw) -> Self { Self(v) }
        }

        impl From<$name> for $raw {
            #[inline]
            fn from(v: $name) -> $raw { v.0 }
        }

        impl core::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

unit_newtype!(
    /// Frequency in Hertz. Used for filter cutoffs, LFO rates, EQ centers.
    Hz
);
unit_newtype!(
    /// Time in seconds. Used for attack/release, delay times, lookahead.
    Seconds
);
unit_newtype!(
    /// Amplitude in decibels. Used for thresholds, gains, makeup, ceilings.
    Db
);
unit_newtype!(
    /// Unitless normalized amount. Used for mix (0..1), feedback (0..~0.99),
    /// depth, LFO amplitude, and similar ratio-of-range controls.
    Linear
);
unit_newtype!(
    /// Dimensionless ratio. Used for compressor ratio and filter Q.
    Ratio
);
unit_newtype!(
    /// Angle in degrees. Used for spatial azimuth and elevation.
    Degrees
);
unit_newtype!(
    /// Tempo in beats per minute. Distinct from `Hz`: beats/minute, not cycles/second.
    ///
    /// `f64`-backed because every real consumer widens tempo to f64 for beat math
    /// (beats ↔ seconds conversions with sample-accurate precision), and f32 BPM
    /// would silently round-trip through the widening conversion at every read.
    Bpm,
    f64
);
unit_newtype!(
    /// Pitch offset in semitones. 12 semitones = 1 octave.
    Semitones
);
unit_newtype!(
    /// Pitch offset in cents. 100 cents = 1 semitone.
    Cents
);
/// Audio sample rate in Hertz. `f64`-backed because sample rates routinely
/// exceed `f32`'s integer-precision range (e.g., 192_000) and are used in
/// time arithmetic where precision matters.
///
/// Re-exported from `fundsp-tutti` so the [`AudioNode`](fundsp::audionode::AudioNode)
/// and [`AudioUnit`](fundsp::audiounit::AudioUnit) trait surfaces (which live
/// below `tutti-core` in the dependency graph) can take this same nominal
/// type. This crate cannot define its own copy because `tutti-core` depends
/// on `fundsp-tutti`, not the other way around.
pub use fundsp::params::SampleRate;

impl Unit for SampleRate {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.0
    }
}
