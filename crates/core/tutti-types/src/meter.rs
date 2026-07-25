//! Musical meter: the [`TimeSignature`] value, the [`MeterMap`] timeline, and
//! the [`Meter`] query trait.
//!
//! Where [`Beat`] answers "where am I on the timeline", meter answers "what does
//! that mean musically" — which bar, which beat within it, and whether that beat
//! is the downbeat. That mapping is pure integer/float math with no audio
//! dependency, so it lives here as a layer *over* the transport rather than as
//! state inside it: nothing in this module touches the graph, the transport
//! settings, or the audio thread.
//!
//! Before this, the same idea was re-encoded four ways: a `(u32, u32)` pair in
//! `tutti-core`, a `(u8, u8)` pair in the app model and its CRDT bridge, an
//! `(i32, i32)` pair in the plugin transport snapshot, and a `(u16, u16)` cast at
//! the CLAP boundary. Two of them — the bar/beat readout and the timeline ruler —
//! computed bar length as the numerator alone, which is wrong for anything but
//! `x/4`; the plugin ones only forwarded the pair, and the `tutti-core` one was
//! correct but had no consumers.
//!
//! # Value vs query
//!
//! [`TimeSignature`] is the value a document stores and a UI edits. [`Meter`] is
//! what consumers *read through* — deliberately a trait, and deliberately taking
//! the position as an argument. A constant meter ignores it; a [`MeterMap`]
//! binary-searches on it. Consumers written against `Meter` need no edit to
//! support meter changes, which is why the seam exists at all.
//!
//! # Quarter notes are the unit of position
//!
//! [`Beat`] is always a quarter note, everywhere in the engine. A time signature
//! describes how those quarter notes group into bars and what the *notated* beat
//! is: in 7/8 a bar is 3.5 quarter notes long and the notated beat is an eighth
//! (0.5 quarter notes). Keeping position in quarters and asking the meter for the
//! grouping is what lets tempo and meter stay orthogonal.

use crate::value::{Beat, BeatDuration};

/// How many notated beats are in a bar — the upper number of a time signature.
///
/// A separate type from [`NoteValue`] because the two are trivially transposable
/// and the compiler should catch it: `TimeSignature::new(4, 8)` and `new(8, 4)`
/// are both plausible-looking and only one is what you meant.
///
/// Also serves as the 1-based ordinal of a beat *within* a bar
/// ([`BarPosition::beat`]) — the same `1..=MAX` domain, so comparing a position's
/// beat against its signature's count is type-checked.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct BeatsPerBar(u32);

/// Largest [`BeatsPerBar`] a signature may name.
///
/// Bounded because the ruler and the metronome both loop over the beats in a bar;
/// an absurd numerator from a malformed document should produce a strange meter,
/// not a hang.
pub const MAX_BEATS_PER_BAR: u32 = 64;

impl BeatsPerBar {
    /// Construct, clamping to `1..=`[`MAX_BEATS_PER_BAR`].
    ///
    /// Total rather than fallible: the writers are a UI number field and a CRDT
    /// document, neither of which has a channel to report an error on. Zero would
    /// make [`TimeSignature::bar_length`] zero and every downstream `bar_at`
    /// divide by it.
    #[inline]
    pub const fn new(beats: u32) -> Self {
        Self(if beats == 0 {
            1
        } else if beats > MAX_BEATS_PER_BAR {
            MAX_BEATS_PER_BAR
        } else {
            beats
        })
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for BeatsPerBar {
    fn default() -> Self {
        Self(4)
    }
}

impl core::fmt::Display for BeatsPerBar {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// Which note value gets the beat, named by its denominator — the lower number of
/// a time signature. `NoteValue::EIGHTH` is 8.
///
/// Always a power of two, enforced on construction: `x/5` is not a time
/// signature, and a non-power-of-two here would break the
/// [SMF exponent](Self::to_smf_exponent) round trip.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct NoteValue(u32);

/// Largest note value a denominator may name (a whole note).
pub const MIN_NOTE_VALUE: u32 = 1;
/// Smallest note value a denominator may name (a 64th note).
pub const MAX_NOTE_VALUE: u32 = 64;

impl NoteValue {
    /// A whole note — `x/1`.
    pub const WHOLE: Self = Self(1);
    /// A half note — `x/2`.
    pub const HALF: Self = Self(2);
    /// A quarter note — `x/4`. The unit [`Beat`] is measured in.
    pub const QUARTER: Self = Self(4);
    /// An eighth note — `x/8`.
    pub const EIGHTH: Self = Self(8);
    /// A sixteenth note — `x/16`.
    pub const SIXTEENTH: Self = Self(16);

    /// Construct, rounding up to a power of two and clamping to
    /// [`MIN_NOTE_VALUE`]`..=`[`MAX_NOTE_VALUE`].
    ///
    /// Rounding up rather than rejecting matches what the inspector's number
    /// field already does by hand (`.next_power_of_two().clamp(2, 16)`);
    /// centralising it means the document and the UI cannot disagree about what
    /// `x/5` means.
    ///
    /// The clamp comes **before** the rounding, not after: `next_power_of_two`
    /// overflows above 2³¹, which panics in debug and returns 0 in release. A
    /// `NoteValue(0)` would break this type's entire invariant — `bar_length`
    /// becomes infinite and every downstream bar position `NaN`.
    #[inline]
    pub fn new(value: u32) -> Self {
        Self(
            value
                .clamp(MIN_NOTE_VALUE, MAX_NOTE_VALUE)
                .next_power_of_two(),
        )
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// This note value as a Standard MIDI File denominator exponent — SMF stores
    /// `3` to mean an eighth note, not `8`.
    ///
    /// Homed here rather than in the MIDI crate so the encode/decode pair stays
    /// adjacent, and so the exponent↔value conversion has one spelling when a
    /// MIDI file reader arrives. Nothing consumes it yet — SMF import currently
    /// drops meter entirely.
    #[inline]
    pub const fn to_smf_exponent(self) -> u8 {
        self.0.trailing_zeros() as u8
    }

    /// Build from a Standard MIDI File denominator exponent (`3` → an eighth).
    ///
    /// Saturates at [`MAX_NOTE_VALUE`], so a corrupt file cannot shift past the
    /// width of the underlying integer.
    #[inline]
    pub fn from_smf_exponent(exponent: u8) -> Self {
        if exponent >= MAX_NOTE_VALUE.trailing_zeros() as u8 {
            Self(MAX_NOTE_VALUE)
        } else {
            Self(1 << exponent)
        }
    }
}

impl Default for NoteValue {
    fn default() -> Self {
        Self::QUARTER
    }
}

impl core::fmt::Display for NoteValue {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// Integer conversions for the two halves of a signature.
///
/// The plugin ABIs each want a different width — CLAP takes `u16`, VST2 and VST3
/// take `i32` — so the conversion lives beside the type rather than being spelled
/// (and mis-cast) at each boundary. Every `From` runs through the validating
/// constructor, so a negative or absurd value from a foreign host lands on a
/// legal meter instead of wrapping.
macro_rules! signature_conversions {
    ($($t:ty),*) => {$(
        impl From<$t> for BeatsPerBar {
            #[inline]
            fn from(n: $t) -> Self {
                Self::new(n.clamp(0, MAX_BEATS_PER_BAR as $t) as u32)
            }
        }
        impl From<BeatsPerBar> for $t {
            #[inline]
            fn from(b: BeatsPerBar) -> Self {
                b.0 as $t
            }
        }
        impl From<$t> for NoteValue {
            #[inline]
            fn from(n: $t) -> Self {
                Self::new(n.clamp(0, MAX_NOTE_VALUE as $t) as u32)
            }
        }
        impl From<NoteValue> for $t {
            #[inline]
            fn from(v: NoteValue) -> Self {
                v.0 as $t
            }
        }
    )*};
}

signature_conversions!(u8, u16, u32, i32, i64, usize);

/// Deserialize through the validating constructors.
///
/// **Hand-written rather than derived**, because a derived `Deserialize` writes
/// the private field directly and skips `new()` entirely — so a document
/// claiming `{"beats_per_bar": 0, "note_value": 0}` would produce a value this
/// type's own docs say is impossible, `bar_length()` would be `NaN`, and the
/// first `bar_at` would take that `NaN` into the metronome on the audio thread.
/// Deserialization is precisely the untrusted edge the clamping exists for, so it
/// is the last place that should bypass it.
///
/// `Serialize` stays derived: writing out a value that is already valid needs no
/// checking, and `#[serde(transparent)]` keeps it a bare number on the wire.
#[cfg(feature = "serde")]
mod serde_impls {
    use super::{BeatsPerBar, NoteValue};
    use serde::{Deserialize, Deserializer};

    impl<'de> Deserialize<'de> for BeatsPerBar {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            Ok(Self::new(u32::deserialize(d)?))
        }
    }

    impl<'de> Deserialize<'de> for NoteValue {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            Ok(Self::new(u32::deserialize(d)?))
        }
    }
}

/// A musical time signature, e.g. 4/4 or 7/8.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TimeSignature {
    beats_per_bar: BeatsPerBar,
    note_value: NoteValue,
}

impl TimeSignature {
    /// Construct from the two halves.
    ///
    /// Total: both components clamp in their own constructors, so there is no
    /// invalid signature to reject.
    #[inline]
    pub const fn new(beats_per_bar: BeatsPerBar, note_value: NoteValue) -> Self {
        Self {
            beats_per_bar,
            note_value,
        }
    }

    /// Construct from raw numbers, coercing each to its legal range.
    ///
    /// For the document/ABI edges, where the numbers arrive untyped. Prefer
    /// [`new`](Self::new) in engine code, where the types make transposition a
    /// compile error.
    #[inline]
    pub fn from_parts(beats_per_bar: u32, note_value: u32) -> Self {
        Self::new(BeatsPerBar::new(beats_per_bar), NoteValue::new(note_value))
    }

    #[inline]
    pub const fn beats_per_bar(self) -> BeatsPerBar {
        self.beats_per_bar
    }

    #[inline]
    pub const fn note_value(self) -> NoteValue {
        self.note_value
    }

    /// One bar's length in quarter-note beats. 7/8 is 3.5, **not** 7.
    ///
    /// This is the conversion every bar-math site needs, and treating the
    /// numerator as the bar length is the bug this type exists to prevent.
    #[inline]
    pub fn bar_length(self) -> BeatDuration {
        BeatDuration(f64::from(self.beats_per_bar.get()) * 4.0 / f64::from(self.note_value.get()))
    }

    /// One *notated* beat's length in quarter-note beats — 0.5 in 7/8.
    ///
    /// What the metronome clicks and what the ruler subdivides, as distinct from
    /// [`bar_length`](Self::bar_length), which is what bars are counted in.
    #[inline]
    pub fn beat_length(self) -> BeatDuration {
        BeatDuration(4.0 / f64::from(self.note_value.get()))
    }
}

impl core::fmt::Display for TimeSignature {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{}", self.beats_per_bar, self.note_value)
    }
}

/// A bar's position on the timeline.
///
/// **1-based**: bar 1 is the first bar, which is what every display, every plugin
/// ABI (CLAP's `bar_number`), and every musician means. Bar 0 and below are legal
/// and describe pre-roll — a count-in sits at negative beats.
///
/// A position, not a count: `BarNumber + BarNumber` has no meaning without an
/// origin, exactly as `Beat + Beat` does not. Subtracting two gives a
/// [`BarCount`].
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct BarNumber(pub i64);

/// A number of bars — the displacement between two [`BarNumber`]s.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct BarCount(pub i64);

// Both fields are public — unlike the signature halves, a bar number has no range
// invariant for a constructor to enforce — but `new`/`get` exist anyway, because
// every other unit newtype in this crate has them (`Samples`, `Beat`, `Bpm`, …)
// and a host reaching for the familiar spelling should find it.

impl BarNumber {
    /// The first bar of the timeline.
    pub const FIRST: Self = Self(1);

    #[inline]
    pub const fn new(n: i64) -> Self {
        Self(n)
    }

    #[inline]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// How many whole bars precede this one — `BarNumber(1)` yields `0`.
    ///
    /// The 1-based/0-based conversion, in one place, so callers indexing from a
    /// bar number do not each re-derive the off-by-one.
    #[inline]
    pub const fn index(self) -> i64 {
        self.0 - 1
    }

    /// Build from a 0-based index, the inverse of [`index`](Self::index).
    #[inline]
    pub const fn from_index(index: i64) -> Self {
        Self(index + 1)
    }
}

impl BarCount {
    #[inline]
    pub const fn new(n: i64) -> Self {
        Self(n)
    }

    #[inline]
    pub const fn get(self) -> i64 {
        self.0
    }
}

// The affine pair. `unit_affine!` is written against `+`/`-` only, so it applies
// to integral bar-space unchanged — the macros are visible here because `value`
// is `#[macro_use]`d ahead of this module in lib.rs.
//
// The set is complete rather than trimmed to current callers — `tutti` is a
// library, so the algebra has to be coherent for a host that is not this DAW. A
// half-implemented affine pair is a worse API than an unused operator.
//
// Deliberately absent: `BarNumber + BarNumber`, which `unit_affine!` omits for
// the same reason it omits `Beat + Beat` — adding two positions needs an origin.
unit_affine!(BarNumber, BarCount);

// `BarCount` composes with itself and negates, the same algebra `unit_additive!` +
// `unit_signed!` give `BeatDuration`.
unit_additive!(BarCount);
unit_signed!(BarCount);

// Scaling stays hand-written. `unit_scalable!` would also generate `Div`, which
// truncates on `i64` — and a fraction of a bar is a `BeatDuration`, not a
// `BarCount`. The omission is the point, so the macro must not be used here.
impl core::ops::Mul<i64> for BarCount {
    type Output = Self;
    #[inline]
    fn mul(self, k: i64) -> Self {
        Self(self.0 * k)
    }
}

impl core::ops::Mul<BarCount> for i64 {
    type Output = BarCount;
    #[inline]
    fn mul(self, count: BarCount) -> BarCount {
        BarCount(self * count.0)
    }
}

impl core::fmt::Display for BarNumber {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl core::fmt::Display for BarCount {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// Integer conversions for bar numbers, for the plugin ABIs that carry them.
macro_rules! bar_conversions {
    ($($t:ty),*) => {$(
        impl From<$t> for BarNumber {
            #[inline]
            fn from(n: $t) -> Self {
                Self(n as i64)
            }
        }
        impl From<BarNumber> for $t {
            #[inline]
            fn from(b: BarNumber) -> Self {
                b.0 as $t
            }
        }
    )*};
}

bar_conversions!(i32, i64);

/// Where a beat position falls in musical bar/beat terms.
///
/// The answer to one query with the arithmetic already done, so no consumer
/// re-derives it — the same shape as [`Compensation`](crate::latency::Compensation).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BarPosition {
    /// Which bar, 1-based.
    pub bar: BarNumber,
    /// Which notated beat within the bar, 1-based — `1..=signature.beats_per_bar()`.
    pub beat: BeatsPerBar,
    /// Timeline position of this bar's downbeat.
    pub bar_start: Beat,
    /// How far past the current notated beat's onset, in quarter notes.
    ///
    /// Scale by [`TimeSignature::beat_length`] to get a 0..1 fraction of the
    /// notated beat — which is what a tick display or a MIDI PPQN figure wants.
    pub fraction: BeatDuration,
    /// The signature in force here.
    pub signature: TimeSignature,
}

impl BarPosition {
    /// Whether this position sits exactly on a bar's downbeat.
    #[inline]
    pub fn is_downbeat(&self) -> bool {
        self.beat == BeatsPerBar::new(1) && self.fraction.get().abs() < DOWNBEAT_EPSILON
    }
}

/// How close to a beat's onset counts as being *on* it.
///
/// Beat positions arrive as accumulated `f64`s, so an exact compare would miss a
/// downbeat by an ULP. One-thousandth of a quarter note is far below audible
/// placement and far above the accumulated error of a realistic session.
const DOWNBEAT_EPSILON: f64 = 1e-3;

/// Slack when counting whole bars in a segment, as a fraction of one bar.
///
/// A segment whose length is an exact multiple of its bar length can divide to
/// `3.9999999996`; without this, rounding up would credit it a bar it does not
/// have. Small enough that a genuinely short final bar — the case the rounding
/// exists for — still counts.
const BAR_COUNT_EPSILON: f64 = 1e-9;

/// What meter is in force at a point on the timeline.
///
/// Consumers — the metronome, the ruler, the bar/beat readout, the plugin
/// transport snapshot — read through this rather than off a [`TimeSignature`]
/// field, so a position-dependent meter substitutes without touching them.
///
/// Taking the position as an argument is the whole point: it is the one signature
/// that a meter map would otherwise force every consumer to change.
pub trait Meter: Send + Sync {
    /// The signature in force at `beat`.
    fn at(&self, beat: Beat) -> TimeSignature;

    /// Where `beat` falls in bar/beat terms.
    fn bar_at(&self, beat: Beat) -> BarPosition;

    /// Timeline position of a bar's downbeat.
    ///
    /// The inverse of [`bar_at`](Self::bar_at) — the ruler iterates bar numbers
    /// and asks where each one goes.
    fn bar_start(&self, bar: BarNumber) -> Beat;
}

/// Split a position into (whole units elapsed, remainder), flooring.
///
/// Floor rather than truncate, because pre-roll sits at negative beats: `-1.0` in
/// 4/4 is bar 0 beat 4, not bar 0 beat -1. Rust's `/` and `%` truncate toward
/// zero, which gets both wrong on the negative side.
#[inline]
fn floor_div_rem(value: BeatDuration, unit: BeatDuration) -> (i64, BeatDuration) {
    let quotient = (value / unit).floor();
    (quotient as i64, value - unit * quotient)
}

/// Bar/beat for `beat` under a single signature, measured from `origin`.
///
/// Shared by both [`Meter`] impls: the constant case passes beat 0 and bar 1, and
/// [`MeterMap`] passes the start of whichever segment `beat` lands in.
#[inline]
fn bar_position_within(
    beat: Beat,
    origin: Beat,
    origin_bar: BarNumber,
    signature: TimeSignature,
) -> BarPosition {
    let bar_length = signature.bar_length();
    let (bars, into_bar) = floor_div_rem(beat - origin, bar_length);
    let bar_start = origin + bar_length * bars as f64;

    let (beat_index, fraction) = floor_div_rem(into_bar, signature.beat_length());
    // No clamp here. `floor_div_rem` returns `into_bar` in `[0, bar_length)`, and
    // `bar_length == beats_per_bar * beat_length` exactly (every denominator is a
    // power of two, so the division is exact in binary), so the index is always
    // in `0..beats_per_bar`. A clamp would only mask a regression in
    // `floor_div_rem` — and the obvious spelling of it panics outright when
    // `beats_per_bar` is 0, turning a bad document into a crash.
    debug_assert!(
        (0..i64::from(signature.beats_per_bar().get())).contains(&beat_index),
        "beat index {beat_index} outside 0..{} for {signature}",
        signature.beats_per_bar()
    );

    BarPosition {
        bar: origin_bar + BarCount(bars),
        beat: BeatsPerBar::new(beat_index as u32 + 1),
        bar_start,
        fraction,
        signature,
    }
}

/// A constant meter: the same signature over the whole timeline, with bar 1
/// starting at beat 0.
impl Meter for TimeSignature {
    #[inline]
    fn at(&self, _beat: Beat) -> TimeSignature {
        *self
    }

    #[inline]
    fn bar_at(&self, beat: Beat) -> BarPosition {
        bar_position_within(beat, Beat(0.0), BarNumber::FIRST, *self)
    }

    #[inline]
    fn bar_start(&self, bar: BarNumber) -> Beat {
        Beat(0.0) + self.bar_length() * bar.index() as f64
    }
}

/// A change of meter at a point on the timeline.
#[derive(Copy, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MeterChange {
    /// Where the new signature takes effect. Always a bar line — see
    /// [`MeterMap`].
    pub beat: Beat,
    pub signature: TimeSignature,
}

impl MeterChange {
    #[inline]
    pub const fn new(beat: Beat, signature: TimeSignature) -> Self {
        Self { beat, signature }
    }
}

/// A timeline of meter changes.
///
/// # Invariants
///
/// [`new`](Self::new) enforces all of these, so every lookup is a plain binary
/// search with no empty case and no "before the first change" case:
///
/// - `changes` is sorted by beat and has no two entries at the same beat.
/// - The first entry is at or before beat 0, so every position — including
///   negative pre-roll — has a signature in force.
/// - `starts[i]` is the [`BarNumber`] of `changes[i]`'s downbeat.
///
/// # A change always begins a bar
///
/// If a change's beat falls mid-bar, the bar before it is simply short. Every
/// major DAW does this, and it is what keeps `starts` exact: no fractional bar
/// accumulates across segments, so bar numbering stays integral no matter how
/// many changes precede a position.
#[derive(Clone, Debug, PartialEq)]
pub struct MeterMap {
    changes: Vec<MeterChange>,
    starts: Vec<BarNumber>,
}

impl MeterMap {
    /// Build from a set of changes, establishing the invariants above.
    ///
    /// Input may be unsorted and may omit a change at the timeline origin;
    /// duplicates at one beat resolve to the last one given, and non-finite
    /// beats are dropped.
    pub fn new(changes: impl IntoIterator<Item = MeterChange>) -> Self {
        // Drop NaN/inf first. A NaN compares false against everything, so it
        // would make the sort non-transitive, survive the dedup, and hand every
        // later lookup a segment whose bar math is all `NaN`. It would also break
        // `PartialEq` reflexivity, silently defeating the change detection the
        // projection relies on.
        let mut changes: Vec<MeterChange> = changes
            .into_iter()
            .filter(|c| c.beat.get().is_finite())
            .collect();
        // Total on finite floats, so `unwrap_or` is unreachable after the filter.
        changes.sort_by(|a, b| {
            a.beat
                .get()
                .partial_cmp(&b.beat.get())
                .unwrap_or(core::cmp::Ordering::Equal)
        });
        // Later entry wins at an equal beat: `dedup_by` keeps the *first* of each
        // run, so walk from the back.
        changes.reverse();
        changes.dedup_by(|a, b| a.beat.get() == b.beat.get());
        changes.reverse();

        // Guarantee a signature at the origin. A map whose first change is after
        // beat 0 would leave earlier positions — pre-roll, or simply bar 1 —
        // without one.
        match changes.first() {
            Some(first) if first.beat.get() <= 0.0 => {}
            _ => changes.insert(0, MeterChange::new(Beat(0.0), TimeSignature::default())),
        }

        let starts = Self::bar_starts(&changes);
        Self { changes, starts }
    }

    /// The bar number each change's downbeat falls on.
    ///
    /// Walked once here rather than per query.
    ///
    /// **Rounds up, with a floor of one bar.** A change always begins a bar, so a
    /// segment that does not divide evenly ends in a short bar — and that short
    /// bar still counts. Flooring (or rounding) would let a segment shorter than
    /// a bar contribute zero, so the next change would reuse the same bar number
    /// and bar N would have two different downbeats. The epsilon absorbs float
    /// error on a segment that *is* an exact multiple, which must not be rounded
    /// up to one bar too many.
    fn bar_starts(changes: &[MeterChange]) -> Vec<BarNumber> {
        let mut starts = Vec::with_capacity(changes.len());
        let mut bar = BarNumber::FIRST;
        starts.push(bar);

        for pair in changes.windows(2) {
            let span = pair[1].beat - pair[0].beat;
            let exact = span / pair[0].signature.bar_length();
            let bars = (exact - BAR_COUNT_EPSILON).ceil() as i64;
            bar += BarCount(bars.max(1));
            starts.push(bar);
        }
        starts
    }

    /// Index of the change in force at `beat`.
    #[inline]
    fn segment_at(&self, beat: Beat) -> usize {
        match self.changes.binary_search_by(|c| {
            c.beat
                .get()
                .partial_cmp(&beat.get())
                .unwrap_or(core::cmp::Ordering::Equal)
        }) {
            Ok(i) => i,
            // `Err(i)` is the insertion point, so the change in force is the one
            // before it. `Err(0)` is the pre-roll case — a beat before the first
            // change — and `saturating_sub` correctly resolves it to the origin
            // segment, which the origin invariant guarantees exists.
            Err(i) => i.saturating_sub(1),
        }
    }

    /// Index of the segment containing `bar`.
    #[inline]
    fn segment_of_bar(&self, bar: BarNumber) -> usize {
        match self.starts.binary_search(&bar) {
            Ok(i) => i,
            // As in `segment_at`: `Err(0)` is a bar before the first change, and
            // resolves to the origin segment.
            Err(i) => i.saturating_sub(1),
        }
    }

    /// The changes in force, in timeline order. Never empty.
    #[inline]
    pub fn changes(&self) -> &[MeterChange] {
        &self.changes
    }

    /// The bar number each change begins on, parallel to [`changes`](Self::changes).
    ///
    /// No production consumer — it exists so the prefix walk is assertable. The
    /// bar-numbering bug this map originally shipped with (a short segment
    /// contributing zero bars, so two changes claimed the same bar) is only
    /// visible here or by comparing `bar_at` against `bar_start` across it.
    #[inline]
    pub fn bar_numbers(&self) -> &[BarNumber] {
        &self.starts
    }
}

impl Default for MeterMap {
    /// Constant 4/4 — one change at beat 0.
    fn default() -> Self {
        Self {
            changes: vec![MeterChange::new(Beat(0.0), TimeSignature::default())],
            starts: vec![BarNumber::FIRST],
        }
    }
}

impl Meter for MeterMap {
    #[inline]
    fn at(&self, beat: Beat) -> TimeSignature {
        self.changes[self.segment_at(beat)].signature
    }

    fn bar_at(&self, beat: Beat) -> BarPosition {
        let i = self.segment_at(beat);
        let change = self.changes[i];
        bar_position_within(beat, change.beat, self.starts[i], change.signature)
    }

    fn bar_start(&self, bar: BarNumber) -> Beat {
        let i = self.segment_of_bar(bar);
        let change = self.changes[i];
        change.beat + change.signature.bar_length() * (bar - self.starts[i]).get() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7/8 — the meter that exposes every "numerator is the bar length" bug.
    fn seven_eight() -> TimeSignature {
        TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH)
    }

    #[test]
    fn bar_length_handles_compound_meter() {
        assert_eq!(TimeSignature::default().bar_length(), BeatDuration(4.0));
        assert_eq!(seven_eight().bar_length(), BeatDuration(3.5));
        assert_eq!(
            TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER).bar_length(),
            BeatDuration(3.0)
        );
    }

    #[test]
    fn beat_length_is_the_notated_beat() {
        assert_eq!(TimeSignature::default().beat_length(), BeatDuration(1.0));
        assert_eq!(seven_eight().beat_length(), BeatDuration(0.5));
    }

    #[test]
    fn components_clamp_rather_than_reject() {
        // Zero beats per bar would make bar_length zero and every bar_at divide
        // by it.
        assert_eq!(BeatsPerBar::new(0).get(), 1);
        assert_eq!(BeatsPerBar::new(999).get(), MAX_BEATS_PER_BAR);
        // x/5 is not a meter; round up to the next power of two.
        assert_eq!(NoteValue::new(5).get(), 8);
        assert_eq!(NoteValue::new(0).get(), 1);
        assert_eq!(NoteValue::new(999).get(), MAX_NOTE_VALUE);
        assert!(TimeSignature::from_parts(0, 5).bar_length().get() > 0.0);
        // Above 2^31 `next_power_of_two` overflows — panicking in debug, and
        // returning 0 in release, which would make `bar_length` infinite. The
        // clamp has to happen first.
        assert_eq!(NoteValue::new(u32::MAX).get(), MAX_NOTE_VALUE);
        assert_eq!(NoteValue::new(2_147_483_649).get(), MAX_NOTE_VALUE);
        assert_eq!(BeatsPerBar::new(u32::MAX).get(), MAX_BEATS_PER_BAR);
    }

    #[test]
    fn signature_components_do_not_transpose() {
        // The whole reason the two halves are distinct types: transposing them
        // is a different meter, and with `u32` on both sides the swap would have
        // compiled silently.
        //
        // Uses a legal power-of-two denominator on both sides, so this compares
        // 7/8 against 8/8 rather than 7/8 against a coerced 8/8 — the first
        // version picked `NoteValue::new(7)`, which rounds to 8, making the
        // assertion hold for a reason unrelated to its name.
        let seven_eight = TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH);
        let eight_eighths = TimeSignature::new(BeatsPerBar::new(8), NoteValue::EIGHTH);
        assert_eq!(seven_eight.bar_length(), BeatDuration(3.5));
        assert_eq!(eight_eighths.bar_length(), BeatDuration(4.0));
        assert_ne!(seven_eight.bar_length(), eight_eighths.bar_length());
    }

    #[test]
    fn smf_exponent_round_trips() {
        for value in [
            NoteValue::WHOLE,
            NoteValue::HALF,
            NoteValue::QUARTER,
            NoteValue::EIGHTH,
            NoteValue::SIXTEENTH,
        ] {
            assert_eq!(NoteValue::from_smf_exponent(value.to_smf_exponent()), value);
        }
        // The conversion SMF actually specifies: 3 means an eighth note.
        assert_eq!(NoteValue::EIGHTH.to_smf_exponent(), 3);
        assert_eq!(NoteValue::from_smf_exponent(3), NoteValue::EIGHTH);
        // A corrupt file must not shift past the integer width.
        assert_eq!(NoteValue::from_smf_exponent(200).get(), MAX_NOTE_VALUE);
    }

    #[test]
    fn constant_meter_counts_bars_from_one() {
        let m = TimeSignature::default();
        let at = |b: f64| m.bar_at(Beat(b));

        assert_eq!(at(0.0).bar, BarNumber(1));
        assert_eq!(at(0.0).beat, BeatsPerBar::new(1));
        assert!(at(0.0).is_downbeat());

        assert_eq!(at(2.5).bar, BarNumber(1));
        assert_eq!(at(2.5).beat, BeatsPerBar::new(3));
        assert_eq!(at(2.5).fraction, BeatDuration(0.5));
        assert!(!at(2.5).is_downbeat());

        assert_eq!(at(4.0).bar, BarNumber(2));
        assert_eq!(at(4.0).beat, BeatsPerBar::new(1));
        assert!(at(4.0).is_downbeat());
    }

    #[test]
    fn constant_meter_handles_compound_bars() {
        // A 7/8 bar is 3.5 quarter notes, and the notated beat is an eighth.
        let m = seven_eight();
        assert_eq!(m.bar_at(Beat(0.0)).bar, BarNumber(1));
        assert_eq!(m.bar_at(Beat(3.0)).bar, BarNumber(1));
        assert_eq!(m.bar_at(Beat(3.0)).beat, BeatsPerBar::new(7));
        // The second bar starts at 3.5 quarters, not at 7.
        assert_eq!(m.bar_at(Beat(3.5)).bar, BarNumber(2));
        assert!(m.bar_at(Beat(3.5)).is_downbeat());
    }

    #[test]
    fn negative_positions_floor_into_preroll() {
        // Pre-roll: one quarter note before the start of bar 1 is the last beat
        // of bar 0 — NOT bar 0 beat -1, which is what truncating division gives.
        let m = TimeSignature::default();
        assert_eq!(m.bar_at(Beat(-1.0)).bar, BarNumber(0));
        assert_eq!(m.bar_at(Beat(-1.0)).beat, BeatsPerBar::new(4));
        assert_eq!(m.bar_at(Beat(-4.0)).bar, BarNumber(0));
        assert_eq!(m.bar_at(Beat(-4.0)).beat, BeatsPerBar::new(1));
        assert!(m.bar_at(Beat(-4.0)).is_downbeat());
        assert_eq!(m.bar_at(Beat(-5.0)).bar, BarNumber(-1));
    }

    #[test]
    fn bar_start_inverts_bar_at() {
        let m = seven_eight();
        for bar in [-2i64, 0, 1, 2, 7, 40] {
            let bar = BarNumber(bar);
            let start = m.bar_start(bar);
            let round_trip = m.bar_at(start);
            assert_eq!(round_trip.bar, bar, "bar_start/bar_at disagree at {bar}");
            assert!(round_trip.is_downbeat());
            assert_eq!(round_trip.bar_start, start);
        }
    }

    #[test]
    fn map_defaults_to_constant_four_four() {
        let map = MeterMap::default();
        assert_eq!(map.changes().len(), 1);
        assert_eq!(map.at(Beat(1000.0)), TimeSignature::default());
        // A default map must agree with the bare signature everywhere.
        let plain = TimeSignature::default();
        for b in [-3.5, 0.0, 1.0, 4.0, 17.25] {
            assert_eq!(map.bar_at(Beat(b)), plain.bar_at(Beat(b)));
            assert_eq!(map.at(Beat(b)), plain.at(Beat(b)));
        }
    }

    #[test]
    fn map_with_one_change_matches_the_bare_signature() {
        // The single-entry map the `SetTimeSignature` sugar builds.
        let map = MeterMap::new([MeterChange::new(Beat(0.0), seven_eight())]);
        let plain = seven_eight();
        for b in [0.0, 1.75, 3.5, 10.0] {
            assert_eq!(map.bar_at(Beat(b)), plain.bar_at(Beat(b)));
            assert_eq!(map.bar_start(BarNumber(3)), plain.bar_start(BarNumber(3)));
        }
    }

    #[test]
    fn map_numbers_bars_continuously_across_a_change() {
        // 4/4 for 2 bars (8 quarters), then 3/4.
        let map = MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::default()),
            MeterChange::new(
                Beat(8.0),
                TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER),
            ),
        ]);

        assert_eq!(map.bar_at(Beat(0.0)).bar, BarNumber(1));
        assert_eq!(map.bar_at(Beat(4.0)).bar, BarNumber(2));
        // The change lands exactly on bar 3's downbeat; numbering must not skip
        // or repeat.
        assert_eq!(map.bar_at(Beat(8.0)).bar, BarNumber(3));
        assert!(map.bar_at(Beat(8.0)).is_downbeat());
        assert_eq!(map.at(Beat(8.0)).bar_length(), BeatDuration(3.0));
        // Then 3-quarter bars.
        assert_eq!(map.bar_at(Beat(11.0)).bar, BarNumber(4));
        assert_eq!(map.bar_at(Beat(14.0)).bar, BarNumber(5));
    }

    #[test]
    fn map_bar_start_inverts_bar_at_across_changes() {
        let map = MeterMap::new([
            MeterChange::new(Beat(0.0), seven_eight()),
            MeterChange::new(Beat(14.0), TimeSignature::default()),
            MeterChange::new(
                Beat(30.0),
                TimeSignature::new(BeatsPerBar::new(5), NoteValue::EIGHTH),
            ),
        ]);

        for bar in 1..=20i64 {
            let bar = BarNumber(bar);
            let start = map.bar_start(bar);
            let back = map.bar_at(start);
            assert_eq!(
                back.bar, bar,
                "round trip failed at {bar} (start {start:?})"
            );
            assert!(back.is_downbeat(), "bar {bar} start is not a downbeat");
        }
    }

    #[test]
    fn map_normalizes_unsorted_and_origin_free_input() {
        // Out of order, and with nothing at the origin.
        let map = MeterMap::new([
            MeterChange::new(Beat(16.0), seven_eight()),
            MeterChange::new(Beat(4.0), TimeSignature::default()),
        ]);

        let beats: Vec<f64> = map.changes().iter().map(|c| c.beat.get()).collect();
        assert_eq!(beats, vec![0.0, 4.0, 16.0], "must sort and seed the origin");
        // The synthesized origin entry is the default meter.
        assert_eq!(map.at(Beat(0.0)), TimeSignature::default());
        // Positions before the first supplied change still resolve.
        assert_eq!(map.at(Beat(-100.0)), TimeSignature::default());
    }

    #[test]
    fn map_dedupes_to_the_last_change_at_a_beat() {
        let map = MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::default()),
            MeterChange::new(Beat(0.0), seven_eight()),
        ]);
        assert_eq!(map.changes().len(), 1);
        assert_eq!(map.at(Beat(0.0)), seven_eight());
    }

    #[test]
    fn map_starts_a_new_bar_at_an_off_grid_change() {
        // 4/4, then a change partway into a bar. However short the leading
        // fragment, it still counts as one bar, so the change always begins
        // bar 2 — and bar 1 has exactly one downbeat.
        //
        // Swept rather than fixed at a single offset: the first version of this
        // test used only 2.0, which is the one value where the old `.round()`
        // happened to give the right answer. Every other offset exposed the bug.
        for offset in [0.5, 1.0, 2.0, 3.0, 3.5] {
            let map = MeterMap::new([
                MeterChange::new(Beat(0.0), TimeSignature::default()),
                MeterChange::new(
                    Beat(offset),
                    TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER),
                ),
            ]);
            assert_eq!(
                map.bar_at(Beat(0.0)).bar,
                BarNumber(1),
                "origin must be bar 1 (change at {offset})"
            );
            assert_eq!(
                map.bar_at(Beat(offset)).bar,
                BarNumber(2),
                "the change must begin a new bar (change at {offset})"
            );
            assert!(map.bar_at(Beat(offset)).is_downbeat());
            // Bar 1 has exactly one downbeat: the origin, not the change.
            assert_eq!(map.bar_start(BarNumber(1)), Beat(0.0));
            assert_eq!(map.bar_start(BarNumber(2)), Beat(offset));
            // And the following 3/4 bar lands one bar-length later.
            assert_eq!(
                map.bar_at(Beat(offset + 3.0)).bar,
                BarNumber(3),
                "3/4 bar after the change (change at {offset})"
            );
        }
    }

    /// A segment that *is* an exact multiple of its bar length must not gain a
    /// spurious extra bar from the round-up — the case `BAR_COUNT_EPSILON` exists
    /// for.
    #[test]
    fn map_does_not_over_count_an_exact_segment() {
        let map = MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::default()),
            // Exactly four 4/4 bars.
            MeterChange::new(
                Beat(16.0),
                TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER),
            ),
        ]);
        assert_eq!(map.bar_at(Beat(16.0)).bar, BarNumber(5));
        assert_eq!(map.bar_start(BarNumber(5)), Beat(16.0));
        // No gap: bar 4 is the last 4/4 bar and starts at 12.
        assert_eq!(map.bar_start(BarNumber(4)), Beat(12.0));
    }

    /// Deserialization must route through the validating constructors.
    ///
    /// A derived `Deserialize` writes the private field directly, so a document
    /// claiming `0/0` would produce a value the type says is impossible and make
    /// `bar_length` `NaN` — on the audio thread, via the metronome.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialization_clamps_hostile_values() {
        let sig: TimeSignature =
            serde_json::from_str(r#"{"beats_per_bar":0,"note_value":0}"#).unwrap();
        assert_eq!(sig.beats_per_bar().get(), 1);
        assert_eq!(sig.note_value().get(), 1);
        assert!(sig.bar_length().get().is_finite());

        let sig: TimeSignature =
            serde_json::from_str(r#"{"beats_per_bar":9999,"note_value":9999}"#).unwrap();
        assert_eq!(sig.beats_per_bar().get(), MAX_BEATS_PER_BAR);
        assert_eq!(sig.note_value().get(), MAX_NOTE_VALUE);

        // And the round trip still holds for legal values.
        let seven_eight = seven_eight();
        let json = serde_json::to_string(&seven_eight).unwrap();
        assert_eq!(
            serde_json::from_str::<TimeSignature>(&json).unwrap(),
            seven_eight
        );
    }

    /// Non-finite beats are dropped rather than poisoning the map: a NaN makes
    /// the sort non-transitive, survives dedup, and breaks `PartialEq`
    /// reflexivity — which would silently defeat the projection's change gate.
    #[test]
    fn map_drops_non_finite_changes() {
        let map = MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::default()),
            MeterChange::new(Beat(f64::NAN), seven_eight()),
            MeterChange::new(Beat(f64::INFINITY), seven_eight()),
            MeterChange::new(Beat(8.0), seven_eight()),
        ]);
        assert_eq!(map.changes().len(), 2, "NaN and inf must be dropped");
        assert!(map.changes().iter().all(|c| c.beat.get().is_finite()));
        assert_eq!(map, map.clone(), "PartialEq must stay reflexive");
        assert!(map.bar_at(Beat(4.0)).bar_start.get().is_finite());
    }

    #[test]
    fn bar_number_is_an_affine_space() {
        // Subtracting two positions gives a displacement; adding a displacement
        // to a position gives a position. `BarNumber + BarNumber` is deliberately
        // absent, exactly as `Beat + Beat` is.
        assert_eq!(BarNumber(7) - BarNumber(3), BarCount(4));
        assert_eq!(BarNumber(3) + BarCount(4), BarNumber(7));
        assert_eq!(BarNumber(7) - BarCount(4), BarNumber(3));

        // `BarCount` composes with itself, negates, and scales by a whole number.
        assert_eq!(BarCount(2) + BarCount(3), BarCount(5));
        assert_eq!(BarCount(5) - BarCount(3), BarCount(2));
        assert_eq!(-BarCount(4), BarCount(-4));
        assert_eq!(BarCount(3) * 4, BarCount(12));
        assert_eq!(4 * BarCount(3), BarCount(12));

        let mut c = BarCount(2);
        c += BarCount(3);
        assert_eq!(c, BarCount(5));
        c -= BarCount(1);
        assert_eq!(c, BarCount(4));

        // The constructors every other unit newtype in this crate also has.
        assert_eq!(BarNumber::new(3).get(), 3);
        assert_eq!(BarCount::new(3).get(), 3);
        assert_eq!(BarNumber(2).to_string(), "2");
        assert_eq!(BarCount(2).to_string(), "2");

        let mut b = BarNumber::FIRST;
        b -= BarCount(1);
        assert_eq!(b, BarNumber(0), "pre-roll bars are legal");
        b += BarCount(1);
        b += BarCount(3);
        assert_eq!(b, BarNumber(4));

        // The 1-based/0-based conversion lives in one place.
        assert_eq!(BarNumber::FIRST.index(), 0);
        assert_eq!(BarNumber::from_index(0), BarNumber::FIRST);
    }

    #[test]
    fn abi_conversions_coerce_hostile_input() {
        // The CLAP boundary used to do `as u16` on an i32: -1 became 65535.
        assert_eq!(BeatsPerBar::from(-1i32).get(), 1);
        assert_eq!(BeatsPerBar::from(i32::MAX).get(), MAX_BEATS_PER_BAR);
        assert_eq!(NoteValue::from(-1i32).get(), 1);
        assert_eq!(NoteValue::from(0i32).get(), 1);
        // And the widths the three plugin ABIs actually want.
        assert_eq!(u16::from(BeatsPerBar::new(7)), 7u16);
        assert_eq!(i32::from(NoteValue::EIGHTH), 8i32);
        assert_eq!(i32::from(BarNumber(3)), 3i32);
    }

    #[test]
    fn display_reads_as_music() {
        assert_eq!(seven_eight().to_string(), "7/8");
        assert_eq!(TimeSignature::default().to_string(), "4/4");
    }
}
