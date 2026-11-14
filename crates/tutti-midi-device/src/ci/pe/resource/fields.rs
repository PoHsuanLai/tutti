//! The pieces the Property Data of the standard Resources are made of: ranged
//! numbers, limited text, the closed sets of their enumerated properties, the
//! manufacturer-specific properties, and the reader of an object's properties.

use std::fmt;

use tutti_ump::{Bank, U7};

use super::super::chunk::ranged;
use super::super::limits::{NotInRange, NotValid, PropertyName, text_type};
use super::super::tree::{PeJson, PeObject};

ranged! {
    /// A MIDI Channel of a Function Block: 1 to 256, counted from the first
    /// Channel of the first Group of the block and on across its Groups
    /// (M2-105 Table 10, M2-117 Table 5).
    PeChannel(u16), "u16", 1, 256, "channel of a function block"
}

ranged! {
    /// The priority of a controller: 1, the most important, to 5, hidden
    /// (M2-117 §2.1.1.1).
    Priority(u8), "u8", 1, 5, "priority"
}

ranged! {
    /// The significant bits of a 32-bit controller value: 1 to 32 (M2-117
    /// Table 5).
    SignificantBits(u8), "u8", 1, 32, "count of significant bits"
}

ranged! {
    /// The number of steps of a stepped controller (M2-117 Table 5): at least 1.
    StepCount(u32), "u32", 1, u32::MAX, "step count"
}

text_type! {
    /// A string a Resource requires: not empty (M2-103 §8.3).
    PeText, "required text",
    |t| !t.is_empty()
}

text_type! {
    /// The identifier of a Mode: 1 to 36 characters (M2-106 §2.2).
    ModeId, "mode id",
    |t| (1..=36).contains(&t.chars().count())
}

text_type! {
    /// The role of a Link: 1 to 32 characters (M2-103 Table 64).
    LinkRole, "link role",
    |t| (1..=32).contains(&t.chars().count())
}

/// A JSON Pointer that names a parameter: at most 256 bytes (M2-117 Table 5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParamPath(super::super::partial::JsonPointer);

impl ParamPath {
    /// The most bytes a path takes, 256.
    pub const MAX_LEN: usize = 256;

    /// The path `text` spells, or `None` unless it is a JSON Pointer of at most
    /// [`MAX_LEN`](Self::MAX_LEN) bytes.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > Self::MAX_LEN {
            return None;
        }
        super::super::partial::JsonPointer::new(text).map(Self)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for ParamPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Bank Select and Program Change, as the `bankPC` property carries them: the
/// three 7-bit values, 0-based (M2-105 Table 10, M2-107 Table 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeBankProgram {
    /// The Bank MSB and LSB.
    pub bank: Bank,
    /// The Program Change number.
    pub program: U7,
}

impl PeBankProgram {
    pub(super) fn to_json(self) -> PeJson {
        ints(&[self.bank.msb.get(), self.bank.lsb.get(), self.program.get()])
    }

    pub(super) fn from_json(json: &PeJson) -> Option<Self> {
        let [msb, lsb, program] = u7_array::<3>(json)?;
        Some(Self {
            bank: Bank { msb, lsb },
            program,
        })
    }
}

/// A MIDI mode: Omni on or off, Poly or Mono (M2-108 §2.2). The numbers are
/// those of the Complete MIDI 1.0 Detailed Specification's modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeMidiMode {
    /// 1: Omni On, Poly.
    OmniOnPoly,
    /// 2: Omni On, Mono.
    OmniOnMono,
    /// 3: Omni Off, Poly.
    OmniOffPoly,
    /// 4: Omni Off, Mono.
    OmniOffMono,
}

impl PeMidiMode {
    /// The number the property carries, 1 to 4.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::OmniOnPoly => 1,
            Self::OmniOnMono => 2,
            Self::OmniOffPoly => 3,
            Self::OmniOffMono => 4,
        }
    }

    /// The mode a number names, or `None` outside 1 to 4.
    #[must_use]
    pub const fn new(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::OmniOnPoly),
            2 => Some(Self::OmniOnMono),
            3 => Some(Self::OmniOffPoly),
            4 => Some(Self::OmniOffMono),
            _ => None,
        }
    }
}

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The property value.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// The variant a property value names, or `None` for other text.
            #[must_use]
            pub fn new(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

text_enum! {
    /// The kind of a Cluster of Channels (M2-105 Table 10).
    ClusterKind {
        /// `other`: any Cluster.
        Other = "other",
        /// `profile`: a Cluster of a MIDI-CI Profile, MPE's included.
        Profile = "profile",
        /// `mpe1`: an MPE zone of MPE 1.x, not the Profile.
        Mpe1 = "mpe1",
    }
}

text_enum! {
    /// The kind of a controller message (M2-117 Table 5).
    CtrlKind {
        /// `cc`: Control Change.
        ControlChange = "cc",
        /// `chPress`: Channel Pressure.
        ChannelPressure = "chPress",
        /// `pPress`: Poly Pressure.
        PolyPressure = "pPress",
        /// `nrpn`: an Assignable Controller.
        Nrpn = "nrpn",
        /// `rpn`: a Registered Controller.
        Rpn = "rpn",
        /// `pBend`: Pitch Bend.
        PitchBend = "pBend",
        /// `pnrc`: a Per-Note Registered Controller (MIDI 2.0 only).
        PerNoteRegistered = "pnrc",
        /// `pnac`: a Per-Note Assignable Controller (MIDI 2.0 only).
        PerNoteAssignable = "pnac",
        /// `pnp`: Per-Note Pitch Bend (MIDI 2.0 only).
        PerNotePitchBend = "pnp",
    }
}

text_enum! {
    /// The kind of value a controller sends or recognizes (M2-117 Table 5).
    CtrlValueKind {
        /// `absolute`: absolute values, the default.
        Absolute = "absolute",
        /// `relative`: relative values.
        Relative = "relative",
        /// `both`: absolute and relative values.
        Both = "both",
        /// `none`: no value.
        None = "none",
    }
}

text_enum! {
    /// The control that suits a controller, except `valueSelect`, which names a
    /// map and so is a [`CtrlControl`](super::CtrlControl) of its own (M2-117
    /// §2.1.1.2).
    TypeHint {
        /// `continuous`: a contiguous set of values.
        Continuous = "continuous",
        /// `momentary`: a binary switch that follows the control.
        Momentary = "momentary",
        /// `toggle`: a binary switch that alternates.
        Toggle = "toggle",
        /// `relative`: relative values.
        Relative = "relative",
    }
}

/// An array of integers as JSON.
pub(super) fn ints(values: &[u8]) -> PeJson {
    PeJson::Array(
        values
            .iter()
            .map(|&n| PeJson::Integer(i64::from(n)))
            .collect(),
    )
}

/// An array of `N` 7-bit integers.
pub(super) fn u7_array<const N: usize>(json: &PeJson) -> Option<[U7; N]> {
    let items = json.as_array()?;
    if items.len() != N {
        return None;
    }
    let mut out = [U7::of(0); N];
    for (slot, item) in out.iter_mut().zip(items) {
        let PeJson::Integer(n) = item else {
            return None;
        };
        *slot = U7::new(u8::try_from(*n).ok()?)?;
    }
    Some(out)
}

/// A property that cannot be read from Property Data, or a value a Resource
/// refuses to carry.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{place}: {fault}")]
pub struct PeDataError {
    /// The property, or the Resource when the whole Property Data is the fault.
    pub place: String,
    /// What is wrong with it.
    pub fault: PeDataFault,
}

/// What is wrong with a property of Property Data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeDataFault {
    /// The property is required and absent, or empty where the spec requires it
    /// not to be (M2-103 §8.3).
    #[error("required and missing")]
    Missing,
    /// The value is not of the type the property has.
    #[error("not {0}")]
    WrongType(&'static str),
    /// The value is of the type and outside the range or form the spec allows.
    #[error("not a valid {0}")]
    NotValid(&'static str),
    /// Two properties the spec does not allow together.
    #[error("not allowed with {0}")]
    Conflicts(&'static str),
    /// A property whose name is neither one the Resource has nor a
    /// manufacturer-specific one.
    #[error("not a property name of Property Exchange")]
    BadName,
    /// The entry of a list is not an object.
    #[error("not an object")]
    NotAnObject,
}

impl PeDataError {
    pub(super) fn new(place: impl Into<String>, fault: PeDataFault) -> Self {
        Self {
            place: place.into(),
            fault,
        }
    }
}

impl From<NotValid> for PeDataFault {
    fn from(error: NotValid) -> Self {
        Self::NotValid(error.what)
    }
}

impl<R: fmt::Display + fmt::Debug> From<NotInRange<R>> for PeDataFault {
    fn from(error: NotInRange<R>) -> Self {
        Self::NotValid(error.what)
    }
}

/// The manufacturer-specific properties of an object: names that start `x-`
/// (M2-103 §6.1.4), each with any JSON value, kept and written back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeExtensions(Vec<(PropertyName, PeJson)>);

/// A name that does not start with `x-`, so is not manufacturer-specific.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} does not start with \"x-\"")]
pub struct NotVendorName(pub String);

impl PeExtensions {
    /// Adds a property, replacing the value of one with the same name.
    ///
    /// # Errors
    ///
    /// [`NotVendorName`] unless the name starts `x-`.
    pub fn insert(&mut self, name: PropertyName, value: PeJson) -> Result<(), NotVendorName> {
        if !name.as_str().starts_with("x-") {
            return Err(NotVendorName(name.into()));
        }
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some((_, old)) => *old = value,
            None => self.0.push((name, value)),
        }
        Ok(())
    }

    /// The value of the property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PeJson> {
        self.0
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v)
    }

    /// The properties, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&PropertyName, &PeJson)> {
        self.0.iter().map(|(n, v)| (n, v))
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn write(&self, object: &mut PeObject) {
        for (name, value) in &self.0 {
            object.insert(name.as_str(), value.clone());
        }
    }
}

/// The properties of one object being read: each is taken once, and what is
/// left is the manufacturer-specific ones (or a name that is none).
pub(super) struct Fields<'a> {
    what: &'static str,
    left: Vec<(&'a str, &'a PeJson)>,
}

impl<'a> Fields<'a> {
    pub(super) fn new(json: &'a PeJson, what: &'static str) -> Result<Self, PeDataError> {
        match json {
            PeJson::Object(object) => Ok(Self {
                what,
                left: object.iter().collect(),
            }),
            _ => Err(PeDataError::new(what, PeDataFault::NotAnObject)),
        }
    }

    pub(super) fn fail(&self, name: &str, fault: PeDataFault) -> PeDataError {
        PeDataError::new(format!("{}.{name}", self.what), fault)
    }

    fn take(&mut self, name: &str) -> Option<&'a PeJson> {
        let at = self.left.iter().position(|(n, _)| *n == name)?;
        Some(self.left.remove(at).1)
    }

    /// The property `name` read by `read`, or `None` when it is absent.
    ///
    /// # Errors
    ///
    /// The property is there and `read` refuses it, as `expected`.
    pub(super) fn optional<T>(
        &mut self,
        name: &'static str,
        expected: &'static str,
        read: impl FnOnce(&'a PeJson) -> Option<T>,
    ) -> Result<Option<T>, PeDataError> {
        match self.take(name) {
            None => Ok(None),
            Some(json) => read(json)
                .map(Some)
                .ok_or_else(|| self.fail(name, PeDataFault::WrongType(expected))),
        }
    }

    /// The property `name`, which the object must have.
    pub(super) fn required<T>(
        &mut self,
        name: &'static str,
        expected: &'static str,
        read: impl FnOnce(&'a PeJson) -> Option<T>,
    ) -> Result<T, PeDataError> {
        self.optional(name, expected, read)?
            .ok_or_else(|| self.fail(name, PeDataFault::Missing))
    }

    /// The remaining properties: those that start `x-` are the manufacturer's;
    /// another that is a valid property name is a later version's and is
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`PeDataFault::BadName`] for a name that is no property name.
    pub(super) fn finish(self) -> Result<PeExtensions, PeDataError> {
        let mut extensions = PeExtensions::default();
        for (name, value) in self.left {
            let valid = PropertyName::new(name).ok_or_else(|| {
                PeDataError::new(format!("{}.{name}", self.what), PeDataFault::BadName)
            })?;
            if valid.as_str().starts_with("x-") {
                // The name starts `x-`, so the insert cannot refuse it.
                let _ = extensions.insert(valid, value.clone());
            }
        }
        Ok(extensions)
    }
}

pub(super) fn text(json: &PeJson) -> Option<&str> {
    json.as_str()
}

pub(super) fn boolean(json: &PeJson) -> Option<bool> {
    match json {
        PeJson::Bool(b) => Some(*b),
        _ => None,
    }
}

pub(super) fn integer(json: &PeJson) -> Option<i64> {
    match json {
        PeJson::Integer(n) => Some(*n),
        _ => None,
    }
}

pub(super) fn unsigned32(json: &PeJson) -> Option<u32> {
    u32::try_from(integer(json)?).ok()
}

/// A required, non-empty text property.
pub(super) fn required_text(
    fields: &mut Fields<'_>,
    name: &'static str,
) -> Result<PeText, PeDataError> {
    let text = fields.required(name, "a string", |j| text(j).map(str::to_owned))?;
    PeText::new(&text).ok_or_else(|| fields.fail(name, PeDataFault::Missing))
}

/// A property that is an array of `T` read by `read`, each element or none.
pub(super) fn list<T>(
    fields: &mut Fields<'_>,
    name: &'static str,
    read: impl Fn(&PeJson) -> Result<T, PeDataFault>,
) -> Result<Vec<T>, PeDataError> {
    let Some(items) =
        fields.optional(name, "an array", |j| j.as_array().map(<[PeJson]>::to_vec))?
    else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| read(item).map_err(|fault| fields.fail(name, fault)))
        .collect()
}

/// The unsigned 32-bit integer of a JSON value.
pub(super) fn u32_json(value: u32) -> PeJson {
    PeJson::Integer(i64::from(value))
}
