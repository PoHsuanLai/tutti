//! Unit newtypes for DSP and transport parameters.
//!
//! Each newtype is `#[repr(transparent)]` around its raw float type (`f32` by
//! default, `f64` for wide-range values like `Bpm`/`Beat`) — zero-cost at
//! runtime, but distinct at compile time so `Hz` and `Seconds` cannot be
//! swapped by accident. The [`Unit`] marker trait carries the raw type as an
//! associated type so [`Param`](super::Param) can be generic over the unit.
//!
//! `fundsp`'s `SampleRate` also implements [`Unit`] — but that `impl` lives in
//! `fundsp-tutti` (where the type is defined), since `fundsp-tutti` depends on
//! this crate, not the reverse.

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
// it does not double the gain. Convert through `Linear` for amplitude scaling.
// NOT `unit_ratio!`: a quotient of logarithms is not a quantity.
unit_newtype!(
    /// Unitless normalized amount. Used for mix (0..1), feedback (0..~0.99),
    /// depth, LFO amplitude, and similar ratio-of-range controls.
    Linear
);
unit_ordered!(Linear);
unit_bounded!(Linear, f32);
unit_scalable!(Linear, f32);
// NOT `unit_additive!`: two 0..1 mixes summing to 1.4 is out of range and means
// nothing. `Linear` is a position on a normalized scale, not a magnitude.
unit_newtype!(
    /// Dimensionless ratio. Used for compressor ratio and filter Q.
    Ratio
);
unit_ordered!(Ratio);
unit_bounded!(Ratio, f32);
unit_scalable!(Ratio, f32);
// NOT `unit_additive!`: 4:1 plus 4:1 is not 8:1.
unit_newtype!(
    /// Angle in degrees. Used for spatial azimuth and elevation.
    Degrees
);
// A WRAPPING coordinate: azimuth runs -180..180 with 0 = front. Ordering and
// addition are both traps here — `Degrees(350) < Degrees(10)` is true under a
// naive compare, but 350 deg is 20 deg *clockwise* of 10 deg, and a
// non-wrapping `+` silently leaves the circle. Angular arithmetic wants named
// methods (shortest_arc_to, wrap_signed), not operators.
unit_signed!(Degrees);
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
    pub fn for_rates(file_rate: f64, session_rate: f64) -> Self {
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
    pub fn read_rate(self, src: SrcRatio) -> f64 {
        self.0 as f64 * src.0 as f64
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
    /// - `Linear + Linear`, `Ratio + Ratio` — normalized scales do not compose.
    /// - `Degrees < Degrees`, `Degrees + Degrees` — a wrapping coordinate.
    /// - `Bpm - Bpm` — a tempo difference is not a tempo.
    #[test]
    fn omitted_operators_are_documented() {}

    #[test]
    fn beat_splits_into_floor_and_fraction() {
        let b = Beat(4.75);
        assert_eq!(b.floor(), Beat(4.0));
        assert_eq!(b.fract(), BeatDuration(0.75));
        // The two halves recombine, and are visibly different kinds of thing.
        assert_eq!(b.floor() + b.fract(), b);
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
        assert_eq!(Linear(1.5).clamp(Linear(0.0), Linear(1.0)), Linear(1.0));
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
        assert!((rate.read_rate(src) - expected).abs() < 1e-6);

        // Unity on both sides consumes exactly one source sample per output —
        // this one IS exact, and must stay so: it is the matched-rate path every
        // same-sample-rate session takes.
        assert_eq!(PlaybackRate::UNITY.read_rate(SrcRatio::UNITY), 1.0);
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
