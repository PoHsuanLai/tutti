//! What an `AudioUnitParameterUnit` code measures, as a [`ParamUnit`] and a
//! [`UnitParam`] role (doc 023 §5.2).
//!
//! AU's unit is a numeric code from a closed enumeration, the most honest
//! declaration of the four formats. The codes are plain numbers, so this module
//! is not macOS-gated and its table is tested on every platform; on macOS a test
//! cross-checks each number against `coreaudio-sys`.
//!
//! Source of the table: Apple's `AudioUnitProperties.h`, enum
//! `AudioUnitParameterUnit`, 27 entries with explicit values 0 to 26, read from
//! a public copy of the macOS 10.15 SDK header (`phracker/MacOSX-SDKs`) on
//! 2026-11-11. The doc comments there fix each meaning: *Phase* is degrees
//! (-180 to 180), *Degrees* is the same range "but more general", *Meters* is a
//! **distance** in meters (not a level meter), *Beats* is time relative to
//! tempo, *Percent* is "usually from 0 -> 100, sometimes -50 -> +50".

use tutti_plugin_types::ParamUnit;
use tutti_types::UnitParam;

/// `kAudioUnitParameterUnit_*`, by value (`AudioUnitProperties.h`).
mod code {
    pub(super) const GENERIC: u32 = 0;
    pub(super) const INDEXED: u32 = 1;
    pub(super) const BOOLEAN: u32 = 2;
    pub(super) const PERCENT: u32 = 3;
    pub(super) const SECONDS: u32 = 4;
    pub(super) const SAMPLE_FRAMES: u32 = 5;
    pub(super) const PHASE: u32 = 6;
    pub(super) const RATE: u32 = 7;
    pub(super) const HERTZ: u32 = 8;
    pub(super) const CENTS: u32 = 9;
    pub(super) const RELATIVE_SEMITONES: u32 = 10;
    pub(super) const MIDI_NOTE_NUMBER: u32 = 11;
    pub(super) const MIDI_CONTROLLER: u32 = 12;
    pub(super) const DECIBELS: u32 = 13;
    pub(super) const LINEAR_GAIN: u32 = 14;
    pub(super) const DEGREES: u32 = 15;
    pub(super) const EQUAL_POWER_CROSSFADE: u32 = 16;
    pub(super) const MIXER_FADER_CURVE_1: u32 = 17;
    pub(super) const PAN: u32 = 18;
    pub(super) const METERS: u32 = 19;
    pub(super) const ABSOLUTE_CENTS: u32 = 20;
    pub(super) const OCTAVES: u32 = 21;
    pub(super) const BPM: u32 = 22;
    pub(super) const BEATS: u32 = 23;
    pub(super) const MILLISECONDS: u32 = 24;
    pub(super) const RATIO: u32 = 25;
    pub(super) const CUSTOM_UNIT: u32 = 26;
}

/// What the unit code `raw` (`AudioUnitParameterInfo::unit`) measures.
///
/// `Some` for 18 of the 27 codes: hertz, decibels, seconds, milliseconds,
/// percent, cents, relative semitones, octaves, BPM, beats, degrees, pan,
/// linear gain, rate, MIDI note number and MIDI controller, and `Indexed`
/// (`Choice`) and `Boolean` (`Toggle`). Generic, custom, equal-power
/// crossfade, mixer fader curve, absolute cents, meters (a distance), phase,
/// ratio and sample frames are `None`: their algebra is not one of ours, or the
/// code does not say enough. An unknown code is `None`.
///
/// The range check ([`ParamUnit::fits`]) is the caller's, through
/// [`ParameterInfo::with_measures`](tutti_plugin_types::ParameterInfo::with_measures).
#[must_use]
pub fn unit_measures(raw: u32) -> Option<ParamUnit> {
    Some(match raw {
        code::INDEXED => ParamUnit::Choice,
        code::BOOLEAN => ParamUnit::Toggle,
        code::PERCENT => ParamUnit::Percent,
        code::SECONDS => ParamUnit::Seconds,
        code::RATE => ParamUnit::PlaybackRate,
        code::HERTZ => ParamUnit::Hertz,
        code::CENTS => ParamUnit::Cents,
        code::RELATIVE_SEMITONES => ParamUnit::Semitones,
        code::MIDI_NOTE_NUMBER => ParamUnit::MidiNote,
        code::MIDI_CONTROLLER => ParamUnit::MidiController,
        code::DECIBELS => ParamUnit::Decibels,
        code::LINEAR_GAIN => ParamUnit::LinearGain,
        code::DEGREES => ParamUnit::Degrees,
        code::PAN => ParamUnit::Pan,
        code::OCTAVES => ParamUnit::Octaves,
        code::BPM => ParamUnit::Bpm,
        code::BEATS => ParamUnit::Beats,
        code::MILLISECONDS => ParamUnit::Milliseconds,
        code::GENERIC
        | code::SAMPLE_FRAMES
        | code::PHASE
        | code::EQUAL_POWER_CROSSFADE
        | code::MIXER_FADER_CURVE_1
        | code::METERS
        | code::ABSOLUTE_CENTS
        | code::RATIO
        | code::CUSTOM_UNIT => return None,
        _ => return None,
    })
}

/// The role the unit code `raw` declares: `Pan` for the pan code, nothing else.
/// AU has no per-parameter bypass flag (`BypassEffect` is a unit property), so
/// no parameter is a `Bypass` from here.
#[must_use]
pub fn unit_role(raw: u32) -> Option<UnitParam> {
    (raw == code::PAN).then_some(UnitParam::Pan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ParamUnit as U;

    /// Every one of the 27 codes, by its value in Apple's header. `None` rows
    /// are the nine whose algebra is not ours.
    ///
    /// Mutation (run, 2026-11-11): the `code::OCTAVES` arm's `ParamUnit::Octaves`
    /// changed to `ParamUnit::Cents` → the 21 row fails; the `code::BEATS` arm
    /// removed (falling to `_ => None`) → the 23 row fails.
    #[test]
    fn every_code_maps_per_the_table() {
        let table: [(u32, Option<ParamUnit>); 27] = [
            (0, None),                     // Generic
            (1, Some(U::Choice)),          // Indexed
            (2, Some(U::Toggle)),          // Boolean
            (3, Some(U::Percent)),         // Percent
            (4, Some(U::Seconds)),         // Seconds
            (5, None),                     // SampleFrames
            (6, None),                     // Phase
            (7, Some(U::PlaybackRate)),    // Rate
            (8, Some(U::Hertz)),           // Hertz
            (9, Some(U::Cents)),           // Cents
            (10, Some(U::Semitones)),      // RelativeSemiTones
            (11, Some(U::MidiNote)),       // MIDINoteNumber
            (12, Some(U::MidiController)), // MIDIController
            (13, Some(U::Decibels)),       // Decibels
            (14, Some(U::LinearGain)),     // LinearGain
            (15, Some(U::Degrees)),        // Degrees
            (16, None),                    // EqualPowerCrossfade
            (17, None),                    // MixerFaderCurve1
            (18, Some(U::Pan)),            // Pan
            (19, None),                    // Meters (a distance)
            (20, None),                    // AbsoluteCents
            (21, Some(U::Octaves)),        // Octaves
            (22, Some(U::Bpm)),            // BPM
            (23, Some(U::Beats)),          // Beats
            (24, Some(U::Milliseconds)),   // Milliseconds
            (25, None),                    // Ratio
            (26, None),                    // CustomUnit
        ];
        for (raw, want) in table {
            assert_eq!(unit_measures(raw), want, "code {raw}");
        }
        assert_eq!(table.iter().filter(|(_, w)| w.is_some()).count(), 18);
    }

    /// A code outside the enumeration is not a unit.
    ///
    /// Mutation (run, 2026-11-11): the match's catch-all `_ => return None`
    /// changed to `_ => ParamUnit::Hertz` → code 27 and up map, and this test
    /// fails (the table test covers 0..=26 only).
    #[test]
    fn an_unknown_code_is_none() {
        for raw in [27, 28, 100, 0x6d73_7264, u32::MAX] {
            assert_eq!(unit_measures(raw), None, "code {raw}");
        }
    }

    /// Only the pan code declares a role.
    ///
    /// Mutation (run): `raw == code::PAN` changed to `raw == code::PHASE` →
    /// both rows fail.
    #[test]
    fn only_the_pan_code_is_a_role() {
        assert_eq!(unit_role(18), Some(UnitParam::Pan));
        for raw in (0..27).filter(|&r| r != 18) {
            assert_eq!(unit_role(raw), None, "code {raw}");
        }
    }

    /// The numbers above are Apple's: each constant equals the SDK's.
    ///
    /// Unchecked by a compiler on the machine that wrote it (macOS only).
    #[cfg(target_os = "macos")]
    #[test]
    fn the_codes_are_coreaudios() {
        use coreaudio_sys as sys;
        let pairs: [(u32, u32); 27] = [
            (code::GENERIC, sys::kAudioUnitParameterUnit_Generic),
            (code::INDEXED, sys::kAudioUnitParameterUnit_Indexed),
            (code::BOOLEAN, sys::kAudioUnitParameterUnit_Boolean),
            (code::PERCENT, sys::kAudioUnitParameterUnit_Percent),
            (code::SECONDS, sys::kAudioUnitParameterUnit_Seconds),
            (
                code::SAMPLE_FRAMES,
                sys::kAudioUnitParameterUnit_SampleFrames,
            ),
            (code::PHASE, sys::kAudioUnitParameterUnit_Phase),
            (code::RATE, sys::kAudioUnitParameterUnit_Rate),
            (code::HERTZ, sys::kAudioUnitParameterUnit_Hertz),
            (code::CENTS, sys::kAudioUnitParameterUnit_Cents),
            (
                code::RELATIVE_SEMITONES,
                sys::kAudioUnitParameterUnit_RelativeSemiTones,
            ),
            (
                code::MIDI_NOTE_NUMBER,
                sys::kAudioUnitParameterUnit_MIDINoteNumber,
            ),
            (
                code::MIDI_CONTROLLER,
                sys::kAudioUnitParameterUnit_MIDIController,
            ),
            (code::DECIBELS, sys::kAudioUnitParameterUnit_Decibels),
            (code::LINEAR_GAIN, sys::kAudioUnitParameterUnit_LinearGain),
            (code::DEGREES, sys::kAudioUnitParameterUnit_Degrees),
            (
                code::EQUAL_POWER_CROSSFADE,
                sys::kAudioUnitParameterUnit_EqualPowerCrossfade,
            ),
            (
                code::MIXER_FADER_CURVE_1,
                sys::kAudioUnitParameterUnit_MixerFaderCurve1,
            ),
            (code::PAN, sys::kAudioUnitParameterUnit_Pan),
            (code::METERS, sys::kAudioUnitParameterUnit_Meters),
            (
                code::ABSOLUTE_CENTS,
                sys::kAudioUnitParameterUnit_AbsoluteCents,
            ),
            (code::OCTAVES, sys::kAudioUnitParameterUnit_Octaves),
            (code::BPM, sys::kAudioUnitParameterUnit_BPM),
            (code::BEATS, sys::kAudioUnitParameterUnit_Beats),
            (
                code::MILLISECONDS,
                sys::kAudioUnitParameterUnit_Milliseconds,
            ),
            (code::RATIO, sys::kAudioUnitParameterUnit_Ratio),
            (code::CUSTOM_UNIT, sys::kAudioUnitParameterUnit_CustomUnit),
        ];
        for (ours, apple) in pairs {
            assert_eq!(ours, apple);
        }
    }
}
