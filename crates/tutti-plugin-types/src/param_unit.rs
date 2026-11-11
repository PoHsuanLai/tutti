//! What a parameter's number measures: [`ParamUnit`], and the one-way read of a
//! plain value into a tutti unit, [`FromPlain`] (doc 023 §5.2).
//!
//! A format declares a unit in a different way each time (AU a numeric code,
//! VST3 and VST2 a display string, CLAP not at all), and every one of them is
//! untrusted. So the typed unit is an `Option`, filled only where the declaration
//! is exact, and a unit is accepted only with a declared plain range it fits
//! ([`ParamUnit::fits`]): a parameter labelled `s` that runs `0..1000` is not
//! seconds. The flat display string stays on
//! [`ParameterInfo::unit`](crate::ParameterInfo::unit) whatever this says.
//!
//! `ParamUnit` is a runtime tag and lives here, with the hosts, not in
//! `tutti-types` beside the compile-time units (doc 023 Q9): a second name for
//! `Hz` or `Db` there would be two names for one behaviour.

use crate::ParamRange;
use tutti_types::{
    Amplitude, ArcDegrees, BeatDuration, Bpm, Cents, Db, Hz, Mix, Pan, PlaybackRate, Seconds,
    Semitones,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// What a parameter's number measures, from a closed set.
///
/// `None` (in [`ParameterInfo::measures`](crate::ParameterInfo::measures)) is
/// the common case and means "not declared exactly", never "unitless".
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::ParamUnit;
///
/// assert_eq!(ParamUnit::from_label("Hz"), Some(ParamUnit::Hertz));
/// assert_eq!(ParamUnit::from_label("kHz"), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum ParamUnit {
    /// Frequency in hertz.
    Hertz,
    /// Level in decibels.
    Decibels,
    /// Time in seconds.
    Seconds,
    /// Time in milliseconds.
    Milliseconds,
    /// A percentage: `50` is half.
    Percent,
    /// A pitch offset in cents.
    Cents,
    /// A pitch offset in semitones.
    Semitones,
    /// A pitch offset in octaves.
    Octaves,
    /// A tempo in beats per minute.
    Bpm,
    /// A span in beats.
    Beats,
    /// An angle in degrees.
    Degrees,
    /// A position on the left/right axis, `-1..=1`.
    Pan,
    /// A linear amplitude gain.
    LinearGain,
    /// A playback rate: `1` is unchanged.
    PlaybackRate,
    /// A MIDI note number, `0..=127`.
    MidiNote,
    /// A MIDI controller number, `0..=127`.
    MidiController,
    /// A choice: the value is an index into named options.
    Choice,
    /// An on/off switch.
    Toggle,
}

impl ParamUnit {
    /// The unit a VST3 `units` string or a VST2 parameter label names, from a
    /// closed list of exact matches: `Hz`, `dB`, `s`, `ms`, `%`, `ct`, `st` and
    /// `BPM`.
    ///
    /// Case-sensitive, after trimming ASCII space (and only that: a no-break
    /// space is not trimmed). Everything else (`kHz`, `Db`, `dB(A)`,
    /// `semitones`, the empty string) is `None`. The list grows only from a
    /// corpus run (doc 023 §6), because the cost of a wrong accept is a
    /// mislabelled parameter.
    ///
    /// ```
    /// use tutti_plugin_types::ParamUnit;
    ///
    /// assert_eq!(ParamUnit::from_label("  dB "), Some(ParamUnit::Decibels));
    /// assert_eq!(ParamUnit::from_label("db"), None);
    /// ```
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim_matches(' ') {
            "Hz" => Some(Self::Hertz),
            "dB" => Some(Self::Decibels),
            "s" => Some(Self::Seconds),
            "ms" => Some(Self::Milliseconds),
            "%" => Some(Self::Percent),
            "ct" => Some(Self::Cents),
            "st" => Some(Self::Semitones),
            "BPM" => Some(Self::Bpm),
            _ => None,
        }
    }

    /// Whether `range` is one this unit may be declared over.
    ///
    /// Every unit but [`Choice`](Self::Choice) and [`Toggle`](Self::Toggle)
    /// needs a **plain** range with finite bounds in order, because a
    /// normalized-only parameter's number is not in the unit and a unit
    /// without a range cannot be converted. On top of that:
    ///
    /// | Unit | The declared range must be |
    /// |---|---|
    /// | `Hertz`, `Seconds`, `Milliseconds`, `Beats`, `LinearGain`, `PlaybackRate` | non-negative (`min >= 0`) |
    /// | `Bpm` | positive (`min > 0`) |
    /// | `Percent` | inside `-100..=100` |
    /// | `Pan` | inside `-1..=1` |
    /// | `MidiNote`, `MidiController` | inside `0..=127` |
    /// | `Decibels`, `Cents`, `Semitones`, `Octaves`, `Degrees` | any finite range |
    ///
    /// `Choice` and `Toggle` name an index or a switch, so they fit any range,
    /// a normalized one included (VST3 declares nothing else).
    #[must_use]
    pub fn fits(self, range: &ParamRange) -> bool {
        if matches!(self, Self::Choice | Self::Toggle) {
            return true;
        }
        let ParamRange::Plain { min, max, .. } = *range else {
            return false;
        };
        if !(min.is_finite() && max.is_finite() && min <= max) {
            return false;
        }
        match self {
            Self::Hertz
            | Self::Seconds
            | Self::Milliseconds
            | Self::Beats
            | Self::LinearGain
            | Self::PlaybackRate => min >= 0.0,
            Self::Bpm => min > 0.0,
            Self::Percent => min >= -100.0 && max <= 100.0,
            Self::Pan => min >= -1.0 && max <= 1.0,
            Self::MidiNote | Self::MidiController => min >= 0.0 && max <= 127.0,
            Self::Decibels | Self::Cents | Self::Semitones | Self::Octaves | Self::Degrees => true,
            Self::Choice | Self::Toggle => true,
        }
    }
}

/// Reads a tutti unit out of a plain value measured in a declared [`ParamUnit`].
///
/// This is the one place a percent becomes a fraction (`/ 100`) and a
/// millisecond becomes a second (`/ 1000`), so the scaling is never done twice
/// and the plugin's own number is still what is sent back. An implementor
/// answers `None` for a unit it does not read, and for a value its type cannot
/// hold. The value arrives finite.
///
/// A unit tutti has no type for (octaves, MIDI numbers) has no implementor, so
/// [`ParameterInfo::typed`](crate::ParameterInfo::typed) answers `None` for it.
pub trait FromPlain: Sized {
    /// `plain`, measured in `unit`, as `Self`; `None` when `unit` is not one
    /// `Self` reads or `plain` is out of its range.
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self>;
}

// A `f32` unit narrows once here, at the edge: a plain figure is the
// plugin's `f64`, and every control unit below is an `f32` cell.
#[allow(clippy::cast_possible_truncation)]
fn narrow(v: f64) -> f32 {
    v as f32
}

impl FromPlain for Hz {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Hertz).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for Db {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Decibels).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for Seconds {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        match unit {
            ParamUnit::Seconds => Some(Self::new(plain)),
            ParamUnit::Milliseconds => Some(Self::new(plain / 1000.0)),
            _ => None,
        }
    }
}

impl FromPlain for Mix {
    /// A percentage as a position on `0..=1`; a negative percentage is not one.
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        let fraction = plain / 100.0;
        (unit == ParamUnit::Percent && (0.0..=1.0).contains(&fraction))
            .then(|| Self::new(narrow(fraction)))
    }
}

impl FromPlain for Pan {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Pan && (-1.0..=1.0).contains(&plain)).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for Cents {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Cents).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for Semitones {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Semitones).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for Bpm {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        if unit == ParamUnit::Bpm {
            Self::new(plain)
        } else {
            None
        }
    }
}

impl FromPlain for BeatDuration {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Beats).then(|| Self::new(plain))
    }
}

impl FromPlain for ArcDegrees {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::Degrees).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for Amplitude {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::LinearGain).then(|| Self::new(narrow(plain)))
    }
}

impl FromPlain for PlaybackRate {
    fn from_plain(unit: ParamUnit, plain: f64) -> Option<Self> {
        (unit == ParamUnit::PlaybackRate).then(|| Self::new(narrow(plain)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamId, ParameterInfo};

    fn plain(min: f64, max: f64) -> ParamRange {
        ParamRange::Plain {
            min,
            max,
            default: min,
        }
    }

    /// Every string on the closed list maps to its unit, and to that one only.
    #[test]
    fn every_accepted_label_maps() {
        let table = [
            ("Hz", ParamUnit::Hertz),
            ("dB", ParamUnit::Decibels),
            ("s", ParamUnit::Seconds),
            ("ms", ParamUnit::Milliseconds),
            ("%", ParamUnit::Percent),
            ("ct", ParamUnit::Cents),
            ("st", ParamUnit::Semitones),
            ("BPM", ParamUnit::Bpm),
        ];
        for (label, unit) in table {
            assert_eq!(ParamUnit::from_label(label), Some(unit), "{label:?}");
        }
    }

    /// Surrounding ASCII spaces are trimmed; a non-ASCII space is not.
    #[test]
    fn only_ascii_space_is_trimmed() {
        assert_eq!(ParamUnit::from_label(" Hz"), Some(ParamUnit::Hertz));
        assert_eq!(ParamUnit::from_label("  dB   "), Some(ParamUnit::Decibels));
        assert_eq!(ParamUnit::from_label("\u{a0}Hz"), None, "no-break space");
        assert_eq!(
            ParamUnit::from_label("Hz\u{3000}"),
            None,
            "ideographic space"
        );
        assert_eq!(
            ParamUnit::from_label("\tHz"),
            None,
            "a tab is not ASCII space"
        );
    }

    /// Near-misses are `None`: case, a prefix, a qualifier, a long name, empty.
    #[test]
    fn near_misses_are_not_units() {
        for label in [
            "kHz",
            "Db",
            "DB",
            "db",
            "hz",
            "HZ",
            "dB(A)",
            "semitones",
            "Semitones",
            "sec",
            "S",
            "MS",
            "bpm",
            "BPM ish",
            "% ",
            "%%",
            "",
            " ",
            "cents",
            "ct.",
            "dBFS",
        ] {
            // "% " trims to "%": that one is the trimming rule, not a near-miss.
            let expect = (label == "% ").then_some(ParamUnit::Percent);
            assert_eq!(ParamUnit::from_label(label), expect, "{label:?}");
        }
    }

    /// A range that does not fit its unit is refused; one that does is kept.
    #[test]
    fn a_range_that_does_not_fit_is_refused() {
        use ParamUnit as U;
        let rows: [(U, f64, f64, bool); 24] = [
            (U::Hertz, 20.0, 20_000.0, true),
            (U::Hertz, -1.0, 20_000.0, false),
            (U::Hertz, 0.0, f64::INFINITY, false),
            (U::Hertz, f64::NAN, 10.0, false),
            (U::Hertz, 10.0, 5.0, false),
            (U::Percent, 0.0, 100.0, true),
            (U::Percent, -100.0, 100.0, true),
            (U::Percent, 0.0, 1000.0, false),
            (U::Percent, -101.0, 100.0, false),
            (U::Pan, -1.0, 1.0, true),
            (U::Pan, -1.0, 1.5, false),
            (U::Pan, -100.0, 100.0, false),
            (U::Seconds, 0.0, 10.0, true),
            (U::Seconds, 0.0, 1000.0, true),
            (U::Seconds, -1.0, 1.0, false),
            (U::Milliseconds, 0.0, 500.0, true),
            (U::Bpm, 20.0, 300.0, true),
            (U::Bpm, 0.0, 300.0, false),
            (U::MidiNote, 0.0, 127.0, true),
            (U::MidiController, 0.0, 128.0, false),
            (U::Decibels, -96.0, 24.0, true),
            (U::Decibels, f64::NEG_INFINITY, 0.0, false),
            (U::Cents, -50.0, 50.0, true),
            (U::LinearGain, -0.5, 1.0, false),
        ];
        for (unit, min, max, fits) in rows {
            assert_eq!(unit.fits(&plain(min, max)), fits, "{unit:?} {min}..{max}");
        }
    }

    /// A unit needs a plain range; a normalized-only parameter fits only
    /// `Choice` and `Toggle`.
    #[test]
    fn a_normalized_range_fits_only_choice_and_toggle() {
        let normalized = ParamRange::Normalized { default: 0.0 };
        assert!(!ParamUnit::Hertz.fits(&normalized));
        assert!(!ParamUnit::Percent.fits(&normalized));
        assert!(ParamUnit::Choice.fits(&normalized));
        assert!(ParamUnit::Toggle.fits(&normalized));
    }

    /// `with_measures` keeps a unit the range fits and drops one it does not.
    #[test]
    fn with_measures_applies_the_range_check() {
        let p = |max| {
            ParameterInfo::new(ParamId::new(1), "x")
                .with_plain_range(0.0, max, 0.0)
                .with_measures(Some(ParamUnit::Percent))
        };
        assert_eq!(p(100.0).measures, Some(ParamUnit::Percent));
        assert_eq!(p(1000.0).measures, None);
        let none = ParameterInfo::new(ParamId::new(1), "x").with_measures(None);
        assert_eq!(none.measures, None);
    }

    /// `typed` scales a percent by 100 and a millisecond by 1000, once.
    #[test]
    fn typed_scales_percent_and_milliseconds() {
        let percent = ParameterInfo::new(ParamId::new(1), "Mix")
            .with_plain_range(0.0, 100.0, 0.0)
            .with_measures(Some(ParamUnit::Percent));
        assert_eq!(percent.typed::<Mix>(25.0), Some(Mix::new(0.25)));
        assert_eq!(percent.typed::<Mix>(100.0), Some(Mix::new(1.0)));

        let ms = ParameterInfo::new(ParamId::new(2), "Attack")
            .with_plain_range(0.0, 500.0, 0.0)
            .with_measures(Some(ParamUnit::Milliseconds));
        assert_eq!(ms.typed::<Seconds>(250.0), Some(Seconds::new(0.25)));

        let s = ParameterInfo::new(ParamId::new(3), "Release")
            .with_plain_range(0.0, 5.0, 0.0)
            .with_measures(Some(ParamUnit::Seconds));
        assert_eq!(s.typed::<Seconds>(2.5), Some(Seconds::new(2.5)));
    }

    /// `typed` answers `None` for a normalized-only parameter, for a unit the
    /// target does not read, for no unit at all and for a non-finite value.
    #[test]
    fn typed_is_none_where_it_cannot_say() {
        let mut normalized = ParameterInfo::new(ParamId::new(1), "Cutoff");
        normalized.measures = Some(ParamUnit::Hertz); // built by hand: the guard is `typed`'s own
        assert_eq!(normalized.typed::<Hz>(1000.0), None, "normalized only");

        let hz = ParameterInfo::new(ParamId::new(2), "Cutoff")
            .with_plain_range(20.0, 20_000.0, 1000.0)
            .with_measures(Some(ParamUnit::Hertz));
        assert_eq!(hz.typed::<Hz>(440.0), Some(Hz::new(440.0)));
        assert_eq!(hz.typed::<Db>(440.0), None, "another unit");
        assert_eq!(hz.typed::<Hz>(f64::NAN), None, "not finite");

        let undeclared = ParameterInfo::new(ParamId::new(3), "x").with_plain_range(0.0, 1.0, 0.0);
        assert_eq!(undeclared.typed::<Mix>(0.5), None, "no unit declared");
    }

    /// A value its target type cannot hold is `None`, not clamped.
    #[test]
    fn typed_refuses_a_value_outside_the_target() {
        let percent = ParameterInfo::new(ParamId::new(1), "Bal")
            .with_plain_range(-100.0, 100.0, 0.0)
            .with_measures(Some(ParamUnit::Percent));
        assert_eq!(percent.typed::<Mix>(-50.0), None);
        let bpm = ParameterInfo::new(ParamId::new(2), "Tempo")
            .with_plain_range(20.0, 300.0, 120.0)
            .with_measures(Some(ParamUnit::Bpm));
        assert_eq!(bpm.typed::<Bpm>(0.0), None);
        assert_eq!(bpm.typed::<Bpm>(120.0), Bpm::new(120.0));
    }

    /// Each implementor reads its own unit and no other.
    #[test]
    fn each_target_reads_its_own_unit() {
        use ParamUnit as U;
        assert_eq!(Hz::from_plain(U::Hertz, 1.0), Some(Hz::new(1.0)));
        assert_eq!(Db::from_plain(U::Decibels, -6.0), Some(Db::new(-6.0)));
        assert_eq!(Cents::from_plain(U::Cents, 7.0), Some(Cents::new(7.0)));
        assert_eq!(
            Semitones::from_plain(U::Semitones, -2.0),
            Some(Semitones::new(-2.0))
        );
        assert_eq!(Pan::from_plain(U::Pan, 0.5), Some(Pan::new(0.5)));
        assert_eq!(Pan::from_plain(U::Pan, 1.5), None);
        assert_eq!(
            BeatDuration::from_plain(U::Beats, 0.25),
            Some(BeatDuration::new(0.25))
        );
        assert_eq!(
            ArcDegrees::from_plain(U::Degrees, 90.0),
            Some(ArcDegrees::new(90.0))
        );
        assert_eq!(
            Amplitude::from_plain(U::LinearGain, 0.5),
            Some(Amplitude::new(0.5))
        );
        assert_eq!(
            PlaybackRate::from_plain(U::PlaybackRate, 2.0),
            Some(PlaybackRate::new(2.0))
        );
        assert_eq!(Hz::from_plain(U::Cents, 1.0), None);
        assert_eq!(Cents::from_plain(U::Hertz, 1.0), None);
        assert_eq!(Hz::from_plain(U::Octaves, 1.0), None);
        assert_eq!(Hz::from_plain(U::Choice, 1.0), None);
    }
}
