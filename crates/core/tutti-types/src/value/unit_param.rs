//! [`UnitParam`] — the stable, id-addressable vocabulary of every scalar a
//! built-in tutti unit may expose.
//!
//! Every tutti `*Node` historically exposed bespoke typed setters
//! (`set_frequency`, `set_q`, `set_mix`, …) that callers reached by downcasting
//! to the concrete type. `UnitParam` replaces that with a uniform *name* for
//! each scalar, so a host addresses "cutoff" or "threshold" without knowing the
//! node's concrete type.
//!
//! This module is the **pure vocabulary** half: the enum and its `u16`
//! conversions, with no audio-engine dependency. The other half — carrying a
//! `UnitParam` through fundsp's lock-free `Setting` channel (`setting` /
//! `from_setting`) — lives in `fundsp-tutti` (which owns `Setting`), and
//! `tutti-core` re-exports both so consumers reach them together.

/// The full set of scalar parameters any built-in tutti unit may expose.
///
/// Discriminants are **stable** — they ride through fundsp's `Setting` as an
/// address index and (potentially) persist in tooling, so existing values must
/// never be renumbered. Append new params at the end.
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum UnitParam {
    /// Filter cutoff / centre frequency (Hz).
    Cutoff = 0,
    /// Filter resonance / Q.
    Q = 1,
    /// Filter / EQ gain (dB).
    GainDb = 2,
    /// Wet/dry mix (0..1).
    Wet = 3,
    /// Feedback amount (0..1).
    Feedback = 4,
    /// Delay time (beats or seconds — unit-defined).
    DelayTime = 5,
    /// Modulation rate (Hz).
    Rate = 6,
    /// Modulation depth (0..1).
    Depth = 7,
    /// Reverb room size (0..1).
    RoomSize = 8,
    /// Reverb damping (0..1).
    Damping = 9,
    /// Dynamics threshold (dB).
    Threshold = 10,
    /// Compressor ratio (≥1).
    Ratio = 11,
    /// Envelope attack (seconds).
    Attack = 12,
    /// Envelope release (seconds).
    Release = 13,
    /// Limiter ceiling (dB).
    Ceiling = 14,
    /// Drive / saturation amount.
    Drive = 15,
    /// Compressor make-up gain (dB).
    Makeup = 16,
    /// Synth / master volume (linear, 0..1).
    Volume = 17,
    /// Unison detune spread (cents).
    Detune = 18,
    /// Unison stereo spread (0..1).
    StereoSpread = 19,
}

/// The `u16` id was not a known [`UnitParam`] discriminant. The scheme is
/// forward-compatible: an unknown id simply doesn't map to a param, so callers
/// treat this as "ignore" rather than a hard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitParamOutOfRange(pub u16);

impl core::fmt::Display for UnitParamOutOfRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "unknown UnitParam id: {}", self.0)
    }
}

impl std::error::Error for UnitParamOutOfRange {}

impl TryFrom<u16> for UnitParam {
    type Error = UnitParamOutOfRange;

    /// Reconstruct from the stable `u16` discriminant. `Err` for unknown ids.
    fn try_from(id: u16) -> Result<Self, Self::Error> {
        use UnitParam::*;
        Ok(match id {
            0 => Cutoff,
            1 => Q,
            2 => GainDb,
            3 => Wet,
            4 => Feedback,
            5 => DelayTime,
            6 => Rate,
            7 => Depth,
            8 => RoomSize,
            9 => Damping,
            10 => Threshold,
            11 => Ratio,
            12 => Attack,
            13 => Release,
            14 => Ceiling,
            15 => Drive,
            16 => Makeup,
            17 => Volume,
            18 => Detune,
            19 => StereoSpread,
            other => return Err(UnitParamOutOfRange(other)),
        })
    }
}

impl From<UnitParam> for u16 {
    #[inline]
    fn from(p: UnitParam) -> u16 {
        p as u16
    }
}

/// The address of a scalar parameter — either one from tutti's known
/// [`UnitParam`] vocabulary, or a foreign param named by an opaque numeric id.
///
/// [`UnitParam`] is a *closed* set: the params tutti's own units define. A unit
/// with params tutti does not model — a hosted plugin, a WASM unit, a scripted
/// node — has an *open* space of its own numeric ids that no fixed enum can
/// enumerate. `ParamAddr` unifies both so one API (e.g. `ModParams::mod_target`)
/// addresses either: a native node answers on [`Unit`](ParamAddr::Unit) and
/// ignores [`Id`](ParamAddr::Id); a foreign unit does the reverse.
///
/// Deliberately not "plugin"-named — the [`Id`](ParamAddr::Id) arm is any
/// externally-numbered param, not a plugin concept.
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamAddr {
    /// A param from tutti's known vocabulary.
    Unit(UnitParam),
    /// A param addressed by an opaque numeric id, foreign to tutti's vocabulary.
    Id(u32),
}

impl From<UnitParam> for ParamAddr {
    #[inline]
    fn from(p: UnitParam) -> Self {
        ParamAddr::Unit(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u16_round_trips() {
        for id in 0..=19u16 {
            let p = UnitParam::try_from(id).expect("known id");
            assert_eq!(u16::from(p), id);
        }
    }

    #[test]
    fn unknown_id_is_err() {
        assert_eq!(UnitParam::try_from(9999), Err(UnitParamOutOfRange(9999)));
    }
}
