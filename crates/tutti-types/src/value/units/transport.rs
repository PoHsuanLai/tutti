// Tempo, timeline position, and the rates a clip is read at.

use super::*;

/// Tempo in beats per minute: finite and above zero.
///
/// [`new`](Self::new) refuses NaN, the infinities, zero and negative numbers;
/// there is no musical ceiling or floor here (a UI limit such as 20 to 999 is
/// the caller's). Distinct from `Hz`: beats/minute, not cycles/second.
///
/// `f64`-backed because every real consumer widens tempo to f64 for beat math
/// (beats ↔ seconds conversions with sample-accurate precision), and f32 BPM
/// would silently round-trip through the widening conversion at every read.
///
/// There is no `Unit` impl and no `From<f64>`: both would be an unchecked
/// constructor.
///
/// ```
/// use tutti_types::Bpm;
///
/// assert_eq!(Bpm::new(120.0).map(Bpm::get), Some(120.0));
/// assert_eq!(Bpm::new(0.0), None);
/// assert_eq!(Bpm::new(f64::NAN), None);
/// ```
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
pub struct Bpm(f64);

// NOT `Additive`: the difference of two tempos is not a tempo, and
// returning `Bpm` would let it be fed to a beat-rate calculation. Use
// `differs_from` for the "did the tempo move?" test.
// NOT `Scalable`, and no `Neg`: a scaled or negated tempo can leave the range
// (a negative factor, an overflow). Scale the raw number and call `new`.
impl Bpm {
    /// The tempo, or `None` when `raw` is NaN, infinite, zero or negative.
    #[inline]
    #[must_use]
    pub const fn new(raw: f64) -> Option<Bpm> {
        if raw.is_finite() && raw > 0.0 {
            Some(Bpm(raw))
        } else {
            None
        }
    }

    /// The tempo for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is not finite and above zero; in a `const` that is a compile
    /// error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: f64) -> Bpm {
        match Self::new(raw) {
            Some(bpm) => bpm,
            None => panic!("a tempo is finite and above zero"),
        }
    }

    /// Returns the raw beats per minute, for a boundary the unit types do not
    /// cross (C ABI, IPC, WIT) or for beat math.
    #[inline]
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// Returns the smaller of the two, in this unit.
    #[inline]
    #[must_use]
    pub fn min(self, other: Bpm) -> Bpm {
        Bpm(self.0.min(other.0))
    }

    /// Returns the larger of the two, in this unit.
    #[inline]
    #[must_use]
    pub fn max(self, other: Bpm) -> Bpm {
        Bpm(self.0.max(other.0))
    }

    /// Clamps into `lo..=hi`, in this unit.
    ///
    /// # Panics
    ///
    /// Panics if `lo > hi`, matching `f64::clamp`.
    #[inline]
    #[must_use]
    pub fn clamp(self, lo: Bpm, hi: Bpm) -> Bpm {
        Bpm(self.0.clamp(lo.0, hi.0))
    }

    /// Whether this tempo differs from `other` by more than `epsilon` BPM.
    ///
    /// The transport clock re-derives its per-sample beat increment only when
    /// the tempo actually moved; comparing floats for equality would re-derive
    /// on every buffer from ULP noise.
    #[inline]
    #[must_use]
    pub fn differs_from(self, other: Bpm, epsilon: f64) -> bool {
        (self.0 - other.0).abs() > epsilon
    }
}

/// 120 BPM, the tempo a new transport runs at.
impl Default for Bpm {
    #[inline]
    fn default() -> Bpm {
        Bpm::of(120.0)
    }
}

impl core::fmt::Display for Bpm {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl TryFrom<f64> for Bpm {
    type Error = BpmOutOfRange;

    #[inline]
    fn try_from(raw: f64) -> Result<Bpm, BpmOutOfRange> {
        Bpm::new(raw).ok_or(BpmOutOfRange(raw))
    }
}

impl From<Bpm> for f64 {
    #[inline]
    fn from(bpm: Bpm) -> f64 {
        bpm.0
    }
}

/// A raw number that is not a tempo: NaN, infinite, zero or negative (see
/// [`Bpm::new`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BpmOutOfRange(pub f64);

impl core::fmt::Display for BpmOutOfRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "a tempo is finite and above zero, got {}", self.0)
    }
}

impl core::error::Error for BpmOutOfRange {}

unit! {
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
    pub struct Beat(pub f64): Unit, Ordered, Bounded, Affine(BeatDuration);
}
// NOT `Additive`: `Beat + Beat` has no meaning without an origin.
// NOT `Scalable`: scaling a position depends on where zero is — scale the
// duration and add it.
// NOT `Modular`: `beat % len` silently assumes the cycle starts at beat 0,
// which invites an off-by-a-loop-start bug. The real operation is
// `(beat - start) % len + start`, where the origin is explicit.
impl Beat {
    /// The start of the beat this position falls in.
    #[inline]
    #[must_use]
    pub fn floor(self) -> Beat {
        Beat(self.0.floor())
    }

    /// How far into the current beat this position is — a *displacement* from
    /// [`floor`](Self::floor), not a position. In `[0, 1)` for every finite
    /// position; NaN and the infinities give NaN.
    ///
    /// A position a hair under a whole beat (`-1e-20`) is within rounding of
    /// the next beat; it gives the largest value under 1 rather than 1.
    #[inline]
    #[must_use]
    pub fn fract(self) -> BeatDuration {
        let f = self.0 - self.0.floor();
        BeatDuration(if f >= 1.0 { 1.0_f64.next_down() } else { f })
    }

    /// Un-wrapped cycle position of a beat-synced periodic source — the one
    /// place beats-per-cycle is interpreted.
    ///
    /// The divisor is a span, not a frequency: doubling it makes the source
    /// *slower*. Substituting an `Hz` is a silent reciprocal — the whole reason
    /// the interpretation lives here and not at each call site.
    ///
    /// Un-wrapped so a stepped shape can read its cycle index off the integer
    /// part. `None` for a non-positive span — a frozen source holds its phase
    /// offset instead.
    #[inline]
    #[must_use]
    pub fn cycles_of(self, beats_per_cycle: BeatDuration) -> Option<f64> {
        (beats_per_cycle.0 > 0.0).then(|| self.0 / beats_per_cycle.0)
    }
}

// ── Playback rates ──────────────────────────────────────────────────────────
//
// Three distinct quantities that all multiply into one advance, kept apart
// because that is the point: `SrcRatio` is derived from the two sample rates
// and is never user intent, `PlaybackRate` is user intent that couples pitch to
// speed, and `StretchFactor` is user intent that does not. Under one `Ratio`
// they multiply together silently; here the wrong pair is a type error.

unit! {
    /// Sample-rate conversion ratio: source rate ÷ destination rate.
    ///
    /// **Derived, never user intent.** A 48 kHz file played by a 44.1 kHz
    /// session reads at `48000/44100 ≈ 1.088` source samples per output sample,
    /// so it sounds correct. Build it with [`SrcRatio::for_rates`] rather than
    /// by hand — that is the one place the derivation lives.
    ///
    /// `f64`-backed, like [`ReadRate`] and [`SamplePosition`]: an `f32` ratio
    /// drifts a converted stream against the timeline (about 2 to 12 file
    /// frames an hour at common rate pairs).
    ///
    /// Distinct from [`PlaybackRate`] because they answer different questions:
    /// this one asks "what does playing at the right pitch cost?", the other
    /// "how fast does the user want it?". They compose (see
    /// [`PlaybackRate::read_rate`]) but must not be confused for one another.
    pub struct SrcRatio(pub f64): Unit, Ordered, Bounded;
}
// NOT `Scalable` / `Additive`: a conversion ratio is derived from
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
            Self(file_rate / session_rate)
        }
    }
}

unit! {
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
    /// The range is a resampler limit, and it lives on the constructor rather
    /// than in each backend's setter. Split across two setters, one tier
    /// accepts an out-of-range speed the other rejects — same command,
    /// different audio. A newtype alone does not close that; the shared
    /// constructor does.
    pub struct PlaybackRate(pub f32): Unit, Ordered, Bounded;
}
// NOT `Additive`: 2× plus 2× is not 4× — rates compose by multiplication.
// NOT `Scalable`: scaling by a bare `f32` is how the three rate kinds got
// multiplied together in the first place. Compose through `read_rate` instead,
// which names both operands.
impl PlaybackRate {
    /// Normal speed.
    pub const UNITY: Self = Self(1.0);

    /// Slowest supported varispeed (quarter speed).
    pub const MIN: Self = Self(0.25);

    /// Fastest supported varispeed (4× speed).
    pub const MAX: Self = Self(4.0);

    /// Creates a rate, clamping into [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    ///
    /// NaN stays NaN, which makes a read position NaN: a caller that takes
    /// user input guards it first.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Self {
        Self(v.clamp(Self::MIN.0, Self::MAX.0))
    }

    /// Source samples consumed per output sample: varispeed × conversion.
    ///
    /// The single composition point for the two rates. Both operands are named,
    /// so the pair cannot be swapped or one of them silently dropped.
    #[inline]
    #[must_use]
    pub fn read_rate(self, src: SrcRatio) -> ReadRate {
        ReadRate(self.0 as f64 * src.0)
    }
}

unit! {
    /// Source samples consumed per output sample: varispeed × sample-rate
    /// conversion, the product [`PlaybackRate::read_rate`] returns.
    ///
    /// Its own type rather than a bare `f64`, on both of the rule's counts:
    ///
    /// - **Distinct algebra.** [`PlaybackRate`] and [`SrcRatio`] both omit `Mul`
    ///   precisely so the three rate kinds cannot be multiplied together
    ///   ad-hoc. This type exists *to be* multiplied — by a sample count, to
    ///   advance a read position — which is the one operation neither factor
    ///   allows. So `Scalable` stays omitted here too, and
    ///   [`advance`](Self::advance) is the named replacement.
    /// - **Distinct range.** `PlaybackRate` is clamped to 0.25–4.0; `SrcRatio` is
    ///   unclamped (a 192 kHz file in a 44.1 kHz session is ≈4.35), so the
    ///   product reaches ≈17.4× — a range neither factor has.
    ///
    /// `f64`-backed, matching [`SamplePosition`]: a read position advanced a
    /// million times must not drift, and widening here is what keeps the
    /// accumulation exact. As a type of its own, it cannot be mixed up with a
    /// file sample rate, nor have `src_ratio` applied to it a second time.
    pub struct ReadRate(pub f64): Unit, Ordered, Bounded;
}
// NOT `Scalable` / `Additive`: see the type docs. The whole point is
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
    #[must_use]
    pub fn advance(self, out: Samples) -> SamplePosition {
        SamplePosition(self.0 * out.get() as f64)
    }

    /// Composes two read rates applied in series.
    ///
    /// The named replacement for the omitted `Mul<Self>`, and deliberately
    /// narrower: it composes `ReadRate` with `ReadRate` only, so the three
    /// *source* rate kinds still cannot be multiplied together ad-hoc. Each
    /// factor has already resolved its own derivation through `read_rate`, so
    /// what remains is genuine composition — two resamplings in the same signal
    /// path — rather than the ratio-mixing the omission exists to block.
    ///
    /// For example, a stretched voice reads its source at varispeed *and* at
    /// the stretcher's rate, and the two are computed by different owners (the
    /// source knows `speed`, the filter knows the stretch factor).
    #[inline]
    #[must_use]
    pub fn then(self, other: Self) -> Self {
        Self(self.0 * other.0)
    }
}

unit! {
    /// Time-stretch factor: how much longer a clip plays, pitch unchanged.
    ///
    /// 2.0 plays twice as long at unchanged pitch — time-stretching, not
    /// resampling. For the kind that couples pitch to duration,
    /// see [`PlaybackRate`].
    pub struct StretchFactor(pub f32): Unit, Ordered, Bounded;
}
// NOT `Additive` / `Scalable`: same reasoning as `PlaybackRate` —
// stretch factors compose by multiplication, and a bare-scalar `*` invites
// mixing them with the other two rate kinds.
impl StretchFactor {
    /// No stretching.
    pub const UNITY: Self = Self(1.0);

    /// Shortest supported stretch (quarter length).
    pub const MIN: Self = Self(0.25);

    /// Longest supported stretch (4× length).
    pub const MAX: Self = Self(4.0);

    /// Creates a factor, clamping into [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    ///
    /// NaN stays NaN, as in [`PlaybackRate::new_clamped`]; the raw `new` is
    /// unchecked. Use this on any path carrying user input, and guard a NaN
    /// first.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Self {
        Self(v.clamp(Self::MIN.0, Self::MAX.0))
    }
}

unit! {
    /// Absolute position within a wave, measured in samples.
    ///
    /// Fractional positions are allowed so interpolating readers can address
    /// between two integer sample indices. `f64`-backed because sample offsets
    /// into long buffers routinely exceed `f32`'s integer-precision range and
    /// the fractional part must survive sample-accurate arithmetic.
    pub struct SamplePosition(pub f64): Unit, Ordered, Bounded, Additive, Scalable;
}
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

impl SamplePosition {
    /// The integer sample index this position falls in.
    #[inline]
    #[must_use]
    pub fn floor(self) -> SamplePosition {
        SamplePosition(self.0.floor())
    }

    /// The interpolation fraction between this index and the next, in `[0, 1)`
    /// for every finite position; NaN and the infinities give NaN.
    ///
    /// A position a hair under a whole index (`-1e-20`) gives the largest value
    /// under 1 rather than 1, as [`Beat::fract`] does.
    #[inline]
    #[must_use]
    pub fn fract(self) -> f64 {
        let f = self.0 - self.0.floor();
        if f >= 1.0 { 1.0_f64.next_down() } else { f }
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
    /// For example, a preload window: "keep `window` frames resident starting
    /// from `entry`" is `entry.advanced_by(window)`, keeping the fraction that
    /// rounding each side first would lose.
    #[inline]
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn to_samples_ceil(self) -> Samples {
        Samples(self.0.ceil().max(0.0) as usize)
    }
}

unit! {
    /// A span on the timeline, measured in beats.
    ///
    /// Distinct from [`Beat`]: subtracting one position from another
    /// yields a duration, not a position. `f64`-backed to match beat-position
    /// precision.
    pub struct BeatDuration(pub f64):
        Unit, Ordered, Bounded, Additive, Signed, Scalable, Ratio, Modular;
}
// `dur / dur -> f64` answers "how many of these fit", which is how a beat span
// converts to a sample count against a per-sample span.

impl BeatDuration {
    /// This span in seconds at `tempo`.
    ///
    /// For *duration* readouts — a clip length, a UI-facing time display. It is
    /// deliberately **not** the conversion the per-sample transport path uses:
    /// `tutti_types::beats_per_sample` computes
    /// `(tempo / 60) / sample_rate`, the one spelling every reader placing a
    /// beat in a block divides by (the two groupings round differently, and
    /// readers must agree). Routing those sites through this method would
    /// silently re-associate the arithmetic. Two conversions, two call sites,
    /// on purpose. (No clock steps by either: a playhead is a frame count,
    /// [`TimelineSegment`](crate::value::TimelineSegment).)
    #[inline]
    #[must_use]
    pub fn to_seconds(self, tempo: Bpm) -> Seconds {
        // A tempo too slow for a finite seconds-per-beat reads as zero, not inf.
        if !(60.0 / tempo.0).is_finite() {
            return Seconds(0.0);
        }
        Seconds((self.0 * 60.0 / tempo.0) as f32)
    }

    /// Magnitude, discarding direction.
    #[inline]
    #[must_use]
    pub fn abs(self) -> BeatDuration {
        BeatDuration(self.0.abs())
    }

    /// Whether this span moves forward. `LoopRange`'s non-empty invariant.
    #[inline]
    #[must_use]
    pub fn is_positive(self) -> bool {
        self.0 > 0.0
    }

    /// The remainder in `[0, |rhs|)`, for a finite `self` and a finite non-zero
    /// `rhs`. A zero `rhs` or a NaN gives NaN. An infinite `rhs` is outside
    /// that contract: a negative `self` gives `f64::MAX` there, and a non-negative
    /// one is returned unchanged, as before.
    ///
    /// `%` on floats keeps the sign of the dividend, so a negative overshoot
    /// would wrap *outside* the span. This is the wrapping-safe form. A
    /// negative `self` a hair under a whole span rounds up to the span itself
    /// in `f64::rem_euclid`; it gives the largest value under the span instead.
    #[inline]
    #[must_use]
    pub fn rem_euclid(self, rhs: BeatDuration) -> BeatDuration {
        let r = self.0.rem_euclid(rhs.0);
        let span = rhs.0.abs();
        BeatDuration(if r >= span { span.next_down() } else { r })
    }
}

#[cfg(test)]
mod tests;
