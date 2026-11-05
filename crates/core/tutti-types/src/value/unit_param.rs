//! [`UnitParam`] — the stable, id-addressable vocabulary of every scalar a
//! built-in tutti unit may expose.
//!
//! One uniform *name* per scalar, so a host addresses "cutoff" or "threshold"
//! without knowing a node's concrete type. The alternative is bespoke typed
//! setters (`set_frequency`, `set_q`, `set_mix`, …) reached by downcasting,
//! which makes every generic host path a match on concrete node types.
//!
//! This module is the **pure vocabulary**: the enum, its `u16` conversions
//! and the addresses built on it ([`UnitAddr`], [`ParamAddr`]), with no
//! audio-engine dependency. A node maps each [`UnitAddr`] it exposes to one of
//! its `f32` cells in a `tutti_graph::ParamSet`, which is how a host sets a
//! param by address.

use core::num::NonZeroU8;

/// The full set of scalar parameters any built-in tutti unit may expose.
///
/// Discriminants are **stable**: they are the kind a `ParamSet` and a
/// host's `AudioParam<U, P>` name a param by (with an index among params of
/// the kind: [`UnitAddr`]), and may be persisted, so an existing value is
/// never renumbered. New params are appended at the end. They fit a byte
/// (`repr(u8)`): a [`UnitAddr`] packs its index above them in a `u16`.
///
/// ```
/// use tutti_types::UnitParam;
///
/// let id = u16::from(UnitParam::Cutoff);
/// assert_eq!(UnitParam::try_from(id), Ok(UnitParam::Cutoff));
/// assert!(UnitParam::try_from(9_999).is_err());
/// ```
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
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
    ///
    /// No engine node binds it: the compressor's make-up is its one gain
    /// cell, addressed as [`GainDb`](Self::GainDb) (one cell, one address),
    /// so a route or param on `Makeup` reaches nothing. Kept for a plugin
    /// or host node that names the address itself.
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
    /// about its centre; this moves the centre. It is addressed through a
    /// node's `ParamSet` like every other `UnitParam`, so a generic param
    /// reconciler reaches it too.
    Pan = 20,
    /// Mute toggle: **`>= 0.5` is muted**, below is unmuted.
    ///
    /// The threshold encoding is forced, not chosen: a `ParamSet` cell carries
    /// an `f32`, so a boolean has to ride one. Stated here because the decode
    /// lives in each node that reads the cell, and a node that picked `!= 0.0`
    /// instead would mute on a denormal.
    Mute = 21,
    /// Compressor knee width (dB, `>= 0`): the span about the threshold over
    /// which the ratio eases in. `0` is a hard knee.
    Knee = 22,
    /// Bypass toggle: **`>= 0.5` is bypassed**, below is processing. Encoded
    /// as [`Mute`](Self::Mute) is.
    Bypass = 23,
    /// Solo toggle: **`>= 0.5` is soloed**, below is not. Encoded as
    /// [`Mute`](Self::Mute) is.
    Solo = 24,
}

/// The error converting a `u16` that is not a known [`UnitParam`]
/// discriminant.
///
/// The scheme is forward-compatible: an unknown id simply does not map to a
/// param, so callers treat this as "ignore" rather than a hard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitParamOutOfRange(pub u16);

impl core::fmt::Display for UnitParamOutOfRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "unknown UnitParam id: {} (low byte {})",
            self.0,
            self.0 & 0xFF
        )
    }
}

impl std::error::Error for UnitParamOutOfRange {}

impl TryFrom<u16> for UnitParam {
    type Error = UnitParamOutOfRange;

    /// Converts the stable `u16` discriminant back to a param; `Err` for an
    /// unknown id.
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
            22 => Knee,
            23 => Bypass,
            24 => Solo,
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

/// The address of one built-in param of a node: a [`UnitParam`] kind and an
/// `index` among the node's params of that kind.
///
/// A node with one param of a kind (a filter's cutoff) is at index 0, which
/// is what [`From<UnitParam>`] gives, so a bare `UnitParam` is still an
/// address wherever one is taken as `impl Into<UnitAddr>`. A node with
/// several of a kind (a multiband compressor's threshold per band) numbers
/// them from 0.
///
/// # Packed form
///
/// [`to_u16`](Self::to_u16) packs the address into the `u16` an
/// `AudioParam<U, P>` and a persisted id carry: the **index in the high
/// byte, the [`UnitParam`] discriminant in the low byte**. Index 0 is
/// bit-identical to the plain discriminant, so a plain id decodes to index 0.
///
/// ```
/// use tutti_types::{UnitAddr, UnitParam};
///
/// let band1 = UnitAddr::new(UnitParam::Threshold, 1);
/// assert_eq!(band1.to_u16(), 0x0100 | 10);
/// assert_eq!(UnitAddr::try_from(0x010A), Ok(band1));
/// // Index 0 is the plain id.
/// assert_eq!(u16::from(UnitAddr::from(UnitParam::Threshold)), 10);
/// ```
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitAddr {
    /// The kind of param.
    pub param: UnitParam,
    /// Which of the node's params of that kind (0 for the first or only one).
    pub index: u8,
}

// A shape holds a list of these per node; they stay two bytes.
const _: () = assert!(core::mem::size_of::<UnitAddr>() == 2);

impl UnitAddr {
    /// `param` at `index`.
    #[inline]
    #[must_use]
    pub const fn new(param: UnitParam, index: u8) -> Self {
        Self { param, index }
    }

    /// `param` at index 0: what `UnitAddr::from(param)` is, as a `const`.
    #[inline]
    #[must_use]
    pub const fn first(param: UnitParam) -> Self {
        Self { param, index: 0 }
    }

    /// The packed `u16` (see the type docs): index in the high byte, the
    /// discriminant in the low.
    #[inline]
    #[must_use]
    pub const fn to_u16(self) -> u16 {
        ((self.index as u16) << 8) | self.param as u16
    }
}

impl From<UnitParam> for UnitAddr {
    #[inline]
    fn from(param: UnitParam) -> Self {
        Self::first(param)
    }
}

impl From<UnitAddr> for u16 {
    #[inline]
    fn from(a: UnitAddr) -> u16 {
        a.to_u16()
    }
}

impl TryFrom<u16> for UnitAddr {
    type Error = UnitParamOutOfRange;

    /// Unpacks a `u16`; `Err` (carrying it whole) if its low byte is not a
    /// known [`UnitParam`] discriminant. Every index is accepted: whether a
    /// node has that index is for its `ParamSet` to answer.
    fn try_from(id: u16) -> Result<Self, Self::Error> {
        let param = UnitParam::try_from(id & 0xFF).map_err(|_| UnitParamOutOfRange(id))?;
        Ok(Self::new(param, (id >> 8) as u8))
    }
}

impl PartialEq<UnitParam> for UnitAddr {
    /// An address equals a bare param when it is that param at index 0.
    fn eq(&self, other: &UnitParam) -> bool {
        self.index == 0 && self.param == *other
    }
}

impl PartialEq<UnitAddr> for UnitParam {
    fn eq(&self, other: &UnitAddr) -> bool {
        other == self
    }
}

/// The address of a scalar parameter — either one from tutti's known
/// [`UnitParam`] vocabulary, or a foreign param named by an opaque numeric id.
///
/// [`UnitParam`] is a *closed* set: the params tutti's own units define. A unit
/// with params tutti does not model — a hosted plugin, a WASM unit, a scripted
/// node — has an *open* space of its own numeric ids that no fixed enum can
/// enumerate. `ParamAddr` unifies both so one API (e.g. a modulation
/// target's `TargetKey`) addresses either: a native node answers on
/// [`Unit`](ParamAddr::Unit) and [`Indexed`](ParamAddr::Indexed) and ignores
/// [`Id`](ParamAddr::Id); a foreign unit does the reverse.
///
/// A native param is a [`UnitAddr`]: a kind and an index among the node's
/// params of that kind. **The index-0 address is always
/// [`Unit`](ParamAddr::Unit)**, and [`Indexed`](ParamAddr::Indexed) holds a
/// non-zero index (a [`NonZeroU8`]), so one address has one value and `==`,
/// `Ord` and `Hash` mean what they say. Build one from a [`UnitAddr`] with
/// [`ParamAddr::unit`] or `From`, which pick the variant; match with
/// [`as_unit`](Self::as_unit) to see a native address whole.
///
/// Deliberately not "plugin"-named — the [`Id`](ParamAddr::Id) arm is any
/// externally-numbered param, not a plugin concept.
///
/// Ordered (every `Unit` before every `Indexed` before every `Id`, each in its
/// own order) so a map can key on it: the modulation compiler's `TargetKey` is
/// a node and one.
///
/// ```
/// use tutti_types::{ParamAddr, UnitAddr, UnitParam};
///
/// let band0 = ParamAddr::unit(UnitAddr::new(UnitParam::Threshold, 0));
/// assert_eq!(band0, ParamAddr::Unit(UnitParam::Threshold));
/// let band2 = ParamAddr::unit(UnitAddr::new(UnitParam::Threshold, 2));
/// assert_eq!(band2.as_unit(), Some(UnitAddr::new(UnitParam::Threshold, 2)));
/// assert_eq!(ParamAddr::Id(7).as_unit(), None);
/// ```
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ParamAddr {
    /// A param from tutti's known vocabulary, at index 0.
    Unit(UnitParam),
    /// A param from tutti's known vocabulary at a non-zero index among the
    /// node's params of that kind. Build it with [`ParamAddr::unit`], which
    /// keeps index 0 in [`Unit`](Self::Unit).
    Indexed(UnitParam, NonZeroU8),
    /// A param addressed by an opaque numeric id, foreign to tutti's vocabulary.
    Id(u32),
}

impl ParamAddr {
    /// The native address `addr`, in its one form: [`Unit`](Self::Unit) at
    /// index 0, [`Indexed`](Self::Indexed) otherwise.
    #[must_use]
    pub const fn unit(addr: UnitAddr) -> Self {
        match NonZeroU8::new(addr.index) {
            None => Self::Unit(addr.param),
            Some(index) => Self::Indexed(addr.param, index),
        }
    }

    /// The native address this names, or `None` for a foreign
    /// [`Id`](Self::Id).
    #[must_use]
    pub const fn as_unit(self) -> Option<UnitAddr> {
        match self {
            Self::Unit(param) => Some(UnitAddr::first(param)),
            Self::Indexed(param, index) => Some(UnitAddr::new(param, index.get())),
            Self::Id(_) => None,
        }
    }
}

impl From<UnitParam> for ParamAddr {
    #[inline]
    fn from(p: UnitParam) -> Self {
        ParamAddr::Unit(p)
    }
}

impl From<UnitAddr> for ParamAddr {
    #[inline]
    fn from(a: UnitAddr) -> Self {
        ParamAddr::unit(a)
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
    addr: UnitAddr,
    _unit: core::marker::PhantomData<fn() -> U>,
}

impl<U> ParamKey<U> {
    const fn of(id: UnitParam) -> Self {
        Self {
            addr: UnitAddr::first(id),
            _unit: core::marker::PhantomData,
        }
    }

    /// The same kind of param at `index` among a node's params of that kind
    /// (a multiband compressor's threshold of band `index`): the unit is
    /// unchanged. Keys are built at index 0.
    #[inline]
    #[must_use]
    pub const fn at(self, index: u8) -> Self {
        Self {
            addr: UnitAddr::new(self.addr.param, index),
            _unit: core::marker::PhantomData,
        }
    }

    /// Returns the untyped address (kind and index), for a channel that
    /// carries it type-erased.
    #[inline]
    #[must_use]
    pub const fn addr(self) -> UnitAddr {
        self.addr
    }
}

// Written out: the derives would demand `U: Clone`/`Eq`/…, and the float units
// cannot be `Eq`. The key's identity is its address alone.
impl<U> Clone for ParamKey<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for ParamKey<U> {}
impl<U> PartialEq for ParamKey<U> {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}
impl<U> Eq for ParamKey<U> {}
impl<U> core::hash::Hash for ParamKey<U> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
    }
}
impl<U> core::fmt::Debug for ParamKey<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ParamKey<{}>({:?})",
            core::any::type_name::<U>(),
            self.addr
        )
    }
}

impl<U> From<ParamKey<U>> for UnitAddr {
    fn from(k: ParamKey<U>) -> Self {
        k.addr
    }
}

use super::units::{
    Amplitude, Cents, CompressionRatio, Db, Depth, Drive, Feedback, Hz, Mix, Q, Seconds,
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
        /// Compressor make-up gain. No engine node binds it (see
        /// [`UnitParam::Makeup`]): the compressor's make-up is `GAIN_DB`.
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
    /// Returns every param that has a typed key, with the name of its unit.
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
        assert_eq!(
            ParamKey::<Hz>::CUTOFF.addr(),
            UnitAddr::from(UnitParam::Cutoff)
        );
        assert_eq!(ParamKey::<Hz>::RATE.addr(), UnitAddr::from(UnitParam::Rate));
        assert_eq!(
            ParamKey::<Db>::THRESHOLD.addr(),
            UnitAddr::from(UnitParam::Threshold)
        );
        assert_eq!(
            ParamKey::<Db>::CEILING.addr(),
            UnitAddr::from(UnitParam::Ceiling)
        );
        assert_eq!(
            ParamKey::<Db>::GAIN_DB.addr(),
            UnitAddr::from(UnitParam::GainDb)
        );
        assert_eq!(
            ParamKey::<Db>::MAKEUP.addr(),
            UnitAddr::from(UnitParam::Makeup)
        );
        assert_eq!(
            ParamKey::<Seconds>::ATTACK.addr(),
            UnitAddr::from(UnitParam::Attack)
        );
        assert_eq!(
            ParamKey::<Seconds>::RELEASE.addr(),
            UnitAddr::from(UnitParam::Release)
        );
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
        for id in 0..=24u16 {
            let p = UnitParam::try_from(id).expect("known id");
            assert_eq!(u16::from(p), id);
        }
    }

    /// The discriminants are **stable**: they are a param's address, so
    /// renumbering one silently re-points every param set against the old
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

    /// Every variant, with the id it has always had. The id is persisted
    /// (and is `AudioParam`'s `P`), so this table is the compatibility
    /// record: a renumbered or re-ordered variant fails here by name, which
    /// `u16_round_trips` (any id maps to *some* variant) cannot say.
    const PINNED: [(UnitParam, u16); 25] = [
        (UnitParam::Cutoff, 0),
        (UnitParam::Q, 1),
        (UnitParam::GainDb, 2),
        (UnitParam::Wet, 3),
        (UnitParam::Feedback, 4),
        (UnitParam::DelayTime, 5),
        (UnitParam::Rate, 6),
        (UnitParam::Depth, 7),
        (UnitParam::RoomSize, 8),
        (UnitParam::Damping, 9),
        (UnitParam::Threshold, 10),
        (UnitParam::Ratio, 11),
        (UnitParam::Attack, 12),
        (UnitParam::Release, 13),
        (UnitParam::Ceiling, 14),
        (UnitParam::Drive, 15),
        (UnitParam::Makeup, 16),
        (UnitParam::Volume, 17),
        (UnitParam::Detune, 18),
        (UnitParam::StereoSpread, 19),
        (UnitParam::Pan, 20),
        (UnitParam::Mute, 21),
        (UnitParam::Knee, 22),
        (UnitParam::Bypass, 23),
        (UnitParam::Solo, 24),
    ];

    /// **Index 0 is bit-identical to the id before indices existed**, for
    /// every param: the packed form of `UnitAddr::from(p)` is `p`'s old
    /// `u16`, and that old `u16` decodes to `p` at index 0. This is the
    /// compatibility argument for persisted ids and `AudioParam<U, P>`.
    ///
    /// Mutation (run): pack the index into the *low* byte
    /// (`(self.param as u16) << 8 | self.index`) → index 0 of `Cutoff` is
    /// still 0 but `Q` packs to 0x0100, not 1 → fails.
    #[test]
    fn index_zero_is_the_old_id_for_every_param() {
        for (p, id) in PINNED {
            assert_eq!(u16::from(p), id, "{p:?} was renumbered");
            assert_eq!(UnitAddr::from(p).to_u16(), id);
            assert_eq!(u16::from(UnitAddr::from(p)), id);
            assert_eq!(UnitAddr::try_from(id), Ok(UnitAddr::new(p, 0)));
        }
    }

    /// The index rides the high byte, the kind the low, for every param and
    /// a spread of indices (and the extremes), and decoding inverts it.
    ///
    /// Mutation (run): decode with `(id >> 4) as u8` for the index → index 1
    /// decodes as 16 → fails.
    #[test]
    fn index_is_the_high_byte() {
        for (p, id) in PINNED {
            for index in [0u8, 1, 2, 7, 127, 255] {
                let a = UnitAddr::new(p, index);
                assert_eq!(a.to_u16(), (u16::from(index) << 8) | id, "{a:?}");
                assert_eq!(UnitAddr::try_from(a.to_u16()), Ok(a));
            }
        }
        assert_eq!(UnitAddr::new(UnitParam::Threshold, 1).to_u16(), 0x010A);
        assert_eq!(UnitAddr::new(UnitParam::Mute, 255).to_u16(), 0xFF15);
    }

    /// A packed id whose low byte is not a known kind is refused whatever its
    /// index, and a bare `UnitParam` refuses an indexed id: `UnitParam::try_from`
    /// stays the strict, index-free decode.
    ///
    /// Mutation (run): `UnitParam::try_from(id & 0xFF)` → `UnitParam::try_from(id >> 8)`
    /// in `TryFrom<u16> for UnitAddr` → `0x010A` decodes the wrong param → fails.
    #[test]
    fn unknown_kinds_and_indexed_ids_are_refused_by_the_right_decoder() {
        assert_eq!(UnitAddr::try_from(0x0119), Err(UnitParamOutOfRange(0x0119)));
        assert_eq!(UnitAddr::try_from(0x00FF), Err(UnitParamOutOfRange(0x00FF)));
        // 9_999 is 0x270F: kind 15 at index 39, which is a known kind.
        assert_eq!(
            UnitAddr::try_from(9_999),
            Ok(UnitAddr::new(UnitParam::Drive, 39))
        );
        assert_eq!(
            UnitParam::try_from(0x010A),
            Err(UnitParamOutOfRange(0x010A))
        );
    }

    /// The error names the id whole and its low byte (the kind a `u16`
    /// decode looks at).
    ///
    /// Mutation (run): drop the low byte from the `Display` text → fails.
    #[test]
    fn the_out_of_range_error_names_the_low_byte() {
        let e = UnitAddr::try_from(0x0119).unwrap_err();
        assert_eq!(e.to_string(), "unknown UnitParam id: 281 (low byte 25)");
    }

    /// An address is equal to a bare param only at index 0, in both
    /// directions.
    ///
    /// Mutation (run): drop `self.index == 0 &&` from `PartialEq<UnitParam>`
    /// → `Threshold` at index 1 equals `Threshold` → fails.
    #[test]
    fn an_address_equals_a_bare_param_only_at_index_zero() {
        let t = UnitParam::Threshold;
        assert!(UnitAddr::new(t, 0) == t);
        assert!(t == UnitAddr::new(t, 0));
        assert!(UnitAddr::new(t, 1) != t);
        assert!(t != UnitAddr::new(t, 1));
        assert!(UnitAddr::new(UnitParam::Ratio, 0) != t);
    }

    /// `ParamAddr` keeps one value per address: index 0 is `Unit`, a non-zero
    /// index `Indexed`, and `as_unit` inverts both.
    ///
    /// Mutation (run): `ParamAddr::unit` returning `Indexed` for index 0 too
    /// (use `NonZeroU8::MIN` for zero) → `Unit(Threshold) != unit(Threshold@0)` → fails.
    #[test]
    fn param_addr_has_one_form_per_address() {
        for (p, _) in PINNED {
            let zero = UnitAddr::new(p, 0);
            assert_eq!(ParamAddr::unit(zero), ParamAddr::Unit(p));
            assert_eq!(ParamAddr::from(p), ParamAddr::unit(zero));
            assert_eq!(ParamAddr::unit(zero).as_unit(), Some(zero));
            let one = UnitAddr::new(p, 1);
            let a = ParamAddr::unit(one);
            assert!(matches!(a, ParamAddr::Indexed(q, i) if q == p && i.get() == 1));
            assert_eq!(a.as_unit(), Some(one));
            assert_ne!(a, ParamAddr::Unit(p));
        }
        assert_eq!(ParamAddr::Id(3).as_unit(), None);
        // Native before foreign, and indices in order, so a map keys on it.
        assert!(
            ParamAddr::unit(UnitAddr::new(UnitParam::Mute, 9)) < ParamAddr::Id(0),
            "native before foreign"
        );
        assert!(
            ParamAddr::Unit(UnitParam::Mute) < ParamAddr::unit(UnitAddr::new(UnitParam::Cutoff, 1))
        );
    }

    /// A typed key moved to another index keeps its kind and unit, changes
    /// its address, and compares unequal to the index-0 key.
    ///
    /// Mutation (run): `at` ignoring `index` (`UnitAddr::first`) → the keys
    /// compare equal → fails.
    #[test]
    fn a_key_at_an_index_is_a_different_key() {
        use super::super::units::Db;
        let t0 = ParamKey::<Db>::THRESHOLD;
        let t2 = t0.at(2);
        assert_eq!(t2.addr().param, UnitParam::Threshold);
        assert_eq!(t2.addr(), UnitAddr::new(UnitParam::Threshold, 2));
        assert_eq!(t0.addr(), UnitAddr::from(UnitParam::Threshold));
        assert_ne!(t0, t2);
        assert_eq!(t0.at(0), t0);
        assert_eq!(UnitAddr::from(t2), t2.addr());
    }

    #[test]
    fn unknown_id_is_err() {
        assert_eq!(UnitParam::try_from(9999), Err(UnitParamOutOfRange(9999)));
    }
}
