//! The controller Resources (M2-117): AllCtrlList, ChCtrlList and CtrlMapList.

use tutti_ump::U7;

use super::super::limits::ResourceId;
use super::super::tree::{PeJson, PeObject};
use super::channels::{channel, list_type};
use super::fields::{
    CtrlKind, CtrlValueKind, Fields, ParamPath, PeChannel, PeDataError, PeDataFault, PeExtensions,
    PeText, Priority, SignificantBits, StepCount, TypeHint, boolean, integer, ints, required_text,
    text, u32_json, unsigned32,
};

/// Which controller message an entry describes: the kind and the index it needs
/// (M2-117 Table 5, `ctrlType` and `ctrlIndex`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeController {
    /// `cc`: Control Change, one index, or the MSB and LSB of a 14-bit one.
    ControlChange(CcIndex),
    /// `chPress`: Channel Pressure.
    ChannelPressure,
    /// `pPress`: Poly Pressure.
    PolyPressure,
    /// `rpn`: a Registered Controller, by Bank (MSB) and Index (LSB).
    Rpn {
        /// The Bank.
        bank: U7,
        /// The Index.
        index: U7,
    },
    /// `nrpn`: an Assignable Controller, by Bank (MSB) and Index (LSB).
    Nrpn {
        /// The Bank.
        bank: U7,
        /// The Index.
        index: U7,
    },
    /// `pBend`: Pitch Bend.
    PitchBend,
    /// `pnrc`: a Per-Note Registered Controller, by index.
    PerNoteRegistered(u8),
    /// `pnac`: a Per-Note Assignable Controller, by index.
    PerNoteAssignable(u8),
    /// `pnp`: Per-Note Pitch Bend.
    PerNotePitchBend,
}

/// The controller number of a Control Change entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcIndex {
    /// A 7-bit controller.
    Single(U7),
    /// A 14-bit controller: the MSB and LSB controller numbers.
    Pair {
        /// The MSB controller number.
        msb: U7,
        /// The LSB controller number.
        lsb: U7,
    },
}

impl PeController {
    /// The `ctrlType` of the message.
    #[must_use]
    pub const fn kind(self) -> CtrlKind {
        match self {
            Self::ControlChange(_) => CtrlKind::ControlChange,
            Self::ChannelPressure => CtrlKind::ChannelPressure,
            Self::PolyPressure => CtrlKind::PolyPressure,
            Self::Rpn { .. } => CtrlKind::Rpn,
            Self::Nrpn { .. } => CtrlKind::Nrpn,
            Self::PitchBend => CtrlKind::PitchBend,
            Self::PerNoteRegistered(_) => CtrlKind::PerNoteRegistered,
            Self::PerNoteAssignable(_) => CtrlKind::PerNoteAssignable,
            Self::PerNotePitchBend => CtrlKind::PerNotePitchBend,
        }
    }

    /// The `ctrlIndex`, absent for the kinds that need none.
    fn index_json(self) -> Option<PeJson> {
        match self {
            Self::ControlChange(CcIndex::Single(n)) => Some(ints(&[n.get()])),
            Self::ControlChange(CcIndex::Pair { msb, lsb }) => Some(ints(&[msb.get(), lsb.get()])),
            Self::Rpn { bank, index } | Self::Nrpn { bank, index } => {
                Some(ints(&[bank.get(), index.get()]))
            }
            Self::PerNoteRegistered(n) | Self::PerNoteAssignable(n) => Some(ints(&[n])),
            Self::ChannelPressure
            | Self::PolyPressure
            | Self::PitchBend
            | Self::PerNotePitchBend => None,
        }
    }

    /// The controller of `kind` the `ctrlIndex` names, or `None` when the index
    /// does not fit the kind.
    fn from_parts(kind: CtrlKind, index: Option<&[i64]>) -> Option<Self> {
        let byte = |n: i64| u8::try_from(n).ok();
        let seven = |n: i64| byte(n).and_then(U7::new);
        Some(match (kind, index) {
            (CtrlKind::ChannelPressure, _) => Self::ChannelPressure,
            (CtrlKind::PolyPressure, _) => Self::PolyPressure,
            (CtrlKind::PitchBend, _) => Self::PitchBend,
            (CtrlKind::PerNotePitchBend, _) => Self::PerNotePitchBend,
            (CtrlKind::ControlChange, Some(&[n])) => {
                Self::ControlChange(CcIndex::Single(seven(n)?))
            }
            (CtrlKind::ControlChange, Some(&[msb, lsb])) => Self::ControlChange(CcIndex::Pair {
                msb: seven(msb)?,
                lsb: seven(lsb)?,
            }),
            (CtrlKind::Rpn, Some(&[bank, index])) => Self::Rpn {
                bank: seven(bank)?,
                index: seven(index)?,
            },
            (CtrlKind::Nrpn, Some(&[bank, index])) => Self::Nrpn {
                bank: seven(bank)?,
                index: seven(index)?,
            },
            (CtrlKind::PerNoteRegistered, Some(&[n])) => Self::PerNoteRegistered(byte(n)?),
            (CtrlKind::PerNoteAssignable, Some(&[n])) => Self::PerNoteAssignable(byte(n)?),
            _ => return None,
        })
    }
}

/// How a controller is meant to be shown and driven (`typeHint`, `ctrlMapId`;
/// M2-117 §2.1.1.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CtrlControl {
    /// Any control; the hint and the value map are each optional.
    Free {
        /// The `typeHint`, other than `valueSelect`.
        hint: Option<TypeHint>,
        /// The `ctrlMapId`: the `resId` of the CtrlMapList that names its values.
        map: Option<ResourceId>,
    },
    /// `valueSelect`: the control sends only the values of a CtrlMapList, whose
    /// `resId` is the `ctrlMapId` the entry shall have.
    ValueSelect {
        /// The `ctrlMapId`.
        map: ResourceId,
    },
}

/// What a controller recognizes: every value, a number of steps, or a range of
/// values. `stepCount` and `minMax` shall not both be present (M2-117 §2.1.1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtrlRange {
    /// The whole 32-bit range.
    Full,
    /// `stepCount`: a number of discrete steps.
    Steps(StepCount),
    /// `minMax`: the values from `min` to `max`, unsigned 32-bit.
    MinMax {
        /// The lowest value recognized.
        min: u32,
        /// The highest value recognized.
        max: u32,
    },
}

/// A controller message of a device, as AllCtrlList and ChCtrlList describe it
/// (M2-117 Tables 5 and 11).
#[derive(Clone, Debug, PartialEq)]
pub struct PeCtrl {
    /// The name of the message.
    pub title: PeText,
    /// What it does, in CommonMark.
    pub description: Option<String>,
    /// The controller message.
    pub controller: PeController,
    /// How prominent to make it.
    pub priority: Option<Priority>,
    /// The value when reset, unsigned 32-bit.
    pub default: Option<u32>,
    /// The values it sends.
    pub transmit: CtrlValueKind,
    /// The values it recognizes.
    pub recognize: CtrlValueKind,
    /// The significant bits of its 32-bit values, when not all 32.
    pub significant_bits: Option<SignificantBits>,
    /// Where it sits in a tree of parameters.
    pub param_path: Option<ParamPath>,
    /// The control it suits.
    pub control: CtrlControl,
    /// The values it recognizes.
    pub range: CtrlRange,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeCtrl {
    /// A controller with only the properties the spec requires: absolute values
    /// both ways, any control, the whole range.
    #[must_use]
    pub fn new(title: PeText, controller: PeController) -> Self {
        Self {
            title,
            description: None,
            controller,
            priority: None,
            default: None,
            transmit: CtrlValueKind::Absolute,
            recognize: CtrlValueKind::Absolute,
            significant_bits: None,
            param_path: None,
            control: CtrlControl::Free {
                hint: None,
                map: None,
            },
            range: CtrlRange::Full,
            extensions: PeExtensions::default(),
        }
    }

    fn write(&self, o: &mut PeObject) {
        o.insert("title", self.title.as_str().into());
        if let Some(d) = &self.description {
            o.insert("description", d.as_str().into());
        }
        o.insert("ctrlType", self.controller.kind().as_str().into());
        if let Some(index) = self.controller.index_json() {
            o.insert("ctrlIndex", index);
        }
        if let Some(p) = self.priority {
            o.insert("priority", i64::from(p.get()).into());
        }
        if let Some(d) = self.default {
            o.insert("default", u32_json(d));
        }
        if self.transmit != CtrlValueKind::Absolute {
            o.insert("transmit", self.transmit.as_str().into());
        }
        if self.recognize != CtrlValueKind::Absolute {
            o.insert("recognize", self.recognize.as_str().into());
        }
        if let Some(bits) = self.significant_bits {
            o.insert("numSigBits", i64::from(bits.get()).into());
        }
        if let Some(path) = &self.param_path {
            o.insert("paramPath", path.as_str().into());
        }
        match &self.control {
            CtrlControl::Free { hint, map } => {
                if let Some(hint) = hint {
                    o.insert("typeHint", hint.as_str().into());
                }
                if let Some(map) = map {
                    o.insert("ctrlMapId", map.as_str().into());
                }
            }
            CtrlControl::ValueSelect { map } => {
                o.insert("typeHint", "valueSelect".into());
                o.insert("ctrlMapId", map.as_str().into());
            }
        }
        match self.range {
            CtrlRange::Full => {}
            CtrlRange::Steps(n) => o.insert("stepCount", u32_json(n.get())),
            CtrlRange::MinMax { min, max } => {
                o.insert("minMax", PeJson::Array(vec![u32_json(min), u32_json(max)]));
            }
        }
        self.extensions.write(o);
    }

    fn read(f: &mut Fields<'_>) -> Result<Self, PeDataError> {
        let title = required_text(f, "title")?;
        let description = f.optional("description", "a string", |j| text(j).map(str::to_owned))?;
        let kind_text = f.required("ctrlType", "a string", text)?.to_owned();
        let kind = CtrlKind::new(&kind_text)
            .ok_or_else(|| f.fail("ctrlType", PeDataFault::NotValid("controller type")))?;
        let index = f.optional("ctrlIndex", "an array of integers", |j| {
            j.as_array()?
                .iter()
                .map(integer)
                .collect::<Option<Vec<i64>>>()
        })?;
        let controller = PeController::from_parts(kind, index.as_deref())
            .ok_or_else(|| f.fail("ctrlIndex", PeDataFault::NotValid("controller index")))?;
        let priority = f
            .optional("priority", "an integer", integer)?
            .map(|n| {
                u8::try_from(n)
                    .ok()
                    .and_then(Priority::new)
                    .ok_or_else(|| f.fail("priority", PeDataFault::NotValid("priority")))
            })
            .transpose()?;
        let default = f.optional("default", "an unsigned 32-bit integer", unsigned32)?;
        let transmit = value_kind(f, "transmit")?;
        let recognize = value_kind(f, "recognize")?;
        let significant_bits = f
            .optional("numSigBits", "an integer", integer)?
            .map(|n| {
                u8::try_from(n)
                    .ok()
                    .and_then(SignificantBits::new)
                    .ok_or_else(|| {
                        f.fail(
                            "numSigBits",
                            PeDataFault::NotValid("count of significant bits"),
                        )
                    })
            })
            .transpose()?;
        let param_path = f
            .optional("paramPath", "a string", text)?
            .map(|t| {
                ParamPath::new(t)
                    .ok_or_else(|| f.fail("paramPath", PeDataFault::NotValid("parameter path")))
            })
            .transpose()?;
        let hint_text = f.optional("typeHint", "a string", |j| text(j).map(str::to_owned))?;
        let map = f
            .optional("ctrlMapId", "a string", text)?
            .map(|t| {
                ResourceId::new(t)
                    .ok_or_else(|| f.fail("ctrlMapId", PeDataFault::NotValid("resource id")))
            })
            .transpose()?;
        let control = match hint_text.as_deref() {
            Some("valueSelect") => CtrlControl::ValueSelect {
                map: map.ok_or_else(|| f.fail("ctrlMapId", PeDataFault::Missing))?,
            },
            Some(other) => CtrlControl::Free {
                hint: Some(
                    TypeHint::new(other)
                        .ok_or_else(|| f.fail("typeHint", PeDataFault::NotValid("type hint")))?,
                ),
                map,
            },
            None => CtrlControl::Free { hint: None, map },
        };
        let steps = f
            .optional("stepCount", "an integer", unsigned32)?
            .map(|n| {
                StepCount::new(n)
                    .ok_or_else(|| f.fail("stepCount", PeDataFault::NotValid("step count")))
            })
            .transpose()?;
        let min_max = f.optional(
            "minMax",
            "an array of 2 unsigned 32-bit integers",
            |j| match j.as_array()? {
                [lo, hi] => Some((unsigned32(lo)?, unsigned32(hi)?)),
                _ => None,
            },
        )?;
        let range = match (steps, min_max) {
            (None, None) => CtrlRange::Full,
            (Some(n), None) => CtrlRange::Steps(n),
            (None, Some((min, max))) if min <= max => CtrlRange::MinMax { min, max },
            (None, Some(_)) => return Err(f.fail("minMax", PeDataFault::NotValid("range"))),
            (Some(_), Some(_)) => return Err(f.fail("minMax", PeDataFault::Conflicts("stepCount"))),
        };
        Ok(Self {
            title,
            description,
            controller,
            priority,
            default,
            transmit,
            recognize,
            significant_bits,
            param_path,
            control,
            range,
            extensions: PeExtensions::default(),
        })
    }
}

fn value_kind(f: &mut Fields<'_>, name: &'static str) -> Result<CtrlValueKind, PeDataError> {
    Ok(f.optional(name, "a string", text)?
        .map(|t| {
            CtrlValueKind::new(t).ok_or_else(|| f.fail(name, PeDataFault::NotValid("value kind")))
        })
        .transpose()?
        .unwrap_or(CtrlValueKind::Absolute))
}

/// An entry of AllCtrlList: a controller of one Channel (M2-117 Table 5).
#[derive(Clone, Debug, PartialEq)]
pub struct PeAllCtrlEntry {
    /// The Channel of the Function Block it is on.
    pub channel: PeChannel,
    /// The controller.
    pub ctrl: PeCtrl,
}

/// An entry of ChCtrlList: a controller of the Channel the list was asked for
/// (M2-117 Table 11).
#[derive(Clone, Debug, PartialEq)]
pub struct PeChCtrlEntry {
    ctrl: PeCtrl,
    default_cc_map: bool,
}

/// An entry that claims the Default Control Change Mapping Profile for a message
/// that is no Control Change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("defaultCCMap is for a Control Change, not for {0:?}")]
pub struct DefaultMapNotCc(pub CtrlKind);

impl PeChCtrlEntry {
    /// An entry with `defaultCCMap` false.
    #[must_use]
    pub fn new(ctrl: PeCtrl) -> Self {
        Self {
            ctrl,
            default_cc_map: false,
        }
    }

    /// An entry whose Control Change follows the Default Control Change Mapping
    /// Profile, Level 1 (`defaultCCMap`).
    ///
    /// # Errors
    ///
    /// [`DefaultMapNotCc`] when the controller is no Control Change.
    pub fn with_default_cc_map(ctrl: PeCtrl) -> Result<Self, DefaultMapNotCc> {
        match ctrl.controller {
            PeController::ControlChange(_) => Ok(Self {
                ctrl,
                default_cc_map: true,
            }),
            other => Err(DefaultMapNotCc(other.kind())),
        }
    }

    /// The controller.
    #[must_use]
    pub fn ctrl(&self) -> &PeCtrl {
        &self.ctrl
    }

    /// Whether the Control Change follows the Default Control Change Mapping
    /// Profile.
    #[must_use]
    pub fn default_cc_map(&self) -> bool {
        self.default_cc_map
    }
}

/// A controller that is listed twice where the spec allows once: a
/// `ctrlType` and `ctrlIndex` (and Channel) is listed again only to describe
/// what the device sends where the first describes what it recognizes
/// (M2-117 §2.1.3, §3.1.3).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is listed twice, and not once for transmit and once for recognize")]
pub struct ListedTwice(pub String);

/// Whether `a` and `b` are the one controller seen as sent and as recognized.
fn complementary(a: &PeCtrl, b: &PeCtrl) -> bool {
    (a.transmit == CtrlValueKind::None && b.recognize == CtrlValueKind::None)
        || (b.transmit == CtrlValueKind::None && a.recognize == CtrlValueKind::None)
}

fn check_unique<'a, K: PartialEq + std::fmt::Debug>(
    items: impl Iterator<Item = (K, &'a PeCtrl)>,
) -> Result<(), ListedTwice> {
    let seen: Vec<(K, &PeCtrl)> = items.collect();
    for (at, (key, ctrl)) in seen.iter().enumerate() {
        for (other_key, other) in &seen[..at] {
            if key == other_key && !complementary(ctrl, other) {
                return Err(ListedTwice(format!("{key:?}")));
            }
        }
    }
    Ok(())
}

list_type! {
    /// The Property Data of AllCtrlList (M2-117 §2).
    PeAllCtrlList of PeAllCtrlEntry
}

impl PeAllCtrlList {
    /// The list of `entries`.
    ///
    /// # Errors
    ///
    /// [`ListedTwice`] for a controller listed twice, other than once for what it
    /// sends and once for what it recognizes.
    pub fn checked(entries: Vec<PeAllCtrlEntry>) -> Result<Self, ListedTwice> {
        check_unique(
            entries
                .iter()
                .map(|e| ((e.ctrl.controller, e.channel), &e.ctrl)),
        )?;
        Ok(Self(entries))
    }

    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(
            self.0
                .iter()
                .map(|e| {
                    let mut o = PeObject::new();
                    o.insert("channel", i64::from(e.channel.get()).into());
                    e.ctrl.write(&mut o);
                    PeJson::Object(o)
                })
                .collect(),
        )
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        let entries = items
            .iter()
            .map(|json| {
                let mut f = Fields::new(json, "AllCtrlList entry")?;
                let ch = channel(&mut f, "channel")?
                    .ok_or_else(|| f.fail("channel", PeDataFault::Missing))?;
                let mut ctrl = PeCtrl::read(&mut f)?;
                ctrl.extensions = f.finish()?;
                Ok(PeAllCtrlEntry { channel: ch, ctrl })
            })
            .collect::<Result<Vec<_>, PeDataError>>()?;
        Self::checked(entries)
            .map_err(|e| PeDataError::new("AllCtrlList", PeDataFault::NotValid(listed_twice(&e))))
    }
}

fn listed_twice(_: &ListedTwice) -> &'static str {
    "list: a controller listed once, or once for transmit and once for recognize"
}

list_type! {
    /// The Property Data of ChCtrlList (M2-117 §3).
    PeChCtrlList of PeChCtrlEntry
}

impl PeChCtrlList {
    /// The list of `entries`.
    ///
    /// # Errors
    ///
    /// [`ListedTwice`] as for [`PeAllCtrlList::checked`].
    pub fn checked(entries: Vec<PeChCtrlEntry>) -> Result<Self, ListedTwice> {
        check_unique(entries.iter().map(|e| (e.ctrl.controller, &e.ctrl)))?;
        Ok(Self(entries))
    }

    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(
            self.0
                .iter()
                .map(|e| {
                    let mut o = PeObject::new();
                    e.ctrl.write(&mut o);
                    if e.default_cc_map {
                        o.insert("defaultCCMap", true.into());
                    }
                    PeJson::Object(o)
                })
                .collect(),
        )
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        let entries = items
            .iter()
            .map(|json| {
                let mut f = Fields::new(json, "ChCtrlList entry")?;
                let mut ctrl = PeCtrl::read(&mut f)?;
                let map = f
                    .optional("defaultCCMap", "a boolean", boolean)?
                    .unwrap_or(false);
                ctrl.extensions = f.finish()?;
                if map {
                    PeChCtrlEntry::with_default_cc_map(ctrl).map_err(|_| {
                        PeDataError::new(
                            "ChCtrlList entry.defaultCCMap",
                            PeDataFault::NotValid("Control Change flag"),
                        )
                    })
                } else {
                    Ok(PeChCtrlEntry::new(ctrl))
                }
            })
            .collect::<Result<Vec<_>, PeDataError>>()?;
        Self::checked(entries)
            .map_err(|e| PeDataError::new("ChCtrlList", PeDataFault::NotValid(listed_twice(&e))))
    }
}

/// A named value of a controller (M2-117 Table 19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeCtrlMapEntry {
    /// The unsigned 32-bit value.
    pub value: u32,
    /// The name of the value.
    pub title: PeText,
}

list_type! {
    /// The Property Data of CtrlMapList: the named values of one controller
    /// (M2-117 §4).
    PeCtrlMapList of PeCtrlMapEntry
}

impl PeCtrlMapList {
    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(
            self.0
                .iter()
                .map(|e| {
                    let mut o = PeObject::new();
                    o.insert("value", u32_json(e.value));
                    o.insert("title", e.title.as_str().into());
                    PeJson::Object(o)
                })
                .collect(),
        )
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        items
            .iter()
            .map(|json| {
                let mut f = Fields::new(json, "CtrlMapList entry")?;
                let value = f.required("value", "an unsigned 32-bit integer", unsigned32)?;
                let title = required_text(&mut f, "title")?;
                f.finish()?;
                Ok(PeCtrlMapEntry { value, title })
            })
            .collect::<Result<_, PeDataError>>()
            .map(Self)
    }
}
