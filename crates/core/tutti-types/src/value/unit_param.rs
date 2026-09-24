//! [`UnitParam`] — the stable, id-addressable vocabulary of every scalar a
//! built-in tutti unit may expose.
//!
//! One uniform *name* per scalar, so a host addresses "cutoff" or "threshold"
//! without knowing a node's concrete type. The alternative is bespoke typed
//! setters (`set_frequency`, `set_q`, `set_mix`, …) reached by downcasting,
//! which makes every generic host path a match on concrete node types.
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
    /// Stereo balance, `-1..1` (left to right). The mixer-strip control.
    ///
    /// Distinct from [`StereoSpread`](Self::StereoSpread), which widens a source
    /// about its centre; this moves the centre. Also distinct from fundsp's
    /// `Parameter::Pan`, which addresses its mono-to-stereo `Panner` through a
    /// different channel entirely — this id rides the ordinary
    /// `Setting::value(..).index(..)` path like every other `UnitParam`, which is
    /// what makes it reachable from a generic param reconciler.
    Pan = 20,
    /// Mute toggle: **`>= 0.5` is muted**, below is unmuted.
    ///
    /// The threshold encoding is forced, not chosen: `Setting` carries an `f32`,
    /// so a boolean has to ride one. Stated here because the decode lives in each
    /// unit's `set` and a unit that picked `!= 0.0` instead would mute on a
    /// denormal.
    Mute = 21,
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
            20 => Pan,
            21 => Mute,
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

/// A [`UnitParam`] that knows the unit its value is in.
///
/// `UnitParam` alone is untyped: `Cutoff` and `GainDb` both carry an `f32`,
/// and nothing stops a caller writing a decibel figure to a cutoff. A
/// `ParamKey<U>` pins the unit, so a value travelling under a key — an
/// automation ramp in `tutti-graph`, for one — is built from a `U` and read
/// back as a `U`, with the `f32` in between never exposed.
///
/// The keys exist only for parameters whose unit is not in doubt, as
/// associated constants on the unit: `ParamKey::<Hz>::CUTOFF`,
/// `ParamKey::<Db>::THRESHOLD`. There is deliberately **no key** yet for
/// `DelayTime` ("beats or seconds — unit-defined"), `RoomSize` and `Damping`
/// (bare `0..1` with no unit type of their own), `StereoSpread` (whether it is
/// `Spread` or `StereoWidth` is the node's call today), `Pan` (the `Pan` type
/// is a *measurement*, and a pan control needs its own), and `Mute` (a switch,
/// not a scalar). Each needs its unit decided first; until then they stay
/// reachable through [`UnitParam`] and cannot be ramped by key.
///
/// ```compile_fail
/// use tutti_types::{Db, ParamKey};
/// let _ = ParamKey::<Db>::CUTOFF; // a cutoff is not in decibels
/// ```
pub struct ParamKey<U> {
    id: UnitParam,
    _unit: core::marker::PhantomData<fn() -> U>,
}

impl<U> ParamKey<U> {
    const fn of(id: UnitParam) -> Self {
        Self {
            id,
            _unit: core::marker::PhantomData,
        }
    }

    /// The untyped id, for a channel that carries it type-erased.
    #[inline]
    pub const fn id(self) -> UnitParam {
        self.id
    }
}

// Written out: the derives would demand `U: Clone`/`Eq`/…, and the float units
// cannot be `Eq`. The key's identity is its id alone.
impl<U> Clone for ParamKey<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for ParamKey<U> {}
impl<U> PartialEq for ParamKey<U> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<U> Eq for ParamKey<U> {}
impl<U> core::hash::Hash for ParamKey<U> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}
impl<U> core::fmt::Debug for ParamKey<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ParamKey<{}>({:?})",
            core::any::type_name::<U>(),
            self.id
        )
    }
}

impl<U> From<ParamKey<U>> for UnitParam {
    fn from(k: ParamKey<U>) -> Self {
        k.id
    }
}

use super::units::{
    Amplitude, Cents, CompressionRatio, Db, Depth, Drive, Feedback, Hz, Mix, Seconds, Q,
};

/// Defines the typed keys, and — from the same lines — the registry
/// [`ParamKey::registry`] lists, so a test can check every key at once and a
/// new key cannot be added without landing in it.
macro_rules! param_keys {
    ($($unit:ident { $($(#[$m:meta])* $name:ident = $id:ident;)+ })+) => {
        $(impl ParamKey<$unit> {
            $($(#[$m])* pub const $name: Self = Self::of(UnitParam::$id);)+
        })+

        /// Every keyed param, with the name of its unit type.
        const KEYED: &[(UnitParam, &str)] = &[
            $($((UnitParam::$id, stringify!($unit)),)+)+
        ];
    };
}

param_keys! {
    Hz {
        /// Filter cutoff / centre frequency.
        CUTOFF = Cutoff;
        /// Modulation rate.
        RATE = Rate;
    }
    Q {
        /// Filter Q.
        Q = Q;
    }
    Db {
        /// Filter / EQ gain.
        GAIN_DB = GainDb;
        /// Dynamics threshold.
        THRESHOLD = Threshold;
        /// Limiter ceiling.
        CEILING = Ceiling;
        /// Compressor make-up gain.
        MAKEUP = Makeup;
    }
    Mix {
        /// Wet/dry mix.
        WET = Wet;
    }
    Feedback {
        /// Feedback amount.
        FEEDBACK = Feedback;
    }
    Depth {
        /// Modulation depth.
        DEPTH = Depth;
    }
    CompressionRatio {
        /// Compressor ratio.
        RATIO = Ratio;
    }
    Seconds {
        /// Envelope attack.
        ATTACK = Attack;
        /// Envelope release.
        RELEASE = Release;
    }
    Drive {
        /// Drive / saturation amount.
        DRIVE = Drive;
    }
    Amplitude {
        /// Synth / master volume (linear).
        VOLUME = Volume;
    }
    Cents {
        /// Unison detune spread.
        DETUNE = Detune;
    }
}

impl ParamKey<()> {
    /// Every param that has a typed key, with the name of its unit.
    pub fn registry() -> &'static [(UnitParam, &'static str)] {
        KEYED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each typed key names the param its unit belongs to — a key is only as
    /// good as the pairing, and a swapped pair still compiles.
    ///
    /// Mutation: swap the ids of `ParamKey::<Db>::THRESHOLD` and
    /// `ParamKey::<Db>::CEILING` → fails.
    #[test]
    fn typed_keys_name_their_params() {
        use super::super::units::{Db, Hz, Seconds};
        assert_eq!(ParamKey::<Hz>::CUTOFF.id(), UnitParam::Cutoff);
        assert_eq!(ParamKey::<Hz>::RATE.id(), UnitParam::Rate);
        assert_eq!(ParamKey::<Db>::THRESHOLD.id(), UnitParam::Threshold);
        assert_eq!(ParamKey::<Db>::CEILING.id(), UnitParam::Ceiling);
        assert_eq!(ParamKey::<Db>::GAIN_DB.id(), UnitParam::GainDb);
        assert_eq!(ParamKey::<Db>::MAKEUP.id(), UnitParam::Makeup);
        assert_eq!(ParamKey::<Seconds>::ATTACK.id(), UnitParam::Attack);
        assert_eq!(ParamKey::<Seconds>::RELEASE.id(), UnitParam::Release);
        assert_eq!(UnitParam::from(ParamKey::<Hz>::CUTOFF), UnitParam::Cutoff);
        assert_ne!(ParamKey::<Hz>::CUTOFF, ParamKey::<Hz>::RATE);
    }

    /// Every keyed param has exactly one key: no id appears twice (under the
    /// same unit or under two), so a value can never be read back in a unit
    /// it was not written in.
    ///
    /// Mutation: add `WET = Wet;` under `Depth` in `param_keys!` → `Wet`
    /// appears twice → fails.
    #[test]
    fn every_keyed_param_has_one_unit() {
        let reg = ParamKey::registry();
        assert!(reg.len() >= 15, "the registry is the macro's full list");
        let mut ids: Vec<u16> = reg.iter().map(|&(p, _)| u16::from(p)).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "a UnitParam is keyed twice: {reg:?}");
    }

    #[test]
    fn u16_round_trips() {
        for id in 0..=21u16 {
            let p = UnitParam::try_from(id).expect("known id");
            assert_eq!(u16::from(p), id);
        }
    }

    /// The discriminants are **stable**: they ride `Setting` as an address index,
    /// so renumbering one silently re-points every setting built against the old
    /// value. Pinning the two newest by name is what
    /// [`u16_round_trips`](self::u16_round_trips) cannot do — that loop passes as
    /// long as each id maps to *some* variant, including a swapped pair.
    #[test]
    fn appended_params_keep_their_ids() {
        assert_eq!(u16::from(UnitParam::Pan), 20);
        assert_eq!(u16::from(UnitParam::Mute), 21);
        assert_eq!(UnitParam::try_from(20), Ok(UnitParam::Pan));
        assert_eq!(UnitParam::try_from(21), Ok(UnitParam::Mute));
        // The append must not have disturbed the tail of the existing set.
        assert_eq!(u16::from(UnitParam::Volume), 17);
        assert_eq!(u16::from(UnitParam::StereoSpread), 19);
    }

    #[test]
    fn unknown_id_is_err() {
        assert_eq!(UnitParam::try_from(9999), Err(UnitParamOutOfRange(9999)));
    }
}
