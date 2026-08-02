//! Unit newtypes for DSP and transport parameters.
//!
//! Each newtype is `#[repr(transparent)]` around its raw float type (`f32` by
//! default, `f64` for wide-range values like `Bpm`/`Beat`) — zero-cost at
//! runtime, but distinct at compile time so `Hz` and `Seconds` cannot be
//! swapped by accident. The [`Unit`] marker trait carries the raw type as an
//! associated type so [`Param`](super::Param) can be generic over the unit.
//!
//! [`SampleRate`] is defined here too, and re-exported by `fundsp-tutti` for
//! the `AudioNode` / `AudioUnit` trait surfaces that take one. It used to be
//! defined *there* — the direction is what changed, not the type.

// `Seconds::to_samples*` lands in the frame-count vocabulary, which is
// integer-backed and lives next door in `samples.rs` rather than being one of
// the float units defined here.
use super::samples::Samples;

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
        // `transparent` so a unit serializes as its bare number: a `Beat` is
        // `1.5` on the wire, not `[1.5]` or `{"0":1.5}`. Feature-gated like every
        // other serde derive here, and applied in the macro so a wire-carried
        // type built from any unit (`MeterChange`, which holds a `Beat`) does not
        // have to special-case which units happen to have it.
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(transparent))]
        // Reflection is field-walking, not an ECS opinion: a unit is what a
        // parameter *is*, so a component holding one has to be inspectable for
        // an editor or a scene to round-trip it. That is a weaker claim than
        // `Component`, which no unit gets — an app stores a `Volume`, never a
        // bare `Db`. Applied in the macro for the same reason `serde` is: a type
        // built from any unit should not have to care which ones happen to
        // reflect.
        #[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
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

// ── Operator opt-ins ────────────────────────────────────────────────────────
//
// Deliberately NOT part of `unit_newtype!`. A blanket set would generate
// operators that compile but mean nothing — `Beat + Beat` (adding two timeline
// positions), `Db * 2.0` (which squares the amplitude, it does not double the
// gain), `Degrees(350) < Degrees(10)` (false on a circle). Each type opts into
// exactly the algebra it has, next to its own definition, and what is *omitted*
// is as load-bearing as what is included.

/// Ordering by magnitude.
///
/// `PartialOrd` only: these are float-backed, so `NaN` denies totality exactly
/// as it does for the raw `f64`. (`Samples` has full `Ord` because it is
/// integer-backed.)
macro_rules! unit_ordered {
    ($name:ident) => {
        impl PartialOrd for $name {
            #[inline]
            fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
                self.0.partial_cmp(&other.0)
            }
        }
    };
}

/// `min` / `max` / `clamp` in the unit's own type, replacing the
/// `Unit(x.get().clamp(a, b))` idiom.
macro_rules! unit_bounded {
    ($name:ident, $raw:ty) => {
        impl $name {
            #[inline]
            pub fn min(self, other: Self) -> Self {
                Self(<$raw>::min(self.0, other.0))
            }
            #[inline]
            pub fn max(self, other: Self) -> Self {
                Self(<$raw>::max(self.0, other.0))
            }
            /// Panics if `lo > hi`, matching `f32::clamp` / `f64::clamp`.
            #[inline]
            pub fn clamp(self, lo: Self, hi: Self) -> Self {
                Self(<$raw>::clamp(self.0, lo.0, hi.0))
            }
        }
    };
}

/// A magnitude that composes with itself: `T ± T -> T`.
macro_rules! unit_additive {
    ($name:ident) => {
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
}

/// A signed quantity: negation reverses its direction.
macro_rules! unit_signed {
    ($name:ident) => {
        impl core::ops::Neg for $name {
            type Output = Self;
            #[inline]
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }
    };
}

/// Scaling by a bare scalar. **Never** apply to a logarithmic unit.
macro_rules! unit_scalable {
    ($name:ident, $raw:ty) => {
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
}

/// Ratio of two commensurable magnitudes — a bare number.
macro_rules! unit_ratio {
    ($name:ident, $raw:ty) => {
        impl core::ops::Div for $name {
            type Output = $raw;
            #[inline]
            fn div(self, rhs: Self) -> $raw {
                self.0 / rhs.0
            }
        }
    };
}

/// The affine relationship between a position and its displacement.
///
/// Generates exactly the operators an affine space has — and, critically, **no
/// `$pos + $pos`**: adding two positions has no meaning without an origin
/// convention. That omission is the entire point of splitting a position type
/// from a duration type.
macro_rules! unit_affine {
    ($pos:ident, $dur:ident) => {
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

/// Modular remainder within a repeating span.
macro_rules! unit_modular {
    ($name:ident) => {
        impl core::ops::Rem for $name {
            type Output = Self;
            #[inline]
            fn rem(self, rhs: Self) -> Self {
                Self(self.0 % rhs.0)
            }
        }
    };
}

unit_newtype!(
    /// Frequency in Hertz. Used for filter cutoffs, LFO rates, EQ centers.
    Hz
);
unit_ordered!(Hz);
unit_bounded!(Hz, f32);
unit_additive!(Hz);
unit_scalable!(Hz, f32);
unit_ratio!(Hz, f32);
unit_newtype!(
    /// Time in seconds. Used for attack/release, delay times, lookahead.
    Seconds
);
unit_ordered!(Seconds);
unit_bounded!(Seconds, f32);
unit_additive!(Seconds);
unit_scalable!(Seconds, f32);
unit_ratio!(Seconds, f32);

impl Seconds {
    /// Frames in this span at `sample_rate`, rounded to nearest.
    ///
    /// The *measurement* form: how long is this, in frames. Three named
    /// variants rather than one function with a rounding-mode argument —
    /// the name puts the choice in the signature instead of making a reader
    /// chase what a call site passed.
    ///
    /// Computed in `f64`: `Seconds` is `f32`, which cannot represent frame
    /// counts past 2^24 (about 6 minutes at 48 kHz), and the export planner
    /// already works in `f64`.
    #[inline]
    pub fn to_samples(self, sample_rate: impl Into<SampleRate>) -> Samples {
        Self::frames(self.0 as f64 * sample_rate.into().get(), f64::round)
    }

    /// Frames fully elapsed in this span — rounds **down**.
    ///
    /// The *counting* form: how many whole frames have gone by.
    #[inline]
    pub fn to_samples_floor(self, sample_rate: impl Into<SampleRate>) -> Samples {
        Self::frames(self.0 as f64 * sample_rate.into().get(), f64::floor)
    }

    /// Frames needed to hold this span — rounds **up**.
    ///
    /// The *allocation* form: a delay line sized for `max_delay` must hold at
    /// least that long, so rounding to nearest would under-allocate for half
    /// of all inputs.
    #[inline]
    pub fn to_samples_ceil(self, sample_rate: impl Into<SampleRate>) -> Samples {
        Self::frames(self.0 as f64 * sample_rate.into().get(), f64::ceil)
    }

    /// Shared tail: apply `round`, then clamp into `usize`.
    ///
    /// Non-finite and negative inputs collapse to zero. `NaN as usize` is
    /// already `0` in Rust and a negative cast already saturates, so this
    /// changes no behaviour — it makes the behaviour *stated* rather than
    /// inherited from a cast rule most readers do not have memorized.
    #[inline]
    fn frames(raw: f64, round: fn(f64) -> f64) -> Samples {
        if !raw.is_finite() || raw <= 0.0 {
            return Samples::ZERO;
        }
        Samples(round(raw) as usize)
    }
}
unit_newtype!(
    /// Amplitude in decibels. Used for thresholds, gains, makeup, ceilings.
    Db
);
unit_ordered!(Db);
unit_bounded!(Db, f32);
// dB is logarithmic, so cascaded gain stages ADD — unlike `Beat + Beat`, this
// is meaningful. `Neg` inverts a gain (cut <-> boost).
unit_additive!(Db);
unit_signed!(Db);
// NOT `unit_scalable!`: `Db(-6.0) * 2.0 == Db(-12.0)` squares the *amplitude*,
// it does not double the gain. Convert through `Amplitude` for amplitude scaling.
// NOT `unit_ratio!`: a quotient of logarithms is not a quantity.

impl Db {
    /// The metering floor. Silence has no logarithm, so a display needs a
    /// finite stand-in for it; `-inf` cannot be drawn on a fader.
    ///
    /// −144 dB is roughly the noise floor of 24-bit audio, so it is below
    /// anything real while staying finite.
    pub const FLOOR: Db = Db(-144.0);

    /// Unity gain.
    pub const UNITY: Db = Db(0.0);

    /// This gain as a linear amplitude multiplier.
    #[inline]
    pub fn to_amplitude(self) -> Amplitude {
        Amplitude(10.0_f32.powf(self.0 / 20.0))
    }

    /// This gain as an `f64` amplitude multiplier.
    ///
    /// Not a convenience: the loudness path (`tutti-export`'s EBU R128
    /// normalization) works in `f64` because LUFS targets are `f64`, and
    /// routing it through the `f32` form would change rendered export gain in
    /// the low bits.
    #[inline]
    pub fn to_amplitude_f64(self) -> f64 {
        10.0_f64.powf(self.0 as f64 / 20.0)
    }

    /// Amplitude as decibels, with silence pinned to [`FLOOR`](Self::FLOOR).
    ///
    /// The metering form. Two floors exist on purpose — see
    /// [`from_amplitude_exact`](Self::from_amplitude_exact). The engine had
    /// three different ones before this (`-96` in `dynamics/utils.rs`, `-144`
    /// in `loudness.rs`, and none at all elsewhere); the split here is between
    /// *display* and *arithmetic*, not between two crates' habits.
    #[inline]
    pub fn from_amplitude(amp: Amplitude) -> Db {
        if amp.0 <= 0.0 {
            Db::FLOOR
        } else {
            Db(20.0 * amp.0.log10())
        }
    }

    /// Amplitude as decibels, letting silence be `-inf`.
    ///
    /// For arithmetic that must round-trip: `to_amplitude` of `-inf` is exactly
    /// `0.0`, whereas the clamped form loses that. Use this when the value
    /// feeds further computation, and [`from_amplitude`](Self::from_amplitude)
    /// when it feeds a meter.
    #[inline]
    pub fn from_amplitude_exact(amp: Amplitude) -> Db {
        Db(20.0 * amp.0.log10())
    }

    /// An `f64` amplitude multiplier as decibels, with silence pinned to
    /// [`FLOOR`](Self::FLOOR).
    ///
    /// The inverse of [`to_amplitude_f64`](Self::to_amplitude_f64), and the
    /// metering counterpart to it. Takes a bare `f64` rather than an
    /// `Amplitude` because that is what the `f64` loudness path holds — an
    /// `Amplitude` is `f32`, so requiring one would narrow the input before
    /// the `log10` and defeat the reason the `f64` path exists.
    ///
    /// This existed only in the `to_` direction for a while, and the gap is
    /// what pushed the true-peak meter in `tutti-analysis` into hand-rolling
    /// `20.0 * linear.log10()` with its own zero guard. A converter missing
    /// its inverse gets hand-rolled, not worked around.
    #[inline]
    pub fn from_amplitude_f64(amp: f64) -> Db {
        if amp <= 0.0 {
            Db::FLOOR
        } else {
            Db((20.0 * amp.log10()) as f32)
        }
    }
}
// ── Dimensionless amounts ───────────────────────────────────────────────────
//
// Five types where there was one (`Linear`), whose own doc named four roles in
// two lines — "mix (0..1), feedback (0..~0.99), depth, LFO amplitude" — and
// then described itself as "a position on a normalized scale, not a magnitude",
// which is false of the one role it actually played in production (amplitude,
// which *is* a magnitude and routinely exceeds 1.0).
//
// The roles differ in both range and algebra, which is the module's own test
// for whether two quantities are the same type:
//
//   Amplitude  [0, inf)   scalable      a gain multiplier
//   Mix        [0, 1]     NOT scalable  a blend position between two signals
//   Feedback   [0, 0.99]  NOT scalable  a recirculation coefficient
//   Depth      [-1, 1]    signed, additive, scalable
//   Drive      [0, inf)   NOT scalable  shaper-curve intensity
//
// `Mix` declines scaling because half of a blend position is not a blend
// position; `Feedback` declines it because scaling a coefficient walks it
// through the stability bound; `Drive` declines it because it is never
// multiplied onto a signal at all.

unit_newtype!(
    /// A linear gain multiplier.
    ///
    /// **Not** a 0..1 quantity: a +6 dB peak is `Amplitude(2.0)`, and the
    /// dynamics detectors routinely see values above unity. The only floor is
    /// zero — [`SILENT`](Self::SILENT).
    Amplitude
);
unit_ordered!(Amplitude);
unit_bounded!(Amplitude, f32);
unit_scalable!(Amplitude, f32);
// NOT `unit_additive!`: cascading two gain stages MULTIPLIES them. Summing
// amplitudes is what mixing two signals does, and that is the signals' job,
// not the gains'.

impl Amplitude {
    /// Silence.
    pub const SILENT: Amplitude = Amplitude(0.0);
    /// Unity gain — the signal passes unchanged.
    pub const UNITY: Amplitude = Amplitude(1.0);

    /// This amplitude in decibels, with silence pinned to [`Db::FLOOR`].
    #[inline]
    pub fn to_db(self) -> Db {
        Db::from_amplitude(self)
    }
}

unit_newtype!(
    /// A blend position between two signals, `0..1`. 0 is fully dry, 1 fully
    /// wet.
    ///
    /// A *position on a scale*, not a magnitude — which is why it does not
    /// scale: half of a 50% blend is not a meaningful quantity, and
    /// `mix * 0.5` reads like it dims the wet signal when it actually moves
    /// the crossfade point.
    Mix
);
unit_ordered!(Mix);
unit_bounded!(Mix, f32);
// NOT `unit_scalable!` / `unit_additive!`: see the type doc. Two blends
// summing to 1.4 is off the end of the crossfade.

impl Mix {
    /// Fully dry — none of the processed signal.
    pub const DRY: Mix = Mix(0.0);
    /// Fully wet — none of the original signal.
    pub const WET: Mix = Mix(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Mix {
        Mix(v).clamp(Self::DRY, Self::WET)
    }

    /// Crossfade `dry` and `wet` at this position.
    ///
    /// The operation every consumer hand-wrote as
    /// `dry * (1.0 - mix) + wet * mix`.
    #[inline]
    pub fn blend(self, dry: f32, wet: f32) -> f32 {
        dry * (1.0 - self.0) + wet * self.0
    }
}

unit_newtype!(
    /// A feedback (recirculation) coefficient.
    ///
    /// Must stay below 1.0 or the loop it feeds grows without bound — a
    /// delay line at unity feedback never decays. [`MAX_STABLE`](Self::MAX_STABLE)
    /// is the ceiling every consumer used to spell as a bare `0.99`.
    Feedback
);
unit_ordered!(Feedback);
unit_bounded!(Feedback, f32);
// NOT `unit_scalable!`: scaling a coefficient walks it across the stability
// bound with no check. `new_clamped` is the only way in.
// NOT `unit_additive!`: two feedback paths summing past 1.0 is exactly the
// runaway this type exists to prevent — see the note on cross-feedback below.

impl Feedback {
    /// No recirculation.
    pub const NONE: Feedback = Feedback(0.0);

    /// The largest coefficient that still decays.
    ///
    /// 0.99 — repeated as a bare literal at 13 sites before this constant
    /// existed, including two audio-rate modulation paths where the value
    /// arrives off an input port and never passes through a constructor.
    pub const MAX_STABLE: Feedback = Feedback(0.99);

    /// Constrain into the stable range.
    #[inline]
    pub fn new_clamped(v: f32) -> Feedback {
        Feedback(v).clamp(Self::NONE, Self::MAX_STABLE)
    }

    /// Constrain a *pair* of coefficients that feed the same loop.
    ///
    /// Cross-coupled delays add their direct and cross terms into one
    /// recirculation (`in_l + fb_l·fb + fb_r·cross`), so clamping each to
    /// [`MAX_STABLE`] independently still permits a combined 1.98 and a
    /// runaway. This scales the pair down together when their sum would
    /// exceed the bound, preserving their ratio.
    #[inline]
    pub fn stable_pair(direct: f32, cross: f32) -> (Feedback, Feedback) {
        let d = direct.max(0.0);
        let c = cross.max(0.0);
        let total = d + c;
        if total <= Self::MAX_STABLE.0 {
            return (Feedback(d), Feedback(c));
        }
        let scale = Self::MAX_STABLE.0 / total;
        (Feedback(d * scale), Feedback(c * scale))
    }
}

unit_newtype!(
    /// A bipolar modulation amount, `-1..1`.
    ///
    /// Signed on purpose: a negative depth inverts the modulator, so an LFO at
    /// `Depth(-1.0)` is the same shape phase-flipped. That is the whole reason
    /// this is not just an [`Amplitude`] — and the reason it is additive and
    /// scalable while `Mix` and `Feedback` are not.
    Depth
);
unit_ordered!(Depth);
unit_bounded!(Depth, f32);
unit_additive!(Depth);
unit_signed!(Depth);
unit_scalable!(Depth, f32);

impl Depth {
    /// No modulation.
    pub const NONE: Depth = Depth(0.0);
    /// Full positive modulation.
    pub const FULL: Depth = Depth(1.0);
    /// Full inverted modulation.
    pub const INVERTED: Depth = Depth(-1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Depth {
        Depth(v).clamp(Self::INVERTED, Self::FULL)
    }
}

unit_newtype!(
    /// Waveshaper drive — how hard a signal is pushed into a nonlinearity.
    ///
    /// Distinct from [`Amplitude`] despite sharing its `[0, inf)` range,
    /// because it is **not a multiplier on the signal**: it selects or
    /// parameterizes a shaping curve. Typing it as a gain would license
    /// `sample * drive`, which is meaningless for a curve selector.
    Drive
);
unit_ordered!(Drive);
unit_bounded!(Drive, f32);
// NOT `unit_scalable!`: "twice the drive" is not twice anything — the curve's
// response is nonlinear by construction.
// NOT `unit_additive!`: two drives do not sum.

impl Drive {
    /// No overdrive — the shaper passes the signal through.
    pub const UNITY: Drive = Drive(1.0);
}

unit_newtype!(
    /// Spatial diffusion, `0..1`: how widely a point source is smeared across
    /// a speaker array. 0 is a point, 1 is fully diffuse.
    ///
    /// Not a [`Mix`] — it blends no pair of signals; it is a geometric
    /// property of the panning solution, and the VBAP panner consumes it as
    /// such. Sharing `Mix`'s range is not sharing its meaning.
    Spread
);
unit_ordered!(Spread);
unit_bounded!(Spread, f32);

impl Spread {
    /// A point source — no diffusion.
    pub const POINT: Spread = Spread(0.0);
    /// Fully diffuse across the array.
    pub const DIFFUSE: Spread = Spread(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Spread {
        Spread(v).clamp(Self::POINT, Self::DIFFUSE)
    }
}

unit_newtype!(
    /// Mid/side stereo width. `0` collapses to mono, `1` leaves the image
    /// unchanged, and above `1` widens it past the source.
    ///
    /// Not an [`Amplitude`] despite the matching range: it scales the *side*
    /// component against the mid, so it reshapes the stereo image rather than
    /// making the signal louder. Typing it as a gain would invite it into
    /// signal multiplications where it does not belong.
    StereoWidth
);
unit_ordered!(StereoWidth);
unit_bounded!(StereoWidth, f32);

impl StereoWidth {
    /// Collapsed to mono.
    pub const MONO: StereoWidth = StereoWidth(0.0);
    /// The source image, unchanged.
    pub const NATURAL: StereoWidth = StereoWidth(1.0);

    /// Constrain to non-negative. Deliberately no upper bound — widening past
    /// the source is a legitimate effect.
    #[inline]
    pub fn new_clamped(v: f32) -> StereoWidth {
        StereoWidth(v.max(0.0))
    }
}

// ── Dimensionless ratios ────────────────────────────────────────────────────
//
// Three types where there was one (`Ratio`, "used for compressor ratio and
// filter Q"). They share only the property of being bare numbers; their ranges
// do not overlap and one cannot be substituted for another:
//
//   CompressionRatio  [1, inf)  1:1 is no compression; below 1 would EXPAND
//   Q                 (0, inf)  filter sharpness; 0.707 is flat, high is a
//                               narrow peak
//   Resonance         [0, 1]    ladder feedback; 1.0 self-oscillates
//
// `Q` and `Resonance` are the sharp case: both describe "how resonant", but a
// ladder at 1.0 self-oscillates while a Q of 1.0 is a mild bell. Passing one
// where the other belongs is silent and sounds like a mistuned filter.

unit_newtype!(
    /// Compressor ratio: input dB over threshold per 1 dB of output.
    ///
    /// `1.0` is no compression. Below 1.0 would be an *expander*, which this
    /// type deliberately does not represent — the compressor's setter clamps
    /// up to 1.0 rather than silently inverting its own behaviour.
    CompressionRatio
);
unit_ordered!(CompressionRatio);
unit_bounded!(CompressionRatio, f32);
// NOT `unit_additive!`: 4:1 plus 4:1 is not 8:1.
// NOT `unit_scalable!`: doubling a ratio is not doubling anything audible —
// the mapping from ratio to gain reduction is logarithmic.

impl CompressionRatio {
    /// No compression.
    pub const UNITY: CompressionRatio = CompressionRatio(1.0);

    /// Constrain to a real compression ratio.
    #[inline]
    pub fn new_clamped(v: f32) -> CompressionRatio {
        CompressionRatio(v.max(1.0))
    }
}

unit_newtype!(
    /// Filter quality factor — how sharply a filter resonates at its cutoff.
    ///
    /// `0.707` (1/sqrt2) is the maximally-flat Butterworth response; higher is
    /// a narrower, more peaked resonance. Must stay above zero: `Q` divides
    /// into the filter's damping term.
    Q
);
unit_ordered!(Q);
unit_bounded!(Q, f32);
unit_scalable!(Q, f32);
// NOT `unit_additive!`: two Q values do not sum into a third.

impl Q {
    /// Butterworth — the maximally flat response, `1/sqrt(2)`.
    pub const BUTTERWORTH: Q = Q(core::f32::consts::FRAC_1_SQRT_2);

    /// Constrain above zero, since `Q` divides into the damping term.
    #[inline]
    pub fn new_clamped(v: f32) -> Q {
        Q(v.max(f32::MIN_POSITIVE))
    }
}

unit_newtype!(
    /// Ladder-filter resonance, `0..1` — the normalized feedback around the
    /// filter's four poles.
    ///
    /// Distinct from [`Q`] even though both mean "how resonant": at `1.0` a
    /// ladder self-oscillates, whereas `Q(1.0)` is a mild bell. They are
    /// different parameterizations of different topologies, and swapping them
    /// is silent.
    Resonance
);
unit_ordered!(Resonance);
unit_bounded!(Resonance, f32);
// NOT `unit_scalable!` / `unit_additive!`: as with `Feedback`, scaling walks
// the value toward self-oscillation with no check.

impl Resonance {
    /// No resonance.
    pub const NONE: Resonance = Resonance(0.0);
    /// The self-oscillation threshold.
    pub const SELF_OSCILLATION: Resonance = Resonance(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Resonance {
        Resonance(v).clamp(Self::NONE, Self::SELF_OSCILLATION)
    }
}

// ── Measurements ────────────────────────────────────────────────────────────
//
// Everything above is a *control*: a value the caller sets on a processor.
// These three are *readings*: values the engine measures and reports back.
//
// That distinction is what separates them from the controls they resemble.
// `Correlation` and `Pan` share `Depth`'s range and most of its algebra, and
// `Confidence` shares `Mix`'s range — but a meter's phase coherence is not an
// LFO's modulation amount, and swapping them is silent. This is the same
// argument `Q` and `Resonance` settle by splitting: two quantities that
// describe different things stay different types even when their numbers line
// up.

unit_newtype!(
    /// How certain an estimator is of its own answer, `0..1`. 0 is a guess,
    /// 1 is certainty.
    ///
    /// Not a [`Mix`] despite the shared range — nothing is being blended, and
    /// `Mix::blend`, the reason that type exists, is meaningless here. Not an
    /// [`Amplitude`] either: a confidence is never multiplied onto a signal,
    /// and giving it a gain's algebra would license `sample * confidence`,
    /// silently turning an uncertain pitch estimate into a volume dip.
    Confidence
);
unit_ordered!(Confidence);
unit_bounded!(Confidence, f32);
// Scalable because the smoothers genuinely attenuate it: a Viterbi track
// penalizes a jump by multiplying the confidence by a decay factor.
unit_scalable!(Confidence, f32);
// NOT `unit_additive!`: two estimates being 0.6 confident does not make
// anything 1.2 confident. Combining evidence is `combine`, which states the
// independence precondition that multiplying them requires.

impl Confidence {
    /// No information — the estimator is guessing.
    pub const NONE: Confidence = Confidence(0.0);
    /// Certain.
    pub const CERTAIN: Confidence = Confidence(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Confidence {
        Confidence(v).clamp(Self::NONE, Self::CERTAIN)
    }

    /// Two *independent* estimates of the same fact, combined as a joint
    /// probability.
    ///
    /// Named rather than `Mul` because the multiplication is only correct when
    /// the estimates are independent, and the name is where that precondition
    /// is stated.
    #[inline]
    pub fn combine(self, other: Confidence) -> Confidence {
        Confidence(self.0 * other.0)
    }
}

unit_newtype!(
    /// Phase coherence between two channels, `-1..1`. `+1` is identical
    /// (mono-compatible), `0` uncorrelated, `-1` polarity-inverted.
    ///
    /// Shares [`Depth`]'s range and sign convention but not its meaning: a
    /// modulation depth is a control you set, a correlation is a measurement
    /// reported back. Feeding a correlation meter into an LFO's depth is not a
    /// plausible operation, so the compiler should refuse it.
    ///
    /// The negative half is the whole point — `-1` is the mono-compatibility
    /// failure a correlation meter exists to catch, and it is not "less" than
    /// `+1` in any audible sense; it is the opposite fault.
    Correlation
);
unit_ordered!(Correlation);
unit_bounded!(Correlation, f32);
unit_signed!(Correlation);
// NOT `unit_scalable!`: the value is a normalized inner product, so rescaling
// denormalizes it and the result no longer means "coherence".
// NOT `unit_additive!`: two correlations do not sum. Averaging one over time
// is meter ballistics, which smooths rather than accumulates.

impl Correlation {
    /// Identical channels — fully mono-compatible.
    pub const MONO: Correlation = Correlation(1.0);
    /// Uncorrelated channels.
    pub const UNCORRELATED: Correlation = Correlation(0.0);
    /// Polarity-inverted — cancels completely when summed to mono.
    pub const INVERTED: Correlation = Correlation(-1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Correlation {
        Correlation(v).clamp(Self::INVERTED, Self::MONO)
    }

    /// Whether summing to mono would audibly cancel.
    ///
    /// Named because the threshold is a convention, and a bare comparison at
    /// the call site hides that it is one.
    #[inline]
    pub fn has_phase_issues(self) -> bool {
        self.0 < -0.3
    }

    /// The mid/side width this coherence implies.
    ///
    /// A derivation, not a second field: storing both lets a smoother move one
    /// without the other and leave the pair contradicting itself.
    #[inline]
    pub fn to_stereo_width(self) -> StereoWidth {
        StereoWidth(1.0 - self.0)
    }
}

unit_newtype!(
    /// Position on the left/right axis, `-1..1`. `-1` is hard left, `0`
    /// center, `+1` hard right.
    ///
    /// A *position*, like [`Azimuth`] — but on a segment rather than a circle,
    /// so it saturates at the ends instead of wrapping, and unlike `Azimuth`
    /// it orders (left really is less than right).
    ///
    /// Distinct from [`Depth`] for the reason this section exists: a pan
    /// position and a modulation amount are different quantities that happen
    /// to share a range.
    Pan
);
unit_ordered!(Pan);
unit_bounded!(Pan, f32);
unit_signed!(Pan);
// NOT `unit_scalable!`: half of a pan position is not a pan position — the
// same argument `Mix` makes. Moving a source is `Azimuth::lerp_shortest`'s
// job on the circle, and interpolation here is a `lerp`, not a `*`.
// NOT `unit_additive!`: two pan positions do not sum into a third.

impl Pan {
    /// Hard left.
    pub const LEFT: Pan = Pan(-1.0);
    /// Centered — equal in both channels.
    pub const CENTER: Pan = Pan(0.0);
    /// Hard right.
    pub const RIGHT: Pan = Pan(1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Pan {
        Pan(v).clamp(Self::LEFT, Self::RIGHT)
    }
}

unit_newtype!(
    /// How hard a note was struck or released, `0..1`.
    ///
    /// Normalized rather than a 7-bit code because MIDI 2.0's field is 16-bit:
    /// storing `u8` would quantize to 128 steps permanently, and the widening
    /// back out is not a multiply (see [`Velocity::to_midi2_u16`]). Note-on and
    /// note-off velocity are the same quantity, which is why one type serves
    /// both.
    ///
    /// **Not an [`Amplitude`]**, despite `0..1` and the temptation. A velocity
    /// is never multiplied onto a signal: it selects a sample layer, sets an
    /// envelope's peak, and opens a filter — a synth may be *louder* at low
    /// velocity if that is how it is programmed. Giving it a gain's algebra
    /// would license `sample * velocity`, silently turning a soft note into a
    /// volume dip. Same argument [`Confidence`] makes.
    ///
    /// **Not a [`Mix`]**: nothing is being blended, and `Mix::blend` — the
    /// reason that type exists — is meaningless here. **Not a [`Depth`]**: that
    /// is bipolar, and there is no such thing as negative velocity.
    Velocity
);
unit_ordered!(Velocity);
unit_bounded!(Velocity, f32);
// NOT `unit_scalable!`: `velocity * 0.5` reads as "half as loud", which is
// exactly the amplitude confusion this type exists to prevent — the mapping
// from velocity to loudness is the instrument's, not arithmetic's. Scaling a
// performance is `Velocity::scaled`, which says what it does.
// NOT `unit_additive!`: two velocities do not sum into a third.

impl Velocity {
    /// Silence — a note-on at zero velocity is a note-off by MIDI convention,
    /// which `tutti_midi_types::normalize` folds at the wire edge.
    pub const SILENT: Velocity = Velocity(0.0);
    /// The hardest strike.
    pub const MAX: Velocity = Velocity(1.0);
    /// MIDI 2.0's center code (`0x8000`) — mezzo-forte, and the value a
    /// controller sending no velocity data should be treated as.
    pub const CENTER: Velocity = Velocity(0.5);

    /// Constrain into `0..=1`.
    #[inline]
    pub fn new_clamped(v: f32) -> Velocity {
        Velocity(v).clamp(Self::SILENT, Self::MAX)
    }

    /// Scale a whole performance's dynamics, staying in range.
    ///
    /// The named replacement for the omitted `Mul<f32>`: this is the one
    /// operation that is genuinely a rescale (a "soften the take" edit), and
    /// naming it keeps the *loudness* reading — which is the instrument's job,
    /// not arithmetic's — from being spelled the same way.
    #[inline]
    pub fn scaled(self, factor: f32) -> Velocity {
        Velocity::new_clamped(self.0 * factor)
    }
}

// ── Angles ──────────────────────────────────────────────────────────────────
//
// Three types where there was one (`Degrees`), because a circle and a segment
// are not the same space and the single type could not tell them apart.
//
// The old type omitted `Ord` and `Add` for exactly the right reason — the
// comment named `shortest_arc_to` and `wrap_signed` as the replacements — but
// those methods were never written. With the operators gone and no replacement,
// every call site escaped to raw `f32`, and two real bugs shipped in the escape
// (a saturating clamp on a wrapping coordinate, and a smoother that sweeps 340
// degrees to travel 20). The omissions below therefore ship *with* their
// replacements; that is the rule this split exists to establish.
//
// The split itself is the second half of the fix. `Azimuth` wraps and
// `Elevation` saturates, so the correct clamp for one is the shipped bug for
// the other — and under a single `Degrees` those two call sites are textually
// identical. Now they cannot be confused, because the wrong one no longer
// compiles.

unit_newtype!(
    /// A bearing on the horizontal circle, in degrees. 0 = front, positive =
    /// counter-clockwise, canonical range `-180..180`.
    ///
    /// A *wrapping* coordinate. There is no ordering (`Azimuth(170) <
    /// Azimuth(-170)` is a meaningless question — they are 20 degrees apart)
    /// and no `clamp` (constraining a bearing to a range is what
    /// [`wrap`](Self::wrap) does correctly and what a saturating clamp does
    /// wrongly). Displacements are [`ArcDegrees`].
    Azimuth
);
// `Neg` is meaningful — mirroring a bearing across the front axis, which is how
// a left/right flip is expressed. Everything else is a named method:
// ordering and `clamp` are traps on a circle (see the type doc), and `+`/`-`
// belong to `ArcDegrees` via `rotate_by` / `shortest_arc_to`.
unit_signed!(Azimuth);

impl Azimuth {
    /// Directly ahead.
    pub const FRONT: Azimuth = Azimuth(0.0);

    /// The equivalent bearing in the canonical `-180..180` range.
    ///
    /// This is the operation a saturating `clamp` gets wrong: 190 degrees is
    /// 170 degrees to the *right* (`-170`), not the extreme left (`180`).
    /// Uses `rem_euclid`, not `%` — plain `%` preserves the dividend's sign, so
    /// it leaves negative inputs outside the range it is supposed to enforce.
    #[inline]
    pub fn wrap(self) -> Azimuth {
        Azimuth((self.0 + 180.0).rem_euclid(360.0) - 180.0)
    }

    /// The shortest signed rotation from `self` to `to`, in `-180..=180`.
    ///
    /// The replacement for `to - self`. A plain subtraction across the seam
    /// gives the long way around: from 170 to -170 it reports -340 degrees
    /// rather than the correct +20.
    #[inline]
    pub fn shortest_arc_to(self, to: Azimuth) -> ArcDegrees {
        ArcDegrees(Azimuth(to.0 - self.0).wrap().0)
    }

    /// Rotate by a signed arc, wrapping. The replacement for `+`.
    #[inline]
    pub fn rotate_by(self, arc: ArcDegrees) -> Azimuth {
        Azimuth(self.0 + arc.0).wrap()
    }

    /// Interpolate toward `to` along the *short* arc, `t` in `0..=1`.
    ///
    /// The replacement for `a + (b - a) * t`, which takes the long way around
    /// the seam and is audible as a panner sweeping the wrong direction.
    #[inline]
    pub fn lerp_shortest(self, to: Azimuth, t: f32) -> Azimuth {
        self.rotate_by(self.shortest_arc_to(to) * t)
    }
}

unit_newtype!(
    /// Height above the horizontal plane, in degrees. `-90` = directly below,
    /// `0` = level, `+90` = directly overhead.
    ///
    /// A *saturating* coordinate, and this is the whole reason it is a separate
    /// type from [`Azimuth`]. The poles are endpoints, not a seam: tilting past
    /// straight up does not continue over the top and come out behind you at
    /// this layer, it stops. So ordering and `clamp` — traps on a circle — are
    /// exactly right here.
    Elevation
);
unit_ordered!(Elevation);
unit_bounded!(Elevation, f32);
unit_signed!(Elevation);
// NOT `unit_additive!`: `Elevation + Elevation` is a position sum with no
// origin, and it would also bypass the pole clamp. `tilt_by` is the addition.

impl Elevation {
    /// Directly below.
    pub const DOWN: Elevation = Elevation(-90.0);
    /// The horizontal plane.
    pub const LEVEL: Elevation = Elevation(0.0);
    /// Directly overhead.
    pub const UP: Elevation = Elevation(90.0);

    /// Constrain to the pole-to-pole range. Saturating, unlike
    /// [`Azimuth::wrap`] — the distinction the split exists to enforce.
    #[inline]
    pub fn new_clamped(v: f32) -> Elevation {
        Elevation(v).clamp(Self::DOWN, Self::UP)
    }

    /// Tilt by a signed arc, clamped at the poles. The replacement for `+`.
    #[inline]
    pub fn tilt_by(self, arc: ArcDegrees) -> Elevation {
        Elevation::new_clamped(self.0 + arc.0)
    }

    /// Straight-line interpolation, `t` in `0..=1`. Correct here precisely
    /// because there is no seam to cross.
    #[inline]
    pub fn lerp(self, to: Elevation, t: f32) -> Elevation {
        Elevation::new_clamped(self.0 + (to.0 - self.0) * t)
    }
}

unit_newtype!(
    /// A signed angular *displacement* in degrees — the difference between two
    /// angles, not an angle itself.
    ///
    /// The partner to [`Azimuth`] and [`Elevation`]: those name where something
    /// is, this names how far to turn. Being a displacement, it has the full
    /// algebra its positions cannot have — two rotations compose, and a
    /// rotation scales.
    ArcDegrees
);
unit_ordered!(ArcDegrees);
unit_bounded!(ArcDegrees, f32);
unit_additive!(ArcDegrees);
unit_signed!(ArcDegrees);
unit_scalable!(ArcDegrees, f32);
// Deliberately NOT `unit_affine!(Azimuth, ArcDegrees)`, though the pair looks
// exactly like `Beat`/`BeatDuration` and the macro would expand cleanly. The
// generated `+` and `-` are plain float arithmetic with no wrap, which is the
// precise bug class this split was made to eliminate — `Azimuth(170) +
// ArcDegrees(20)` would give 190, a bearing outside the canonical range that
// then compares and clamps wrongly forever after. `rotate_by` and
// `shortest_arc_to` are the wrapping forms, and they are the only forms.
//
// `Elevation` is left out for a different reason: its `+` must clamp at the
// poles, which `unit_affine!` also does not do. `tilt_by` is that form.

// ── Oscillator phase ────────────────────────────────────────────────────────
//
// Phase is measured in *turns* — one full cycle is 1.0, not 360 and not 2·pi.
// That choice is why `Phase` is its own type rather than an `Azimuth`: a
// modulator's shape table is indexed by turns, and the conversion to radians
// happens once, at the `sin`/`cos` call.
//
// The engine had three different phase wraps before this type existed, and two
// of them were wrong for negative input:
//
//   `%`        (lfo.rs)        keeps the dividend's sign
//   `.fract()` (modulator.rs)  keeps the dividend's sign
//   `rem_euclid` (driver.rs)   correct
//
// A negative phase escapes the `[0, 1)` range every consumer assumes, and the
// consumers do not check — a reverse-rate LFO or a negative phase offset reads
// off the front of a shape table. `Phase` performs exactly one wrap,
// `rem_euclid`, and offers no other.

unit_newtype!(
    /// Oscillator phase in *turns*: `0.0` starts a cycle, `1.0` completes it.
    ///
    /// Always in `[0, 1)` — [`wrapped`](Self::wrapped) is the only constructor
    /// that can be trusted with arbitrary input, and it is the only wrap this
    /// type performs.
    ///
    /// Turns rather than radians because that is what a shape table is indexed
    /// by; [`to_radians`](Self::to_radians) converts at the trig call.
    Phase
);
// NOT `unit_bounded!`: clamping a phase is never the right answer — a phase
// past the end of a cycle belongs at the *start* of the next one, not pinned to
// 0.999. `wrapped` is the constraint.
// NOT `unit_additive!`: `Phase + Phase` is two positions summed, with the same
// no-origin problem as `Beat + Beat`. Advancing is `advance(PhaseIncrement)`.
// NOT `unit_scalable!`: scaling a position depends on where zero is.
// NOT `unit_modular!`: it generates `%`, which is one of the two wrong wraps
// this type exists to eliminate.
unit_ordered!(Phase);

impl Phase {
    /// The start of a cycle.
    pub const START: Phase = Phase(0.0);

    /// Wrap any value into `[0, 1)`.
    ///
    /// `rem_euclid`, not `%` and not `.fract()`: both of those keep the sign of
    /// the input, so `-0.25` stays `-0.25` instead of becoming `0.75`. That is
    /// the whole bug class — a negative phase indexes off the front of a shape
    /// table, and no consumer checks for it.
    #[inline]
    pub fn wrapped(v: f32) -> Phase {
        Phase(v.rem_euclid(1.0))
    }

    /// Advance by one step, wrapping. The replacement for `+`.
    ///
    /// Correct for negative increments too, which is what a reverse-rate
    /// modulator produces.
    #[inline]
    pub fn advance(self, d: PhaseIncrement) -> Phase {
        Phase::wrapped(self.0 + d.0)
    }

    /// Shift by a constant offset, wrapping — an LFO's phase-offset control.
    #[inline]
    pub fn offset_by(self, offset: PhaseIncrement) -> Phase {
        self.advance(offset)
    }

    /// This phase in radians, `[0, 2·pi)` — for the `sin`/`cos` call.
    #[inline]
    pub fn to_radians(self) -> Radians {
        Radians(self.0 * core::f32::consts::TAU)
    }
}

unit_newtype!(
    /// A per-sample phase step, in turns. The displacement partner to
    /// [`Phase`].
    ///
    /// Signed: a negative increment runs a modulator backwards, which is
    /// exactly the case the old `%`/`.fract()` wraps got wrong.
    PhaseIncrement
);
unit_ordered!(PhaseIncrement);
unit_bounded!(PhaseIncrement, f32);
unit_additive!(PhaseIncrement);
unit_signed!(PhaseIncrement);
unit_scalable!(PhaseIncrement, f32);
// Deliberately NOT `unit_affine!(Phase, PhaseIncrement)`, for the same reason
// `Azimuth` declines it: the generated `+` does not wrap, and an unwrapped
// phase is the bug. `advance` is the wrapping form.

impl PhaseIncrement {
    /// The step that completes `frequency` cycles per second at `sample_rate`.
    ///
    /// Computed in f64 and narrowed once at the end: at 20 Hz against 192 kHz
    /// the step is ~1.04e-4, and accumulating an f32-rounded version of that
    /// drifts audibly over a long note.
    #[inline]
    pub fn per_sample(frequency: Hz, sample_rate: impl Into<SampleRate>) -> PhaseIncrement {
        let sample_rate = sample_rate.into().get();
        if sample_rate <= 0.0 {
            return PhaseIncrement(0.0);
        }
        PhaseIncrement((frequency.0 as f64 / sample_rate) as f32)
    }
}

unit_newtype!(
    /// An angle in radians — the argument `sin`/`cos` actually take.
    ///
    /// Distinct from [`Phase`] (turns) and [`Azimuth`] (degrees) because all
    /// three are bare `f32` at the call site, and feeding one where another is
    /// expected is silent: the oscillator keeps running, just at the wrong
    /// rate. It sounds like detuning, not like a bug.
    Radians
);
unit_additive!(Radians);
unit_signed!(Radians);
unit_scalable!(Radians, f32);
// NOT `unit_ordered!` / `unit_bounded!`, for the reason `Azimuth` gives: a
// circle has no ends, so `Radians(6.2) < Radians(0.1)` is false while the two
// are a tenth of a turn apart, and `clamp` saturates a coordinate that should
// have wrapped. `wrapped_signed` is the replacement — wrap first, then the
// comparison is against a magnitude and `.get()` is honest.
//
// `Radians` was the one angular type the file exempted from its own rule, and
// the predicted escape happened: the phase vocoder hand-rolled the missing wrap
// in raw `f32`. That is the `Degrees` failure verbatim, which is why the
// omission ships with the method rather than after it.

impl Radians {
    /// Full circle, `2·pi`.
    pub const TAU: Radians = Radians(core::f32::consts::TAU);

    /// `sin` of this angle.
    #[inline]
    pub fn sin(self) -> f32 {
        self.0.sin()
    }

    /// `cos` of this angle.
    #[inline]
    pub fn cos(self) -> f32 {
        self.0.cos()
    }

    /// The equivalent [`Phase`] in turns, wrapped into `[0, 1)`.
    #[inline]
    pub fn to_phase(self) -> Phase {
        Phase::wrapped(self.0 / core::f32::consts::TAU)
    }

    /// Wrapped into `[-pi, pi)` — the signed-deviation form.
    ///
    /// Half-open at `+pi`, which lands on `-pi`: the same convention
    /// [`Phase::wrapped`] uses at the top of its cycle, and the same reason —
    /// the two are one point on the circle, and a closed interval at both ends
    /// would have to pick one arbitrarily per call.
    ///
    /// The counterpart to [`Azimuth::wrap`] for the radian coordinate, and the
    /// answer to "how far apart are these two angles" once the difference has
    /// been taken: a phase-vocoder deviation of `+3pi/2` is really `-pi/2`, and
    /// keeping the larger number makes a partial jump the wrong way.
    ///
    /// Arithmetic rather than a `while` loop, which is what the phase vocoder
    /// needed: the loop costs one iteration per turn of input, so a phase
    /// accumulated over minutes of audio stalls the audio thread for an
    /// unbounded time. Wrapping *before* comparing is also why [`Ord`] on this
    /// type is a trap — see the omission note below.
    #[inline]
    pub fn wrapped_signed(self) -> Radians {
        let tau = core::f32::consts::TAU;
        Radians(self.0 - tau * ((self.0 + core::f32::consts::PI) / tau).floor())
    }
}

impl From<Azimuth> for Radians {
    #[inline]
    fn from(a: Azimuth) -> Radians {
        Radians(a.0.to_radians())
    }
}

impl From<Elevation> for Radians {
    #[inline]
    fn from(e: Elevation) -> Radians {
        Radians(e.0.to_radians())
    }
}

unit_newtype!(
    /// Tempo in beats per minute. Distinct from `Hz`: beats/minute, not cycles/second.
    ///
    /// `f64`-backed because every real consumer widens tempo to f64 for beat math
    /// (beats ↔ seconds conversions with sample-accurate precision), and f32 BPM
    /// would silently round-trip through the widening conversion at every read.
    Bpm,
    f64
);
unit_ordered!(Bpm);
unit_bounded!(Bpm, f64);
// NOT `unit_additive!`: the difference of two tempos is not a tempo, and
// returning `Bpm` would let it be fed to a beat-rate calculation. Use
// `differs_from` for the "did the tempo move?" test.
impl Bpm {
    /// Whether this tempo differs from `other` by more than `epsilon` BPM.
    ///
    /// The transport clock re-derives its per-sample beat increment only when
    /// the tempo actually moved; comparing floats for equality would re-derive
    /// on every buffer from ULP noise.
    #[inline]
    pub fn differs_from(self, other: Bpm, epsilon: f64) -> bool {
        (self.0 - other.0).abs() > epsilon
    }
}
unit_newtype!(
    /// A position on the musical timeline, in beats.
    ///
    /// A *position*, not a duration — beat 4.0 is where the fifth beat starts,
    /// not "four beats long". Distinct from [`Bpm`], which is a rate.
    ///
    /// `f64`-backed for the same reason as `Bpm`: an `f32` cannot resolve
    /// sub-beat detail past ~beat 16384 (its ULP exceeds 0.002 beats), which is
    /// audible as automation stair-stepping in a long session. `TransportClock`
    /// splits the beat across two `f32` ports precisely to dodge that; the
    /// scalar form must not reintroduce it.
    Beat,
    f64
);
unit_ordered!(Beat);
unit_bounded!(Beat, f64);
unit_affine!(Beat, BeatDuration);
// NOT `unit_additive!`: `Beat + Beat` has no meaning without an origin.
// NOT `unit_scalable!`: scaling a position depends on where zero is — scale the
// duration and add it.
// NOT `unit_modular!`: `beat % len` silently assumes the cycle starts at beat 0,
// which invites an off-by-a-loop-start bug. The real operation is
// `(beat - start) % len + start`, where the origin is explicit.
impl Beat {
    /// The start of the beat this position falls in.
    #[inline]
    pub fn floor(self) -> Beat {
        Beat(self.0.floor())
    }

    /// How far into the current beat this position is — a *displacement* from
    /// [`floor`](Self::floor), not a position.
    #[inline]
    pub fn fract(self) -> BeatDuration {
        BeatDuration(self.0 - self.0.floor())
    }

    /// Un-wrapped cycle position of a beat-synced periodic source — the one
    /// place beats-per-cycle is interpreted.
    ///
    /// The divisor is a span, not a frequency: doubling it makes the source
    /// *slower*. Substituting an `Hz` is a silent reciprocal, which shipped
    /// twice before this was centralized.
    ///
    /// Un-wrapped so a stepped shape can read its cycle index off the integer
    /// part. `None` for a non-positive span — a frozen source holds its phase
    /// offset instead.
    #[inline]
    pub fn cycles_of(self, beats_per_cycle: BeatDuration) -> Option<f64> {
        (beats_per_cycle.0 > 0.0).then(|| self.0 / beats_per_cycle.0)
    }
}
unit_newtype!(
    /// Pitch offset in semitones. 12 semitones = 1 octave.
    Semitones
);
unit_ordered!(Semitones);
unit_bounded!(Semitones, f32);
unit_additive!(Semitones);
unit_signed!(Semitones);
unit_scalable!(Semitones, f32);
unit_newtype!(
    /// Pitch offset in cents. 100 cents = 1 semitone.
    Cents
);
unit_ordered!(Cents);
unit_bounded!(Cents, f32);
unit_additive!(Cents);
unit_signed!(Cents);
unit_scalable!(Cents, f32);

impl Cents {
    /// One semitone.
    pub const SEMITONE: Cents = Cents(100.0);

    /// This offset in semitones.
    ///
    /// A real converter, not `self / 100.0`: `unit_scalable!(Cents, f32)` is
    /// opted in above, so `cents / 100.0` compiles and returns **`Cents`** —
    /// a value wrong by 100x whose *type* says it is fine. Sites that divide
    /// the raw `f32` today are correct by luck; this is the form that stays
    /// correct once they hold the typed value.
    #[inline]
    pub fn to_semitones(self) -> Semitones {
        Semitones(self.0 / 100.0)
    }

    /// This offset as a frequency multiplier. 1200 cents doubles the pitch.
    #[inline]
    pub fn to_pitch_ratio(self) -> f32 {
        2.0_f32.powf(self.0 / 1200.0)
    }

    /// The interval a frequency multiplier spans. Inverse of
    /// [`to_pitch_ratio`](Self::to_pitch_ratio).
    ///
    /// A named method rather than a `From`, because it is not total: a
    /// non-positive ratio has no logarithm. Those give [`Cents(0.0)`](Cents) —
    /// the unison a caller building a scale degree wants — rather than a NaN
    /// that reaches a frequency table before anyone notices.
    #[inline]
    pub fn from_pitch_ratio(ratio: f32) -> Cents {
        if ratio > 0.0 {
            Cents(1200.0 * ratio.log2())
        } else {
            Cents(0.0)
        }
    }
}

impl Semitones {
    /// One octave.
    pub const OCTAVE: Semitones = Semitones(12.0);

    /// This offset in cents. The inverse of [`Cents::to_semitones`], and
    /// likewise a converter rather than a scalar multiply.
    #[inline]
    pub fn to_cents(self) -> Cents {
        Cents(self.0 * 100.0)
    }

    /// This offset as a frequency multiplier. 12 semitones doubles the pitch.
    #[inline]
    pub fn to_pitch_ratio(self) -> f32 {
        2.0_f32.powf(self.0 / 12.0)
    }

    /// The interval a frequency multiplier spans. Inverse of
    /// [`to_pitch_ratio`](Self::to_pitch_ratio), and the semitone twin of
    /// [`Cents::from_pitch_ratio`].
    ///
    /// Named rather than a `From` for that method's reason: it is not total,
    /// because a non-positive ratio has no logarithm. Those give
    /// [`Semitones(0.0)`](Semitones) — a unison — rather than a NaN that
    /// reaches a note table before anyone notices.
    ///
    /// This existed only in the `Cents` form, so the one caller that wanted
    /// semitones (`Note::nearest_to`) hand-rolled `12.0 * ratio.log2()` in raw
    /// `f32` — with its own positivity check several lines away from the
    /// logarithm. That is the omission rule's own failure mode: the inverse
    /// was added for one unit and not its twin.
    #[inline]
    pub fn from_pitch_ratio(ratio: f32) -> Semitones {
        if ratio > 0.0 {
            Semitones(12.0 * ratio.log2())
        } else {
            Semitones(0.0)
        }
    }
}

// `From` for the conversions with exactly one answer.
//
// The rule this file follows, stated once here because the split was
// previously accidental — `Azimuth`/`Elevation` had `From<_> for Radians`
// while every other conversion was a named method, for no reason anyone
// recorded:
//
// A conversion is `From` when it is total, lossless, and there is only one
// sensible result. It stays a *named method* when any of those fails:
//
//   - It needs another input. `Seconds::to_samples` takes a `SampleRate`,
//     `BeatDuration::to_seconds` takes a `Bpm`. Not two-type conversions.
//   - The answer is not unique. `Amplitude` → `Db` has three
//     (`from_amplitude` pins silence at `Db::FLOOR`, `from_amplitude_exact`
//     lets it be `-inf`, `from_amplitude_f64` takes the wider input). A
//     `From` impl would have to pick one and apply it silently at every
//     `.into()`, which is the ambiguity the three names exist to expose.
//   - The target is a bare scalar. `Cents::to_pitch_ratio` and
//     `Semitones::to_pitch_ratio` both land on `f32`; as `From` impls they
//     would be two different meanings of one target type.
//
// The impls below delegate to the inherent methods rather than repeating the
// arithmetic, so there is one implementation and two spellings.

impl From<Cents> for Semitones {
    #[inline]
    fn from(c: Cents) -> Semitones {
        c.to_semitones()
    }
}

impl From<Semitones> for Cents {
    #[inline]
    fn from(s: Semitones) -> Cents {
        s.to_cents()
    }
}

impl From<Phase> for Radians {
    #[inline]
    fn from(p: Phase) -> Radians {
        p.to_radians()
    }
}

impl From<Correlation> for StereoWidth {
    #[inline]
    fn from(c: Correlation) -> StereoWidth {
        c.to_stereo_width()
    }
}

// ── Sample rates ────────────────────────────────────────────────────────────

unit_newtype!(
    /// Audio sample rate in Hertz.
    ///
    /// `f64`-backed because sample rates routinely exceed `f32`'s
    /// integer-precision range (192 kHz is exact, but `1.0 / 48_000.0` in `f32`
    /// carries ~6e-8 of relative error, and a phase accumulator integrating
    /// that over a long render drifts audibly).
    ///
    /// Also used at the `AudioNode` / `AudioUnit` trait surfaces in
    /// `fundsp-tutti`, which re-exports it. It lived *there* until this crate
    /// grew the unit vocabulary: the trait taking the parameter must see the
    /// type, and `fundsp-tutti` depends on this crate, so a definition here was
    /// only possible once a re-export replaced the definition. That is why the
    /// module doc no longer carries a "one unit defined elsewhere" exception.
    ///
    /// Builds from `f64` and from `u32` (file headers, device configs) — both
    /// widen exactly. Deliberately **not** from `f32`: that widening is
    /// lossless in isolation, but an `f32` rate has nearly always already lost
    /// precision upstream, and an implicit conversion would launder it in
    /// silently. A site that genuinely holds one writes `SampleRate(x as f64)`,
    /// where review can see it.
    SampleRate,
    f64
);
unit_ordered!(SampleRate);
unit_bounded!(SampleRate, f64);

// Deliberately absent, and the omissions are load-bearing:
//
// - `Add` / `Sub` (`unit_additive!`) — 44.1 kHz plus 48 kHz is not a rate.
// - `Mul<f64>` / `Div<f64>` (`unit_scalable!`) — a scaled rate is a *different
//   device rate*; re-derive it from the device rather than scaling a stale one.
// - `Div<SampleRate> -> f64` (`unit_ratio!`) — that quotient is `SrcRatio`, and
//   `SrcRatio::for_rates` owns the derivation along with its unity tolerance
//   and its non-positive guard. A bare operator here would let callers bypass
//   both.
//
// `Mul<f64>`'s replacement is `nyquist` / `nyquist_scaled` below, per the rule
// that an omission ships with the method that supersedes it. Without them the
// scaling escaped to raw floats and was written four different ways —
// `sample_rate * 0.499`, `sample_rate as f32 * 0.45`, `sample_rate.get() / 2.0`
// — each an unlabelled anti-aliasing margin.

impl SampleRate {
    /// 44.1 kHz — the historic CD-audio rate.
    pub const SR_44K1: Self = Self(44_100.0);

    /// 48 kHz — the typical pro-audio default.
    pub const SR_48K: Self = Self(48_000.0);

    /// The Nyquist frequency: the highest representable at this rate.
    ///
    /// Returns [`Hz`] because the result is a *signal* frequency — something to
    /// compare a cutoff against — not a clock rate. That crossing is the whole
    /// reason the method exists: it is the one sanctioned bridge between the
    /// two, so a filter clamping its cutoff does not reach for a raw float.
    ///
    /// Narrowing to `Hz`'s `f32` is safe here: Nyquist is at most ~96 kHz for
    /// any real device rate, well inside `f32`'s exact-integer range.
    #[inline]
    pub fn nyquist(self) -> Hz {
        Hz((self.0 * 0.5) as f32)
    }

    /// Nyquist scaled by `margin`, for filters that must stay *below* it.
    ///
    /// A biquad or ladder evaluated exactly at Nyquist is degenerate — `tan`
    /// diverges — so cutoffs clamp slightly under. The margin is the caller's
    /// because the safe distance is topology-specific (an SVF tolerates 0.499,
    /// an all-pass chain wants more headroom); naming it as an argument keeps
    /// that a stated choice rather than a bare literal beside a multiply.
    ///
    /// `margin` is clamped to `0.0..=1.0`: above 1.0 it would return a
    /// frequency *above* Nyquist, which is the bug this guards.
    #[inline]
    pub fn nyquist_scaled(self, margin: f32) -> Hz {
        Hz((self.0 * f64::from(margin.clamp(0.0, 1.0)) * 0.5) as f32)
    }
}

// `unit_newtype!` generates `From<f64>` only. File headers and device configs
// hand out `u32`, which widens exactly, so that one stays implicit — it is what
// lets a migrated signature take `impl Into<SampleRate>` and still accept what
// its callers already hold. No `From<f32>`; see the type's doc for why.
impl From<u32> for SampleRate {
    #[inline]
    fn from(v: u32) -> Self {
        Self(f64::from(v))
    }
}

// ── Playback rates ──────────────────────────────────────────────────────────
//
// Three distinct quantities that all used to be `Ratio`, all multiplied into
// one advance. Keeping them apart is the point: `SrcRatio` is derived from the
// two sample rates and is never user intent, `PlaybackRate` is user intent that
// couples pitch to speed, and `StretchFactor` is user intent that does not.
// Multiplying the wrong pair is now a type error rather than a silent bug.

unit_newtype!(
    /// Sample-rate conversion ratio: source rate ÷ destination rate.
    ///
    /// **Derived, never user intent.** A 48 kHz file played by a 44.1 kHz
    /// session reads at `48000/44100 ≈ 1.088` source samples per output sample,
    /// so it sounds correct. Build it with [`SrcRatio::for_rates`] rather than
    /// by hand — that is the one place the derivation lives.
    ///
    /// Distinct from [`PlaybackRate`] because they answer different questions:
    /// this one asks "what does playing at the right pitch cost?", the other
    /// "how fast does the user want it?". They compose (see
    /// [`PlaybackRate::read_rate`]) but must not be confused for one another.
    SrcRatio
);
unit_ordered!(SrcRatio);
unit_bounded!(SrcRatio, f32);
// NOT `unit_scalable!` / `unit_additive!`: a conversion ratio is derived from
// two sample rates, never scaled or summed. Re-derive it instead.
impl SrcRatio {
    /// Unity — source and session agree, so no conversion.
    pub const UNITY: Self = Self(1.0);

    /// Derive the conversion ratio for a file played by a session.
    ///
    /// Returns exactly [`UNITY`](Self::UNITY) when the two rates agree to within
    /// 0.01 Hz, so the common matched-rate case is bit-exact rather than
    /// `1.0000001`. A non-positive session rate also yields unity: there is no
    /// meaningful conversion, and propagating a NaN or infinity here would
    /// poison every sample downstream.
    #[inline]
    pub fn for_rates(
        file_rate: impl Into<SampleRate>,
        session_rate: impl Into<SampleRate>,
    ) -> Self {
        let (file_rate, session_rate) = (file_rate.into().get(), session_rate.into().get());
        if session_rate <= 0.0 || (file_rate - session_rate).abs() < 0.01 {
            Self::UNITY
        } else {
            Self((file_rate / session_rate) as f32)
        }
    }
}

unit_newtype!(
    /// Varispeed: how fast a clip plays relative to its recorded rate.
    ///
    /// **Couples pitch to speed**, exactly like changing a turntable's speed —
    /// 2.0 plays twice as fast an octave up. When pitch must stay put, that is
    /// [`StretchFactor`], a different operation with different DSP.
    ///
    /// Bounded ONLY when built through [`new_clamped`](Self::new_clamped) —
    /// the generated `new` is unchecked, so the type makes the range *visible*
    /// and *shared*, it does not enforce it. Every path fed by user input or a
    /// document field must use `new_clamped`; bare `new` is for reading back a
    /// value that was already clamped on the way in (an atomic reload).
    ///
    /// The range is a resampler limit. It used to live inside one backend's
    /// setter, so the other silently accepted out-of-range speeds — same
    /// command, different audio per tier. One shared constructor is what fixed
    /// that; a newtype alone could not.
    PlaybackRate
);
unit_ordered!(PlaybackRate);
unit_bounded!(PlaybackRate, f32);
// NOT `unit_additive!`: 2× plus 2× is not 4× — rates compose by multiplication.
// NOT `unit_scalable!`: scaling by a bare `f32` is how the three rate kinds got
// multiplied together in the first place. Compose through `read_rate` instead,
// which names both operands.
impl PlaybackRate {
    /// Normal speed.
    pub const UNITY: Self = Self(1.0);

    /// Slowest supported varispeed (quarter speed).
    pub const MIN: Self = Self(0.25);

    /// Fastest supported varispeed (4× speed).
    pub const MAX: Self = Self(4.0);

    /// Build a rate, clamping into [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    ///
    /// Non-finite input yields [`UNITY`](Self::UNITY): a NaN rate would make the
    /// read position NaN and silence the clip permanently, which is far worse
    /// than ignoring the request.
    #[inline]
    pub fn new_clamped(v: f32) -> Self {
        if !v.is_finite() {
            Self::UNITY
        } else {
            Self(v.clamp(Self::MIN.0, Self::MAX.0))
        }
    }

    /// Source samples consumed per output sample: varispeed × conversion.
    ///
    /// The single composition point for the two rates. Both operands are named,
    /// so the pair cannot be swapped or one of them silently dropped — which is
    /// what happened when both were a bare `Ratio` multiplied at four separate
    /// call sites.
    #[inline]
    pub fn read_rate(self, src: SrcRatio) -> ReadRate {
        ReadRate(self.0 as f64 * src.0 as f64)
    }
}

unit_newtype!(
    /// Source samples consumed per output sample: varispeed × sample-rate
    /// conversion, the product [`PlaybackRate::read_rate`] returns.
    ///
    /// Its own type rather than a bare `f64`, on both of the rule's counts:
    ///
    /// - **Distinct algebra.** [`PlaybackRate`] and [`SrcRatio`] both omit `Mul`
    ///   precisely so the three rate kinds cannot be multiplied together
    ///   ad-hoc. This type exists *to be* multiplied — by a sample count, to
    ///   advance a read position — which is the one operation neither factor
    ///   allows. So `unit_scalable!` stays omitted here too, and
    ///   [`advance`](Self::advance) is the named replacement.
    /// - **Distinct range.** `PlaybackRate` is clamped to 0.25–4.0; `SrcRatio` is
    ///   unclamped (a 192 kHz file in a 44.1 kHz session is ≈4.35), so the
    ///   product reaches ≈17.4× — a range neither factor has.
    ///
    /// `f64`-backed, matching [`SamplePosition`]: a read position advanced a
    /// million times must not drift, and widening here is what keeps the
    /// accumulation exact.
    ///
    /// Returning it from `read_rate` closed a real hole. The gate kernel took
    /// this rate and a file sample rate as two bare `f64`s, and the two tiers
    /// split the `src_ratio` factor between those parameters *differently* —
    /// memory passed `(file_rate, speed × src_ratio)`, disk passed
    /// `(session_rate × src_ratio, speed)`. Algebraically equal, so it worked;
    /// but nothing stopped a third caller from combining them wrongly, and one
    /// already had — applying `src_ratio` twice, which the disk tier's own
    /// comment records as costing real debugging time.
    ReadRate,
    f64
);
unit_ordered!(ReadRate);
unit_bounded!(ReadRate, f64);
// NOT `unit_scalable!` / `unit_additive!`: see the type docs. The whole point is
// that the ONE legal multiplication is by a sample count, and it is named.
impl ReadRate {
    /// No resampling: one source sample per output sample.
    pub const UNITY: Self = Self(1.0);

    /// How far a read position advances over `out` output samples.
    ///
    /// The named replacement for the omitted `Mul<f64>`. Returns a
    /// [`SamplePosition`] rather than a count because that is what a caller does
    /// with it — seats or advances a fractional read cursor.
    #[inline]
    pub fn advance(self, out: Samples) -> SamplePosition {
        SamplePosition(self.0 * out.get() as f64)
    }

    /// Compose two read rates applied in series.
    ///
    /// The named replacement for the omitted `Mul<Self>`, and deliberately
    /// narrower: it composes `ReadRate` with `ReadRate` only, so the three
    /// *source* rate kinds still cannot be multiplied together ad-hoc. Each
    /// factor has already resolved its own derivation through `read_rate`, so
    /// what remains is genuine composition — two resamplings in the same signal
    /// path — rather than the ratio-mixing the omission exists to block.
    ///
    /// The motivating case: a stretched placed voice reads its source at
    /// varispeed *and* at the stretcher's rate, and the two are computed by
    /// different owners (the source knows `speed`, the filter knows the stretch
    /// factor). Hand-multiplying `.get()`s at that call site is precisely how
    /// `src_ratio` came to be applied twice once before.
    #[inline]
    pub fn then(self, other: Self) -> Self {
        Self(self.0 * other.0)
    }
}

unit_newtype!(
    /// Time-stretch factor: how much longer a clip plays, pitch unchanged.
    ///
    /// 2.0 plays twice as long at unchanged pitch — the phase-vocoder
    /// operation, not resampling. For the kind that couples pitch to duration,
    /// see [`PlaybackRate`].
    StretchFactor
);
unit_ordered!(StretchFactor);
unit_bounded!(StretchFactor, f32);
// NOT `unit_additive!` / `unit_scalable!`: same reasoning as `PlaybackRate` —
// stretch factors compose by multiplication, and a bare-scalar `*` invites
// mixing them with the other two rate kinds.
impl StretchFactor {
    /// No stretching.
    pub const UNITY: Self = Self(1.0);

    /// Shortest supported stretch (quarter length).
    pub const MIN: Self = Self(0.25);

    /// Longest supported stretch (4× length).
    pub const MAX: Self = Self(4.0);

    /// Build a factor, clamping into [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    ///
    /// Non-finite input yields [`UNITY`](Self::UNITY), for the same reason as
    /// [`PlaybackRate::new_clamped`]. As there, the raw `new` is unchecked —
    /// use this on any path carrying user input.
    #[inline]
    pub fn new_clamped(v: f32) -> Self {
        if !v.is_finite() {
            Self::UNITY
        } else {
            Self(v.clamp(Self::MIN.0, Self::MAX.0))
        }
    }
}

unit_newtype!(
    /// Absolute position within a wave, measured in samples.
    ///
    /// Fractional positions are allowed so interpolating readers can address
    /// between two integer sample indices. `f64`-backed because sample offsets
    /// into long buffers routinely exceed `f32`'s integer-precision range and
    /// the fractional part must survive sample-accurate arithmetic.
    SamplePosition,
    f64
);
unit_ordered!(SamplePosition);
unit_bounded!(SamplePosition, f64);
// Self-closed rather than paired with a duration type: `Samples`
// (samples.rs) is the frame-count vocabulary but is `usize`-backed, and
// `SamplePosition` is deliberately fractional so interpolating readers can
// address between integer indices — a `Sub` returning `Samples` would silently
// truncate.
//
// The omission ships with its replacements, per the rule: `advanced_by` is
// `pos + Samples` (exact — the fraction survives), and `to_samples_floor` /
// `to_samples_ceil` are the conversions back, named for their rounding the way
// `Seconds::to_samples*` is. An operator could not carry that choice; that is
// why this is three methods rather than an `impl Add`.
unit_additive!(SamplePosition);
unit_scalable!(SamplePosition, f64);

impl SamplePosition {
    /// The integer sample index this position falls in.
    #[inline]
    pub fn floor(self) -> SamplePosition {
        SamplePosition(self.0.floor())
    }

    /// The interpolation fraction between this index and the next.
    #[inline]
    pub fn fract(self) -> f64 {
        self.0 - self.0.floor()
    }

    /// This position advanced by a frame count.
    ///
    /// The named replacement for `SamplePosition + Samples`, which the omission
    /// note above forbids: `Samples` is `usize` and this is fractional, so an
    /// affine pairing would have to round *somewhere*, and an operator has
    /// nowhere to put the choice. A method does — advancing is exact (the
    /// fractional part survives), and the caller picks the rounding only when it
    /// converts back, with [`to_samples_floor`](Self::to_samples_floor) or
    /// [`to_samples_ceil`](Self::to_samples_ceil).
    ///
    /// The motivating case is a sampler preload window: "keep `window` frames
    /// resident starting from `entry`" is `entry.advanced_by(window)`, and
    /// hand-rolling it as `entry.0 + window.0 as f64` is how a call site loses
    /// the fraction and under-loads by a frame.
    #[inline]
    pub fn advanced_by(self, frames: Samples) -> SamplePosition {
        SamplePosition(self.0 + frames.0 as f64)
    }

    /// Whole frames fully elapsed before this position — rounds **down**.
    ///
    /// The *counting* form, mirroring [`Seconds::to_samples_floor`]. "How many
    /// complete frames are behind this cursor."
    ///
    /// Saturates at zero: a negative position has no frames behind it, and
    /// `as usize` on a negative `f64` is 0 in Rust but by saturation rather than
    /// by intent — stating it here means a caller cannot read the cast and
    /// wonder.
    #[inline]
    pub fn to_samples_floor(self) -> Samples {
        Samples(self.0.floor().max(0.0) as usize)
    }

    /// Whole frames needed to *contain* this position — rounds **up**.
    ///
    /// The *allocation* form, mirroring [`Seconds::to_samples_ceil`]. A buffer
    /// sized to hold everything up to a fractional position must include the
    /// frame that position falls inside, so rounding down under-allocates for
    /// every non-integer input.
    ///
    /// This is the one a preload window wants: the window ends mid-frame, and
    /// that frame has to be resident or the reader interpolates against data it
    /// does not have.
    #[inline]
    pub fn to_samples_ceil(self) -> Samples {
        Samples(self.0.ceil().max(0.0) as usize)
    }
}
unit_newtype!(
    /// A span on the timeline, measured in beats.
    ///
    /// Distinct from [`Beat`]: subtracting one position from another
    /// yields a duration, not a position. `f64`-backed to match beat-position
    /// precision.
    BeatDuration,
    f64
);
unit_ordered!(BeatDuration);
unit_bounded!(BeatDuration, f64);
unit_additive!(BeatDuration);
unit_signed!(BeatDuration);
unit_scalable!(BeatDuration, f64);
// `dur / dur -> f64` answers "how many of these fit", which is how a beat span
// converts to a sample count against a per-sample span.
unit_ratio!(BeatDuration, f64);
unit_modular!(BeatDuration);

impl BeatDuration {
    /// This span in seconds at `tempo`.
    ///
    /// For *duration* readouts — a clip length, a UI-facing time display. It is
    /// deliberately **not** the conversion the per-sample transport path uses:
    /// `tutti_core::transport::state::beats_per_sample` computes
    /// `(tempo / 60) / sample_rate` and documents that association as
    /// load-bearing (the offline timeline is pinned to agree with the clock
    /// sample-for-sample, and the two groupings round differently). Routing
    /// those sites through this method would silently re-associate the
    /// arithmetic. Two conversions, two call sites, on purpose.
    ///
    /// Keeping them apart is now a choice rather than a constraint: that
    /// function takes a [`SampleRate`], which this crate defines, so it *could*
    /// move here. It should not — the reason above is about the arithmetic
    /// association, not about where the types live.
    #[inline]
    pub fn to_seconds(self, tempo: Bpm) -> Seconds {
        if tempo.0 <= 0.0 {
            return Seconds(0.0);
        }
        Seconds((self.0 * 60.0 / tempo.0) as f32)
    }

    /// Magnitude, discarding direction.
    #[inline]
    pub fn abs(self) -> BeatDuration {
        BeatDuration(self.0.abs())
    }

    /// Whether this span moves forward. `LoopRange`'s non-empty invariant.
    #[inline]
    pub fn is_positive(self) -> bool {
        self.0 > 0.0
    }

    /// Always-non-negative remainder.
    ///
    /// `%` on floats keeps the sign of the dividend, so a negative overshoot
    /// would wrap *outside* the span. This is the wrapping-safe form.
    #[inline]
    pub fn rem_euclid(self, rhs: BeatDuration) -> BeatDuration {
        BeatDuration(self.0.rem_euclid(rhs.0))
    }
}

/// Lock-free [`SamplePosition`] cell, shareable with the audio thread.
///
/// Stores the `f64` position as its raw bits inside an [`AtomicU64`], so the
/// sampler no longer has to sprinkle `f64::to_bits`/`from_bits` across its call
/// sites. `load`/`store` take/return [`SamplePosition`] directly.
#[derive(Debug)]
pub struct AtomicSamplePosition(core::sync::atomic::AtomicU64);

impl AtomicSamplePosition {
    #[inline]
    pub fn new(v: SamplePosition) -> Self {
        Self(core::sync::atomic::AtomicU64::new(v.get().to_bits()))
    }

    #[inline]
    pub fn load(&self, order: core::sync::atomic::Ordering) -> SamplePosition {
        SamplePosition::new(f64::from_bits(self.0.load(order)))
    }

    #[inline]
    pub fn store(&self, v: SamplePosition, order: core::sync::atomic::Ordering) {
        self.0.store(v.get().to_bits(), order)
    }
}

impl Default for AtomicSamplePosition {
    #[inline]
    fn default() -> Self {
        Self::new(SamplePosition::default())
    }
}

/// Lock-free [`ReadRate`] cell, shareable with the audio thread.
///
/// The [`AtomicSamplePosition`] shape, for the type that advances one. It
/// exists for the same reason `ReadRate` is `f64`-backed: the rate is the term
/// a disk voice accumulates into its read position once per sample, forever, so
/// a cell that narrowed to `f32` and widened back on every read would reinstate
/// exactly the drift the width was chosen to avoid.
///
/// `Param<ReadRate>` cannot serve here — [`Param`](super::Param) is
/// `Unit<Raw = f32>` only, which is the constraint that produced the narrowing.
#[derive(Debug)]
pub struct AtomicReadRate(core::sync::atomic::AtomicU64);

impl AtomicReadRate {
    #[inline]
    pub fn new(v: ReadRate) -> Self {
        Self(core::sync::atomic::AtomicU64::new(v.get().to_bits()))
    }

    #[inline]
    pub fn load(&self, order: core::sync::atomic::Ordering) -> ReadRate {
        ReadRate::new(f64::from_bits(self.0.load(order)))
    }

    #[inline]
    pub fn store(&self, v: ReadRate, order: core::sync::atomic::Ordering) {
        self.0.store(v.get().to_bits(), order)
    }
}

impl Default for AtomicReadRate {
    #[inline]
    fn default() -> Self {
        Self::new(ReadRate::UNITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn newtype_raw_round_trip() {
        assert_eq!(SamplePosition::from_raw(123.456).to_raw(), 123.456);
        assert_eq!(Beat::from_raw(4.25).to_raw(), 4.25);
        assert_eq!(BeatDuration::from_raw(-2.5).to_raw(), -2.5);
    }

    /// The affine discipline: subtracting two positions yields a *displacement*,
    /// and only a displacement can be added back to a position.
    #[test]
    fn beat_and_beat_duration_form_an_affine_pair() {
        let a = Beat(4.0);
        let b = Beat(6.5);

        let span: BeatDuration = b - a;
        assert_eq!(span, BeatDuration(2.5));

        assert_eq!(a + span, b);
        assert_eq!(b - span, a);

        let mut cursor = Beat(0.0);
        cursor += BeatDuration(0.25);
        assert_eq!(cursor, Beat(0.25));
    }

    /// What is deliberately *absent* is as load-bearing as what is present.
    /// These must not compile; the ledger is here so a future contributor does
    /// not "helpfully" add them.
    ///
    /// - `Beat + Beat` — adding two timeline positions has no meaning.
    /// - `Beat * f64` — scaling a position depends on where zero is.
    /// - `Beat % BeatDuration` — hides the loop-start origin.
    /// - `Db * f32` — squares the amplitude rather than doubling the gain.
    /// - `Db / Db` — a quotient of logarithms is not a quantity.
    /// - `Mix + Mix`, `Mix * f32`, `Ratio + Ratio` — a blend *position* does
    ///   not compose or scale; half of a crossfade point is not a crossfade
    ///   point. (`Depth` *is* additive and scalable — that is the difference
    ///   between a position and a signed magnitude.)
    /// - `Feedback * f32` — scaling walks a coefficient across the stability
    ///   bound unchecked. `new_clamped` / `stable_pair` are the ways in.
    /// - `Amplitude + Amplitude` — cascading gains multiply; summing is what
    ///   the *signals* do, not their gains.
    /// - `Drive * f32` — a shaper's response is nonlinear, so "twice the
    ///   drive" is not twice anything.
    /// - `Azimuth < Azimuth`, `Azimuth.clamp(..)` — a wrapping coordinate has
    ///   no ordering and no saturating clamp. `wrap` is the constraint.
    /// - `Radians < Radians`, `Radians.clamp(..)` — the same, for the radian
    ///   coordinate; `wrapped_signed` is the constraint. This type carried both
    ///   until the phase vocoder hand-rolled the missing wrap in raw `f32`,
    ///   which is the `Degrees` escape reproduced exactly.
    /// - `Azimuth + ArcDegrees` — `rotate_by`, which wraps; the operator would
    ///   not. Same for `Elevation + ArcDegrees` vs `tilt_by`, which clamps.
    /// - `Azimuth - Azimuth` — `shortest_arc_to`; a plain subtraction takes the
    ///   long way around the seam.
    /// - `Phase + Phase`, `Phase * f32`, `Phase.clamp(..)` — a cycle position.
    ///   Clamping pins it at 0.999 where it belongs at the next cycle's start.
    /// - `Phase % Phase` — `%` is one of the two sign-preserving wraps this
    ///   type was introduced to eliminate. `wrapped` is the only wrap.
    /// - `Phase + PhaseIncrement` — `advance`, which wraps; the operator would
    ///   not.
    /// - `Bpm - Bpm` — a tempo difference is not a tempo.
    /// - `Confidence + Confidence` — two 0.6-confident estimates do not make a
    ///   1.2-confident one. `combine` is the replacement, and its name states
    ///   the independence precondition that multiplying them requires.
    /// - `Correlation * f32` — the value is a normalized inner product, so
    ///   rescaling denormalizes it and it stops meaning "coherence".
    /// - `Correlation + Correlation` — coherence does not accumulate;
    ///   time-averaging it is meter ballistics, which smooths.
    /// - `Pan * f32`, `Pan + Pan` — a position on a segment, so the same
    ///   argument `Mix` makes: half a pan position is not a pan position, and
    ///   two of them do not sum. Interpolation is a `lerp`, not a `*`.
    /// - `ReadRate * f64`, `ReadRate + ReadRate` — `advance`, which names the
    ///   operand as a sample count and returns a `SamplePosition`. This type
    ///   exists to be multiplied by exactly one thing, so a bare `*` would let
    ///   it be multiplied by anything — including the `PlaybackRate` and
    ///   `SrcRatio` it is already the product of, which is the double-apply bug
    ///   the disk tier's gate comment records.
    ///
    /// - `Velocity * f32` — `scaled`, which clamps. The bare operator reads as
    ///   "half as loud", and velocity→loudness is the instrument's mapping, not
    ///   arithmetic's: a patch may be louder at low velocity.
    /// - `Velocity + Velocity` — two strikes do not sum into a harder one.
    ///
    /// And five *type* omissions, which the compiler enforces rather than a
    /// missing `impl`: `Correlation` and `Pan` are not `Depth`,
    /// `Confidence` is not `Mix`, and `Velocity` is not `Amplitude`, even
    /// though the ranges coincide. A control you set and a measurement
    /// reported back are different quantities — the same split `Q` and
    /// `Resonance` make. `Velocity` is the sharpest of these, because
    /// `sample * velocity` is not just meaningless but *plausible*: it
    /// compiles as a volume dip and sounds like a quiet note.
    ///
    /// The fourth is the sharpest, because the two are *inverse*: a beat-synced
    /// rate is a [`BeatDuration`] (beats per cycle), never an [`Hz`]. Doubling
    /// the span halves the rate, so the substitution is a silent reciprocal —
    /// it shipped twice. [`Beat::cycles_of`] is the only interpreter.
    #[test]
    fn omitted_operators_are_documented() {}

    /// Velocity scales by a *named* method, and the scale clamps.
    ///
    /// `scaled` exists because a "soften the take" edit is real, and without a
    /// replacement for the omitted `Mul<f32>` every call site would escape to
    /// `Velocity(v.get() * 0.8)` — which does not clamp, and which is how a
    /// velocity leaves `0..1` and reaches the 16-bit widening out of range.
    #[test]
    fn velocity_scales_by_name_and_stays_in_range() {
        assert_eq!(Velocity(0.8).scaled(0.5), Velocity(0.4));
        assert_eq!(Velocity(0.8).scaled(4.0), Velocity::MAX);
        assert_eq!(Velocity(0.8).scaled(-1.0), Velocity::SILENT);
        assert_eq!(Velocity::new_clamped(1.7), Velocity::MAX);
    }

    /// The MIDI 2.0 center code is mezzo-forte, not "half as loud".
    ///
    /// Pinned because `CENTER` looks like an arbitrary 0.5 until you know it is
    /// the value `0x8000` widens from — the one point the spec's Min-Center-Max
    /// scaler is required to preserve exactly.
    #[test]
    fn the_center_velocity_is_the_midi_center_code() {
        assert_eq!(Velocity::CENTER, Velocity(0.5));
        assert!(Velocity::SILENT < Velocity::CENTER);
        assert!(Velocity::CENTER < Velocity::MAX);
    }

    #[test]
    fn confidence_combines_rather_than_sums() {
        // Independent estimates multiply into a joint probability — and the
        // result is never larger than either input, which `+` would not
        // preserve.
        let joint = Confidence(0.6).combine(Confidence(0.5));
        assert_eq!(joint, Confidence(0.3));
        assert!(joint <= Confidence(0.6));

        // The guard the shipped pitch detector lacked: YIN's aperiodicity can
        // leave the unit interval, and only the lower bound was clamped.
        assert_eq!(Confidence::new_clamped(1.4), Confidence::CERTAIN);
        assert_eq!(Confidence::new_clamped(-0.2), Confidence::NONE);
    }

    #[test]
    fn correlation_derives_width_rather_than_storing_it() {
        // The invariant the shipped meter broke by storing `width` alongside
        // `correlation` and smoothing the two independently.
        assert_eq!(Correlation::MONO.to_stereo_width(), StereoWidth::MONO);
        assert_eq!(
            Correlation::UNCORRELATED.to_stereo_width(),
            StereoWidth::NATURAL
        );
        assert_eq!(Correlation::INVERTED.to_stereo_width(), StereoWidth(2.0));

        // Phase trouble is a named threshold, not a bare comparison at the
        // call site.
        assert!(Correlation(-0.5).has_phase_issues());
        assert!(!Correlation(-0.1).has_phase_issues());
        assert!(!Correlation::MONO.has_phase_issues());
    }

    #[test]
    fn pan_saturates_and_orders() {
        // Unlike `Azimuth`, this is a segment: past the end it stops rather
        // than wrapping around to the opposite side.
        assert_eq!(Pan::new_clamped(1.5), Pan::RIGHT);
        assert_eq!(Pan::new_clamped(-1.5), Pan::LEFT);

        // And unlike `Azimuth`, it orders — a segment has ends.
        assert!(Pan::LEFT < Pan::CENTER);
        assert!(Pan::CENTER < Pan::RIGHT);
        assert_eq!(-Pan::LEFT, Pan::RIGHT);
    }

    #[test]
    fn azimuth_wraps_rather_than_saturating() {
        // The shipped bug this type exists to prevent: a saturating clamp maps
        // 190 degrees to 180 (hard left). It is 170 degrees to the *right*.
        assert_eq!(Azimuth(190.0).wrap(), Azimuth(-170.0));
        assert_eq!(Azimuth(-190.0).wrap(), Azimuth(170.0));

        // `rem_euclid`, not `%`: several full turns in either direction still
        // land in range, which plain `%` does not manage for negatives.
        assert_eq!(Azimuth(360.0 + 45.0).wrap(), Azimuth(45.0));
        assert_eq!(Azimuth(-720.0 - 45.0).wrap(), Azimuth(-45.0));

        // The range is half-open at +180: one endpoint, not two names for it.
        assert_eq!(Azimuth(180.0).wrap(), Azimuth(-180.0));
    }

    #[test]
    fn azimuth_takes_the_short_way_across_the_seam() {
        // The second shipped bug: `to - self` reports -340 here. The listener
        // hears the panner sweep almost all the way around to travel 20
        // degrees.
        let from = Azimuth(170.0);
        let to = Azimuth(-170.0);
        assert_eq!(from.shortest_arc_to(to), ArcDegrees(20.0));
        assert_eq!(to.shortest_arc_to(from), ArcDegrees(-20.0));

        // And the interpolator built on it crosses the seam rather than
        // retreating from it: halfway from 170 to -170 is 180, not 0.
        assert_eq!(from.lerp_shortest(to, 0.5), Azimuth(-180.0));
        assert_eq!(from.lerp_shortest(to, 0.0), from);
        assert_eq!(from.lerp_shortest(to, 1.0), to);
    }

    #[test]
    fn azimuth_rotation_stays_on_the_circle() {
        assert_eq!(Azimuth(170.0).rotate_by(ArcDegrees(20.0)), Azimuth(-170.0));
        // Mirroring across the front axis — the one operator a bearing keeps.
        assert_eq!(-Azimuth(45.0), Azimuth(-45.0));
    }

    #[test]
    fn elevation_saturates_where_azimuth_wraps() {
        // Same numbers, opposite correct answers — which is why one `Degrees`
        // could not serve both. Past the pole, elevation stops.
        assert_eq!(Elevation::new_clamped(120.0), Elevation::UP);
        assert_eq!(Elevation::new_clamped(-120.0), Elevation::DOWN);
        // Where the identical azimuth input wraps to the far side instead.
        assert_eq!(Azimuth(120.0).wrap(), Azimuth(120.0));
        assert_eq!(Azimuth(200.0).wrap(), Azimuth(-160.0));

        assert_eq!(Elevation::LEVEL.tilt_by(ArcDegrees(30.0)), Elevation(30.0));
        assert_eq!(Elevation(80.0).tilt_by(ArcDegrees(30.0)), Elevation::UP);

        // Ordering is meaningful here and absent on `Azimuth`.
        assert!(Elevation::DOWN < Elevation::LEVEL);
        assert!(Elevation::LEVEL < Elevation::UP);
    }

    #[test]
    fn elevation_lerp_needs_no_seam_handling() {
        assert_eq!(Elevation(0.0).lerp(Elevation(90.0), 0.5), Elevation(45.0));
        assert_eq!(
            Elevation(-90.0).lerp(Elevation(90.0), 0.5),
            Elevation::LEVEL
        );
    }

    #[test]
    fn phase_wrap_is_correct_for_negatives_where_the_old_ones_were_not() {
        // The two wraps this type replaces, reproduced: both keep the sign of
        // the input, so a negative phase stays negative and indexes off the
        // front of a shape table.
        assert_eq!(-0.25_f32 % 1.0, -0.25);
        assert_eq!((-0.25_f32).fract(), -0.25);
        // `rem_euclid` — the only wrap `Phase` performs.
        assert_eq!(Phase::wrapped(-0.25), Phase(0.75));

        assert_eq!(Phase::wrapped(1.25), Phase(0.25));
        assert_eq!(Phase::wrapped(0.5), Phase(0.5));
        // Many turns out, either direction, still lands in range.
        assert_eq!(Phase::wrapped(-3.25), Phase(0.75));
        assert_eq!(Phase::wrapped(7.5), Phase(0.5));
    }

    #[test]
    fn phase_advances_backwards_without_escaping_the_cycle() {
        // A reverse-rate modulator: the case a sign-preserving wrap breaks.
        let p = Phase(0.1).advance(PhaseIncrement(-0.25));
        assert_eq!(p, Phase(0.85));
        assert!((0.0..1.0).contains(&p.get()));

        // Forward across the seam.
        assert_eq!(
            Phase(0.9).advance(PhaseIncrement(0.25)),
            Phase::wrapped(1.15)
        );

        // Walking a full cycle backwards stays in range at every step.
        let mut cursor = Phase::START;
        for _ in 0..40 {
            cursor = cursor.advance(PhaseIncrement(-0.1));
            assert!((0.0..1.0).contains(&cursor.get()));
        }
    }

    #[test]
    fn phase_increment_derives_from_frequency_and_rate() {
        // One cycle per second at 100 Hz sampling = 1/100 turn per sample.
        assert_eq!(
            PhaseIncrement::per_sample(Hz(1.0), 100.0),
            PhaseIncrement(0.01)
        );
        // A degenerate rate yields a standing phase rather than inf/NaN.
        assert_eq!(
            PhaseIncrement::per_sample(Hz(440.0), 0.0),
            PhaseIncrement(0.0)
        );
    }

    /// The wrap `Radians` was missing, which the phase vocoder hand-rolled in
    /// raw `f32` — the `Degrees` escape, repeated.
    #[test]
    fn radians_wrap_signed_rather_than_saturating() {
        let pi = core::f32::consts::PI;

        // A deviation of +3pi/2 is really -pi/2. Keeping the larger number
        // moves a partial the wrong way round the circle.
        assert!((Radians(1.5 * pi).wrapped_signed().get() - (-0.5 * pi)).abs() < 1e-5);
        assert!((Radians(-1.5 * pi).wrapped_signed().get() - (0.5 * pi)).abs() < 1e-5);

        // Already inside: unchanged.
        assert_eq!(Radians(0.0).wrapped_signed(), Radians(0.0));

        // Half-open at +pi, which folds to -pi — the same point on the circle,
        // and the same convention `Phase::wrapped` uses at the top of a cycle.
        assert!((Radians(pi).wrapped_signed().get() - (-pi)).abs() < 1e-5);

        // Arithmetic, not iteration: a phase accumulated over minutes of audio
        // wraps in constant time. The `while` loop this replaced ran once per
        // turn, on the audio thread.
        let far = Radians(1.0e6).wrapped_signed().get();
        assert!((-pi..pi).contains(&far), "{far}");
    }

    #[test]
    fn turns_and_radians_convert_both_ways() {
        assert_eq!(Phase(0.5).to_radians(), Radians(core::f32::consts::PI));
        assert_eq!(Phase::START.to_radians(), Radians(0.0));
        assert_eq!(Radians::TAU.to_phase(), Phase::START);
        assert_eq!(Radians(core::f32::consts::PI).to_phase(), Phase(0.5));

        // A quarter turn is the sine peak — the check that the units line up
        // rather than being off by tau somewhere.
        assert!((Phase(0.25).to_radians().sin() - 1.0).abs() < 1e-6);
        assert!(Phase::START.to_radians().cos() - 1.0 < 1e-6);

        // Degrees reach radians too, so a panner converts once instead of
        // hand-multiplying by pi/180 at each trig call.
        assert!((Radians::from(Azimuth(180.0)).0 - core::f32::consts::PI).abs() < 1e-6);
        assert_eq!(Radians::from(Elevation::LEVEL), Radians(0.0));
    }

    #[test]
    fn db_round_trips_through_amplitude() {
        assert_eq!(Db::UNITY.to_amplitude(), Amplitude(1.0));
        // -6 dB is very nearly half amplitude.
        assert!((Db(-6.0).to_amplitude().get() - 0.501_187).abs() < 1e-5);
        // +6 dB exceeds 1.0 — amplitude is not a 0..1 quantity.
        assert!(Db(6.0).to_amplitude().get() > 1.99);

        let round = Db::from_amplitude_exact(Db(-12.0).to_amplitude());
        assert!((round.get() - -12.0).abs() < 1e-4);
    }

    #[test]
    fn the_f64_converters_are_inverses_and_share_the_metering_floor() {
        // The pair the loudness path uses. Round-tripping through f64 holds
        // tighter than the f32 form above — that precision is the whole
        // reason these two exist.
        let round = Db::from_amplitude_f64(Db(-12.0).to_amplitude_f64());
        assert!((round.get() - -12.0).abs() < 1e-5);

        // It is the metering form, so it agrees with `from_amplitude` at
        // silence rather than going to -inf.
        assert_eq!(Db::from_amplitude_f64(0.0), Db::FLOOR);
        assert_eq!(Db::from_amplitude_f64(-1.0), Db::FLOOR);

        // And agrees with the f32 form everywhere else it can be compared.
        assert!(
            (Db::from_amplitude_f64(0.5).get() - Db::from_amplitude(Amplitude(0.5)).get()).abs()
                < 1e-5
        );
    }

    #[test]
    fn from_is_the_spelling_for_single_answer_conversions() {
        // Each of these delegates to its inherent method, so the two spellings
        // cannot drift.
        assert_eq!(Semitones::from(Cents(150.0)), Cents(150.0).to_semitones());
        assert_eq!(Cents::from(Semitones(1.5)), Semitones(1.5).to_cents());
        assert_eq!(Radians::from(Phase(0.25)), Phase(0.25).to_radians());
        assert_eq!(
            StereoWidth::from(Correlation(0.25)),
            Correlation(0.25).to_stereo_width()
        );

        // And they round-trip where the pair is mutually inverse.
        assert_eq!(Cents::from(Semitones::from(Cents(1200.0))), Cents(1200.0));
    }

    /// The `From`/named-method split, as a ledger — the same discipline the
    /// operator omissions above get, and for the same reason. The previous
    /// split was accidental (two angular types had `From`, nothing else did),
    /// which is how it stayed unexamined.
    #[test]
    fn conversions_that_stay_named_and_why() {
        // Needs a second input, so it is not a two-type conversion at all.
        assert_eq!(Seconds(1.0).to_samples(SampleRate::SR_48K), Samples(48_000));

        // Not unique: three answers for one type pair. `From` would have to
        // pick one and hide the choice behind `.into()`.
        assert_eq!(Db::from_amplitude(Amplitude(0.0)), Db::FLOOR);
        assert!(Db::from_amplitude_exact(Amplitude(0.0)).get().is_infinite());
        assert_eq!(Db::from_amplitude_f64(0.0), Db::FLOOR);

        // Target is a bare scalar, and two different units convert into it —
        // as `From` impls these would be two meanings of one target type.
        assert!((Cents(1200.0).to_pitch_ratio() - 2.0).abs() < 1e-6);
        assert!((Semitones(12.0).to_pitch_ratio() - 2.0).abs() < 1e-6);

        // `ArcDegrees` has no radians conversion at all. `Azimuth` and
        // `Elevation` do (as `From`), so adding one here would look like
        // filling a gap — but nothing needs it, and an unused conversion
        // between a displacement and an angle invites treating one as the
        // other. Left absent deliberately.
    }

    #[test]
    fn the_two_db_floors_differ_only_at_silence() {
        // The metering form pins silence to a finite value, because -inf
        // cannot be drawn on a fader.
        assert_eq!(Db::from_amplitude(Amplitude(0.0)), Db::FLOOR);
        assert!(Db::from_amplitude(Amplitude(0.0)).get().is_finite());

        // The arithmetic form keeps -inf, which is what round-trips exactly:
        // 10^(-inf/20) is 0.0, while 10^(-144/20) is merely very small.
        assert!(Db::from_amplitude_exact(Amplitude(0.0)).get().is_infinite());
        assert_eq!(
            Db::from_amplitude_exact(Amplitude::SILENT).to_amplitude(),
            Amplitude::SILENT
        );
        assert!(Db::FLOOR.to_amplitude().get() > 0.0);

        // Above silence the two agree.
        assert_eq!(
            Db::from_amplitude(Amplitude(0.5)),
            Db::from_amplitude_exact(Amplitude(0.5))
        );
    }

    #[test]
    fn db_to_amplitude_f64_is_not_just_the_f32_path_widened() {
        // The loudness path works in f64 because LUFS targets are f64. The
        // wider form must actually be computed wide, or export gain shifts in
        // the low bits.
        let wide = Db(-23.0).to_amplitude_f64();
        assert!((wide - 0.070_794_578_438_413_79).abs() < 1e-15);
        assert_eq!(Db::UNITY.to_amplitude_f64(), 1.0);
    }

    #[test]
    fn seconds_to_samples_names_its_rounding() {
        // 0.5 s at 44100 is exactly 22050 frames — every variant agrees.
        assert_eq!(Seconds(0.5).to_samples(44_100.0), Samples(22_050));
        assert_eq!(Seconds(0.5).to_samples_floor(44_100.0), Samples(22_050));
        assert_eq!(Seconds(0.5).to_samples_ceil(44_100.0), Samples(22_050));

        // 10 ms at 44100 is 441.0; nudge it so the three diverge.
        let odd = Seconds(0.010_5);
        assert_eq!(odd.to_samples_floor(44_100.0), Samples(463));
        assert_eq!(odd.to_samples_ceil(44_100.0), Samples(464));
        assert_eq!(odd.to_samples(44_100.0), Samples(463));

        // Allocation must never under-size: ceil is >= nearest, always.
        for ms in 1..200 {
            let s = Seconds(ms as f32 * 0.001);
            assert!(s.to_samples_ceil(48_000.0) >= s.to_samples(48_000.0));
            assert!(s.to_samples(48_000.0) >= s.to_samples_floor(48_000.0));
        }
    }

    #[test]
    fn seconds_to_samples_collapses_nonsense_to_zero() {
        // `NaN as usize` is already 0 and a negative cast already saturates —
        // this pins the behaviour so it is stated rather than inherited.
        assert_eq!(Seconds(f32::NAN).to_samples(48_000.0), Samples::ZERO);
        assert_eq!(Seconds(f32::INFINITY).to_samples(48_000.0), Samples::ZERO);
        assert_eq!(Seconds(-1.0).to_samples(48_000.0), Samples::ZERO);
        assert_eq!(Seconds(1.0).to_samples(0.0), Samples::ZERO);
    }

    #[test]
    fn sample_position_advances_by_a_frame_count_without_losing_the_fraction() {
        // The named replacement for the forbidden `SamplePosition + Samples`.
        // Exactness is the whole point: an interpolating reader sits between two
        // integer indices, and advancing must not quietly seat it on one.
        let entry = SamplePosition(1000.5);
        assert_eq!(entry.advanced_by(Samples(8192)), SamplePosition(9192.5));
        assert_eq!(entry.advanced_by(Samples(0)), entry);
    }

    #[test]
    fn sample_position_to_samples_names_its_rounding() {
        // The distinction that makes these two methods rather than one: a
        // preload window ending mid-frame must load THAT frame, so a window
        // sized by `floor` under-loads and the reader interpolates against data
        // it does not have.
        let mid_frame = SamplePosition(9192.5);
        assert_eq!(mid_frame.to_samples_floor(), Samples(9192));
        assert_eq!(mid_frame.to_samples_ceil(), Samples(9193));

        // Exact positions agree — the rounding only bites on the fraction.
        let exact = SamplePosition(9192.0);
        assert_eq!(exact.to_samples_floor(), exact.to_samples_ceil());

        // Ordering holds for every input, which is the invariant a caller
        // reasons with when it picks one.
        for raw in [0.0, 0.25, 1.0, 1.75, 48_000.5, 1e9 + 0.5] {
            let p = SamplePosition(raw);
            assert!(p.to_samples_ceil() >= p.to_samples_floor(), "{raw}");
        }
    }

    #[test]
    fn sample_position_to_samples_collapses_nonsense_to_zero() {
        // Matches `Seconds::to_samples`'s convention rather than inventing a
        // second one: a negative or non-finite position has no frames behind it.
        assert_eq!(SamplePosition(-1.0).to_samples_floor(), Samples::ZERO);
        assert_eq!(SamplePosition(-1.0).to_samples_ceil(), Samples::ZERO);
        assert_eq!(SamplePosition(f64::NAN).to_samples_floor(), Samples::ZERO);
        assert_eq!(SamplePosition(f64::NAN).to_samples_ceil(), Samples::ZERO);
    }

    #[test]
    fn sample_position_survives_a_long_file() {
        // The reason `SamplePosition` is f64: f32 holds integers exactly only to
        // 2^24, ~5.8 minutes at 48k. A loop point in an hour-long file must not
        // snap, and neither must a preload window computed against one.
        let hour_in = SamplePosition(48_000.0 * 3600.0);
        assert_eq!(
            hour_in.advanced_by(Samples(8192)).to_samples_floor(),
            Samples(172_800_000 + 8192)
        );
    }

    #[test]
    fn seconds_to_samples_computes_wide_enough_for_a_long_render() {
        // f32 cannot represent frame counts past 2^24 (~6 min at 48k), which
        // is why the arithmetic widens to f64 before rounding.
        let hour = Seconds(3600.0);
        assert_eq!(hour.to_samples(48_000.0), Samples(172_800_000));
    }

    #[test]
    fn cents_and_semitones_convert_rather_than_scale() {
        // `unit_scalable!(Cents, f32)` means `Cents / 100.0` COMPILES and
        // yields `Cents` — wrong by 100x with a type that says otherwise.
        // These are the forms that carry the unit change.
        assert_eq!(Cents(100.0).to_semitones(), Semitones(1.0));
        assert_eq!(Cents::SEMITONE.to_semitones(), Semitones(1.0));
        assert_eq!(Semitones(1.0).to_cents(), Cents(100.0));
        assert_eq!(Semitones::OCTAVE.to_cents(), Cents(1200.0));

        // An octave doubles the frequency, by either spelling.
        assert!((Semitones::OCTAVE.to_pitch_ratio() - 2.0).abs() < 1e-6);
        assert!((Cents(1200.0).to_pitch_ratio() - 2.0).abs() < 1e-6);
        assert!((Semitones(0.0).to_pitch_ratio() - 1.0).abs() < 1e-6);
    }

    /// `Cents::from_pitch_ratio` is the named inverse of `to_pitch_ratio`.
    ///
    /// It exists because `tuning.rs` was hand-rolling `1200.0 * ratio.log2()`
    /// while the forward direction was already a converter — the asymmetry the
    /// omission rule is meant to catch.
    #[test]
    fn a_pitch_ratio_round_trips_through_cents() {
        for ratio in [0.5_f32, 1.0, 1.5, 2.0, 3.0] {
            let back = Cents::from_pitch_ratio(ratio).to_pitch_ratio();
            assert!(
                (back - ratio).abs() < 1e-4,
                "{ratio} round-tripped to {back}"
            );
        }
        assert_eq!(Cents::from_pitch_ratio(2.0), Cents(1200.0));
        assert_eq!(Cents::from_pitch_ratio(1.0), Cents(0.0));

        // A ratio has no logarithm at or below zero. Unison, not NaN — a NaN
        // here reaches a frequency table and silences a note with no error.
        assert_eq!(Cents::from_pitch_ratio(0.0), Cents(0.0));
        assert_eq!(Cents::from_pitch_ratio(-1.0), Cents(0.0));
    }

    #[test]
    fn beat_duration_to_seconds_is_the_readout_conversion() {
        // At 120 BPM a beat is half a second.
        assert_eq!(BeatDuration(1.0).to_seconds(Bpm(120.0)), Seconds(0.5));
        assert_eq!(BeatDuration(4.0).to_seconds(Bpm(120.0)), Seconds(2.0));
        assert_eq!(BeatDuration(1.0).to_seconds(Bpm(60.0)), Seconds(1.0));
        // A degenerate tempo yields zero rather than inf.
        assert_eq!(BeatDuration(1.0).to_seconds(Bpm(0.0)), Seconds(0.0));
    }

    #[test]
    fn amplitude_is_not_a_normalized_scale() {
        // The claim the old `Linear` doc got backwards: a gain routinely
        // exceeds 1.0. +6 dB is roughly a doubling.
        assert!(Db(6.0).to_amplitude() > Amplitude::UNITY);
        assert_eq!(Db::UNITY.to_amplitude(), Amplitude::UNITY);
        assert_eq!(Amplitude::UNITY.to_db(), Db::UNITY);
        assert_eq!(Amplitude::SILENT.to_db(), Db::FLOOR);

        // Scalable, because trimming a gain is meaningful.
        assert_eq!(Amplitude(2.0) * 0.5, Amplitude::UNITY);
    }

    #[test]
    fn mix_blends_and_refuses_to_scale() {
        assert_eq!(Mix::DRY.blend(1.0, 9.0), 1.0);
        assert_eq!(Mix::WET.blend(1.0, 9.0), 9.0);
        assert_eq!(Mix(0.5).blend(0.0, 1.0), 0.5);
        assert_eq!(Mix::new_clamped(1.5), Mix::WET);
        assert_eq!(Mix::new_clamped(-0.5), Mix::DRY);
        // `Mix * f32` and `Mix + Mix` are deliberately absent — see the
        // omission ledger.
    }

    #[test]
    fn feedback_stops_below_unity() {
        assert_eq!(Feedback::new_clamped(1.5), Feedback::MAX_STABLE);
        assert_eq!(Feedback::new_clamped(-0.2), Feedback::NONE);
        assert!(
            Feedback::MAX_STABLE.get() < 1.0,
            "a unity loop never decays"
        );
        // The literal this constant replaces, at 13 sites.
        assert_eq!(Feedback::MAX_STABLE, Feedback(0.99));
    }

    #[test]
    fn cross_coupled_feedback_is_bounded_as_a_pair() {
        // The bug a per-value clamp cannot catch: two coefficients that feed
        // the SAME recirculation, each individually legal, summing to 1.98.
        let (d, c) = Feedback::stable_pair(0.99, 0.99);
        assert!(
            d.get() + c.get() <= Feedback::MAX_STABLE.get() + 1e-6,
            "combined feedback {} still runs away",
            d.get() + c.get()
        );
        // Scaled together, so the balance between them survives.
        assert!((d.get() - c.get()).abs() < 1e-6);

        // A pair that is already stable passes through untouched.
        let (d, c) = Feedback::stable_pair(0.5, 0.2);
        assert_eq!((d, c), (Feedback(0.5), Feedback(0.2)));

        // Ratio preserved when scaling is needed.
        let (d, c) = Feedback::stable_pair(0.8, 0.4);
        assert!((d.get() / c.get() - 2.0).abs() < 1e-5);
    }

    #[test]
    fn depth_is_signed_because_inversion_is_the_point() {
        assert_eq!(-Depth::FULL, Depth::INVERTED);
        assert_eq!(Depth::new_clamped(-2.0), Depth::INVERTED);
        assert_eq!(Depth::new_clamped(2.0), Depth::FULL);
        // Additive and scalable, unlike `Mix` and `Feedback`.
        assert_eq!(Depth(0.25) + Depth(0.25), Depth(0.5));
        assert_eq!(Depth::FULL * 0.5, Depth(0.5));
    }

    #[test]
    fn the_three_ratios_have_disjoint_ranges() {
        // Which is why one `Ratio` could not serve all three: a legal value of
        // one is an illegal value of another.
        assert_eq!(CompressionRatio::new_clamped(0.5), CompressionRatio::UNITY);
        assert_eq!(CompressionRatio::new_clamped(4.0), CompressionRatio(4.0));

        assert_eq!(Resonance::new_clamped(4.0), Resonance::SELF_OSCILLATION);
        assert_eq!(Resonance::new_clamped(-1.0), Resonance::NONE);

        // `Q` only has to stay above zero — it divides into the damping term.
        assert!(Q::new_clamped(0.0).get() > 0.0);
        assert!(Q::new_clamped(-5.0).get() > 0.0);
        assert_eq!(Q::new_clamped(10.0), Q(10.0));

        // The sharp case: 1.0 means "mild bell" as a Q and "self-oscillating"
        // as a ladder resonance. Same number, different instrument.
        assert!(Q(1.0) > Q::BUTTERWORTH);
        assert_eq!(Resonance(1.0), Resonance::SELF_OSCILLATION);
    }

    #[test]
    fn drive_is_not_a_gain() {
        // Shares `Amplitude`'s range but not its meaning: `Drive` is never
        // multiplied onto a sample, so it is deliberately not scalable.
        assert_eq!(Drive::UNITY, Drive(1.0));
        assert!(Drive(10.0) > Drive::UNITY);
    }

    #[test]
    fn arc_degrees_composes_like_the_displacement_it_is() {
        assert_eq!(ArcDegrees(20.0) + ArcDegrees(15.0), ArcDegrees(35.0));
        assert_eq!(ArcDegrees(20.0) * 0.5, ArcDegrees(10.0));
        assert_eq!(-ArcDegrees(20.0), ArcDegrees(-20.0));
        assert!(ArcDegrees(10.0) < ArcDegrees(20.0));
    }

    #[test]
    fn beat_splits_into_floor_and_fraction() {
        let b = Beat(4.75);
        assert_eq!(b.floor(), Beat(4.0));
        assert_eq!(b.fract(), BeatDuration(0.75));
        // The two halves recombine, and are visibly different kinds of thing.
        assert_eq!(b.floor() + b.fract(), b);
    }

    #[test]
    fn cycles_of_reads_the_divisor_as_a_span_not_a_frequency() {
        // The reciprocal that shipped twice: a *longer* span is a *slower*
        // source. Under the Hz reading, 2.0 would be twice as fast and land on
        // cycle 8 here.
        assert_eq!(Beat(4.0).cycles_of(BeatDuration(2.0)), Some(2.0));
        assert_eq!(Beat(4.0).cycles_of(BeatDuration(0.5)), Some(8.0));

        // Un-wrapped: the integer part is the cycle index a stepped shape
        // addresses itself by, the fraction is the phase.
        let c = Beat(5.0).cycles_of(BeatDuration(2.0)).unwrap();
        assert_eq!(c.floor(), 2.0);
        assert_eq!(c.fract(), 0.5);

        // One guard, replacing three that disagreed — a non-positive span
        // froze on one path and ran backwards on another.
        assert_eq!(Beat(4.0).cycles_of(BeatDuration(0.0)), None);
        assert_eq!(Beat(4.0).cycles_of(BeatDuration(-2.0)), None);
    }

    #[test]
    fn cycles_of_stays_in_f64_where_the_callers_used_to_narrow() {
        // Every call site divided `beat as f32` by an f32 span. That is exact
        // for power-of-two spans, which is why it went unnoticed, and drifts on
        // everything else.
        let beat = Beat(1_000_000.0);
        let span = BeatDuration(3.0);
        let wide = beat.cycles_of(span).unwrap();
        let narrowed = ((beat.get() as f32) / (span.get() as f32)) as f64;
        assert!(
            (wide.fract() - narrowed.fract()).abs() > 0.01,
            "the narrowing this method exists to avoid should be measurable here"
        );
        assert_eq!(wide, 1_000_000.0 / 3.0);
    }

    #[test]
    fn beat_duration_ratio_counts_how_many_fit() {
        // "how many samples fit in this beat span" — the dimensional analysis
        // a rate type would otherwise be invented for.
        let span = BeatDuration(1.0);
        let per_sample = BeatDuration(0.25);
        let n: f64 = span / per_sample;
        assert_eq!(n, 4.0);
    }

    #[test]
    fn rem_euclid_wraps_negative_overshoot_inside_the_span() {
        let len = BeatDuration(4.0);
        // Plain `%` keeps the dividend's sign, landing outside the span.
        assert_eq!(BeatDuration(-1.0) % len, BeatDuration(-1.0));
        // `rem_euclid` is the wrapping-safe form.
        assert_eq!(BeatDuration(-1.0).rem_euclid(len), BeatDuration(3.0));
    }

    #[test]
    fn db_adds_because_gain_stages_cascade() {
        // Two -6 dB stages in series is -12 dB. This is why `Db` is additive
        // while `Beat` is not.
        assert_eq!(Db(-6.0) + Db(-6.0), Db(-12.0));
        assert_eq!(-Db(6.0), Db(-6.0));
        assert!(Db(-3.0) > Db(-6.0));
    }

    #[test]
    fn bpm_differs_from_ignores_ulp_noise() {
        let a = Bpm(120.0);
        assert!(!a.differs_from(Bpm(120.000_000_1), 0.001));
        assert!(a.differs_from(Bpm(140.0), 0.001));
    }

    #[test]
    fn clamp_stays_in_the_unit_type() {
        assert_eq!(Hz(20_000.0).clamp(Hz(20.0), Hz(18_000.0)), Hz(18_000.0));
        assert_eq!(Mix(1.5).clamp(Mix::DRY, Mix::WET), Mix::WET);
    }

    #[test]
    fn sample_rate_carries_rates_f32_would_round() {
        // The reason the unit is f64-backed. 192 kHz is exact in f32, so the
        // integer value is not the hazard — the reciprocal is: a per-sample
        // step computed in f32 carries ~6e-8 of relative error, which a phase
        // accumulator integrates into audible drift over a long render.
        let sr = SampleRate(192_000.0);
        let step_f64 = 1.0_f64 / sr.get();
        let step_f32 = (1.0_f32 / sr.get() as f32) as f64;
        assert!(
            (step_f64 - step_f32).abs() > 0.0,
            "if f32 were exact here the f64 backing would be unmotivated"
        );
    }

    #[test]
    fn sample_rate_accepts_every_shape_a_caller_holds() {
        // File headers and device configs hand out u32; the engine's own
        // arithmetic is f64. Both widen exactly, which is what lets the
        // migrated signatures take `impl Into<SampleRate>` without churning
        // their call sites.
        assert_eq!(SampleRate::from(48_000.0_f64), SampleRate::SR_48K);
        assert_eq!(SampleRate::from(48_000_u32), SampleRate::SR_48K);
        assert_eq!(SampleRate::SR_44K1, SampleRate(44_100.0));
    }

    #[test]
    fn sample_rate_orders_and_bounds_but_does_not_compose() {
        assert!(SampleRate::SR_44K1 < SampleRate::SR_48K);
        assert_eq!(
            SampleRate::SR_44K1.max(SampleRate::SR_48K),
            SampleRate::SR_48K
        );
        assert_eq!(
            SampleRate(192_000.0).clamp(SampleRate::SR_44K1, SampleRate::SR_48K),
            SampleRate::SR_48K
        );
        // Deliberately absent, per the omission ledger beside the definition:
        // `SR_44K1 + SR_48K` (not a rate), `SR_48K * 2.0` (a scaled rate is a
        // different *device* rate), and `SR_48K / SR_44K1` (that quotient is a
        // `SrcRatio`, and `for_rates` owns the derivation plus its guards).
    }

    #[test]
    fn seconds_to_samples_takes_the_typed_rate() {
        // The converter the DSP nodes call. It used to take a bare f64, which
        // is why holding an untyped rate in a node field was frictionless.
        assert_eq!(Seconds(1.0).to_samples(SampleRate::SR_48K), Samples(48_000));
        // Raw floats still infer through `impl Into`, so no call site churned.
        assert_eq!(Seconds(1.0).to_samples(48_000.0), Samples(48_000));
        // The rounding split survives the retype.
        assert_eq!(Seconds(0.5).to_samples_floor(SampleRate(3.0)), Samples(1));
        assert_eq!(Seconds(0.5).to_samples_ceil(SampleRate(3.0)), Samples(2));
    }

    #[test]
    fn nyquist_crosses_from_clock_rate_to_signal_frequency() {
        assert_eq!(SampleRate::SR_48K.nyquist(), Hz(24_000.0));
        assert_eq!(SampleRate::SR_44K1.nyquist(), Hz(22_050.0));

        // The margin is a fraction of *Nyquist*, not of the sample rate. The
        // call sites this replaced scaled the rate (`sr * 0.499`), so porting
        // one means doubling its constant — 0.499 of the rate is 0.998 of
        // Nyquist. Getting that backwards halves every filter's usable range,
        // so it is pinned here.
        assert_eq!(SampleRate::SR_48K.nyquist_scaled(1.0), Hz(24_000.0));
        assert_eq!(SampleRate::SR_48K.nyquist_scaled(0.5), Hz(12_000.0));
        let svf = SampleRate::SR_48K.nyquist_scaled(0.998);
        assert!((svf.get() - 48_000.0 * 0.499).abs() < 1e-3);
    }

    #[test]
    fn nyquist_scaled_refuses_to_exceed_nyquist() {
        // A margin above unity would hand back a frequency above Nyquist —
        // the exact aliasing bug the clamp exists to stop.
        assert_eq!(SampleRate::SR_48K.nyquist_scaled(1.5), Hz(24_000.0));
        assert_eq!(SampleRate::SR_48K.nyquist_scaled(-0.2), Hz(0.0));
    }

    #[test]
    fn src_ratio_derives_from_typed_rates() {
        // `for_rates` is the one place the derivation lives, and it now speaks
        // the same type its two arguments are measured in.
        assert_eq!(
            SrcRatio::for_rates(SampleRate::SR_44K1, SampleRate::SR_44K1),
            SrcRatio::UNITY
        );
        let r = SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1);
        assert!((r.get() - 48_000.0 / 44_100.0).abs() < 1e-6);
    }

    #[test]
    fn src_ratio_is_exactly_unity_for_matched_rates() {
        // Bit-exact, not 1.0000001 — the matched case is the common one and must
        // not accumulate resampling error.
        assert_eq!(SrcRatio::for_rates(44_100.0, 44_100.0), SrcRatio::UNITY);
        // Within the 0.01 Hz tolerance.
        assert_eq!(SrcRatio::for_rates(44_100.005, 44_100.0), SrcRatio::UNITY);
    }

    #[test]
    fn src_ratio_converts_mismatched_rates() {
        let r = SrcRatio::for_rates(48_000.0, 44_100.0);
        assert!((r.get() - 48_000.0 / 44_100.0).abs() < 1e-6);
        assert!(
            r > SrcRatio::UNITY,
            "a faster file reads more source samples"
        );
    }

    #[test]
    fn src_ratio_survives_a_nonpositive_session_rate() {
        // Would otherwise divide by zero and poison every downstream sample.
        assert_eq!(SrcRatio::for_rates(44_100.0, 0.0), SrcRatio::UNITY);
        assert_eq!(SrcRatio::for_rates(44_100.0, -1.0), SrcRatio::UNITY);
    }

    #[test]
    fn playback_rate_clamps_into_range() {
        assert_eq!(PlaybackRate::new_clamped(8.0), PlaybackRate::MAX);
        assert_eq!(PlaybackRate::new_clamped(0.0), PlaybackRate::MIN);
        assert_eq!(PlaybackRate::new_clamped(-2.0), PlaybackRate::MIN);
        assert_eq!(PlaybackRate::new_clamped(1.5), PlaybackRate::new(1.5));
    }

    #[test]
    fn playback_rate_rejects_non_finite() {
        // A NaN rate makes the read position NaN, silencing the clip forever.
        assert_eq!(PlaybackRate::new_clamped(f32::NAN), PlaybackRate::UNITY);
        assert_eq!(
            PlaybackRate::new_clamped(f32::INFINITY),
            PlaybackRate::UNITY
        );
        assert_eq!(
            PlaybackRate::new_clamped(f32::NEG_INFINITY),
            PlaybackRate::UNITY
        );
    }

    #[test]
    fn read_rate_composes_varispeed_with_conversion() {
        // Half speed on a 48k file in a 44.1k session: both effects apply once.
        // Tolerance is f32-scale, not f64: both rates are f32-backed, so the
        // quotient is rounded once on the way in. Widening to f64 in `read_rate`
        // keeps the *accumulation* exact (a read position advanced a million
        // times must not drift), but it cannot recover precision already lost.
        let rate = PlaybackRate::new(0.5);
        let src = SrcRatio::for_rates(48_000.0, 44_100.0);
        let expected = 0.5 * (48_000.0 / 44_100.0);
        assert!((rate.read_rate(src).get() - expected).abs() < 1e-6);

        // Unity on both sides consumes exactly one source sample per output —
        // this one IS exact, and must stay so: it is the matched-rate path every
        // same-sample-rate session takes.
        assert_eq!(
            PlaybackRate::UNITY.read_rate(SrcRatio::UNITY),
            ReadRate::UNITY
        );
    }

    /// `ReadRate` omits `Mul`, so `advance` is the only way to turn a rate into a
    /// distance — the omission ledger's rule that a removed operator ships with
    /// its named replacement.
    #[test]
    fn read_rate_advances_a_position_by_a_sample_count() {
        // Half speed: 512 output samples consume 256 source samples.
        let half = PlaybackRate::new(0.5).read_rate(SrcRatio::UNITY);
        assert_eq!(half.advance(Samples(512)), SamplePosition(256.0));

        // Unity is the identity, exactly.
        assert_eq!(
            ReadRate::UNITY.advance(Samples(1024)),
            SamplePosition(1024.0)
        );

        // Zero output samples advance nowhere, whatever the rate.
        assert_eq!(half.advance(Samples(0)), SamplePosition(0.0));
    }

    /// `then` composes two read rates applied in series, and composition must be
    /// exactly multiplication — a stretched varispeed voice reads at the product.
    #[test]
    fn read_rates_compose_in_series() {
        let half = PlaybackRate::new(0.5).read_rate(SrcRatio::UNITY);
        let double = PlaybackRate::new(2.0).read_rate(SrcRatio::UNITY);

        // Half speed then double: back to unity, and exactly so.
        assert_eq!(half.then(double), ReadRate::UNITY);

        // Unity is the identity on both sides.
        assert_eq!(half.then(ReadRate::UNITY), half);
        assert_eq!(ReadRate::UNITY.then(half), half);

        // Order does not matter — the composition is a product.
        assert_eq!(half.then(double), double.then(half));

        // The motivating case: a 2x-stretched voice at half varispeed reads a
        // quarter of a source sample per output sample. Composed, not
        // hand-multiplied — which is the whole point of the method.
        let stretch_half = ReadRate(0.5);
        assert_eq!(half.then(stretch_half), ReadRate(0.25));
        assert_eq!(
            half.then(stretch_half).advance(Samples(1024)),
            SamplePosition(256.0)
        );
    }

    #[test]
    fn stretch_factor_clamps_and_rejects_non_finite() {
        assert_eq!(StretchFactor::new_clamped(8.0), StretchFactor::MAX);
        assert_eq!(StretchFactor::new_clamped(0.1), StretchFactor::MIN);
        assert_eq!(StretchFactor::new_clamped(f32::NAN), StretchFactor::UNITY);
    }

    #[test]
    fn atomic_sample_position_load_after_store() {
        let cell = AtomicSamplePosition::new(SamplePosition::new(0.0));
        cell.store(SamplePosition::new(123.456), Ordering::Relaxed);
        assert_eq!(cell.load(Ordering::Relaxed), SamplePosition::new(123.456));

        let cell = AtomicSamplePosition::default();
        assert_eq!(cell.load(Ordering::Relaxed), SamplePosition::new(0.0));
        cell.store(SamplePosition::new(-7.0), Ordering::Relaxed);
        assert_eq!(cell.load(Ordering::Relaxed), SamplePosition::new(-7.0));
    }
}
