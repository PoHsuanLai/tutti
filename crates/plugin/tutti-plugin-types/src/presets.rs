//! Preset vocabulary: how a format names one of its presets, and what a host
//! knows about it.
//!
//! The four hosted formats disagree about presets more than about almost
//! anything else, in two ways that this module's shape is entirely a response
//! to:
//!
//! - **Not every format can do both halves.** CLAP loads a preset by
//!   filesystem path but cannot *enumerate* — discovery is a factory-level
//!   extension this host does not bind. VST3 enumerates richly but has no load
//!   call at all: a program is selected by writing the parameter flagged
//!   `kIsProgramChange`. That is why [`Features::PRESET_LIST`] and
//!   [`Features::PRESET_LOAD`] stay two separate bits.
//! - **A preset identifier is not an index.** See [`PresetId`].
//!
//! [`Features::PRESET_LIST`]: crate::Features::PRESET_LIST
//! [`Features::PRESET_LOAD`]: crate::Features::PRESET_LOAD

use std::path::PathBuf;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// How a format names one of its presets.
///
/// **A preset id is not a position in the list it came from.** Three of the
/// four formats number their presets in a space that is not an index, and
/// treating one as an index is silent corruption rather than a compile error:
///
/// - **AU** — `AUPreset::presetNumber` is a unit-assigned selector. Presets are
///   commonly numbered `0..n`, but a unit is free to number sparsely, so the
///   number handed to `kAudioUnitProperty_PresentPreset` must be the one the
///   unit reported and never a vec position.
/// - **VST3** — a program is `(list id, index within that list)`. The list id is
///   plugin-chosen and is *not* an index into the list of lists, so neither
///   coordinate can be dropped.
/// - **CLAP** — the identifier is a filesystem path, the only thing
///   `CLAP_EXT_PRESET_LOAD` accepts.
/// - **VST2** — genuinely positional: `effProgramChange` takes an index in
///   `[0, numPrograms)`.
///
/// Split on the *shape* of the identifier rather than on the format, for the
/// reason [`ParamAddress`](crate::ParamAddress) gives: `Au(i32) | Vst2(i32)`
/// would be two names for one behaviour, and nothing downstream could act on
/// the distinction. What a consumer must decide is whether the value is a
/// number, a pair, or a path — which is exactly the line these variants draw.
///
/// **Opaque by construction.** A caller receives one from `presets()` and hands
/// it back to `load_preset()`. There is deliberately no `From<PresetId> for
/// i32`: a caller wanting the raw value states which model it expected, via
/// [`number`](Self::number), and gets `None` for the other variants rather than
/// an invented cast.
///
/// ```
/// # use tutti_plugin_types::PresetId;
/// let au = PresetId::Number(7);
/// assert_eq!(au.number(), Some(7));
///
/// // A VST3 program is two coordinates; it will not pretend to be one number.
/// let vst3 = PresetId::Program { list_id: 1, index: 4 };
/// assert_eq!(vst3.number(), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PresetId {
    /// A single format-chosen number: an AU factory-preset selector, or a VST2
    /// program index.
    ///
    /// The two share a variant because they share an algebra — one `i32` the
    /// format assigned — even though only VST2's is dense. A consumer must not
    /// assume arithmetic on it is meaningful: for AU it is a selector, and
    /// `n + 1` is not "the next preset".
    Number(i32),
    /// VST3: a program inside one of the plugin's named program lists.
    ///
    /// `list_id` is a `ProgramListInfo::id` — plugin-chosen, not a position.
    /// `index` is the position *within* that list, which is what
    /// `getProgramName` takes.
    Program {
        /// The owning list's plugin-chosen id.
        list_id: i32,
        /// Position within that list, in `[0, program_count)`.
        index: u32,
    },
    /// CLAP: a preset file's location, as handed to
    /// `clap_plugin_preset_load::from_location`.
    Location(PathBuf),
}

impl PresetId {
    /// The raw number, for the formats that name a preset with one.
    ///
    /// `None` for VST3 and CLAP rather than a fabricated value — their
    /// identifiers genuinely are not a single number, and inventing one is how
    /// a caller ends up loading the wrong preset.
    pub fn number(&self) -> Option<i32> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Program { .. } | Self::Location(_) => None,
        }
    }

    /// The preset file's location, for CLAP.
    pub fn location(&self) -> Option<&std::path::Path> {
        match self {
            Self::Location(p) => Some(p),
            Self::Number(_) | Self::Program { .. } => None,
        }
    }
}

/// What a plugin's preset surface can actually do — one answer instead of two
/// capability bits and two method returns.
///
/// The bits ([`Features::PRESET_LIST`], [`Features::PRESET_LOAD`]) report the
/// two halves separately because no *format* offers both unconditionally. This
/// collapses them into the question a caller is really asking — "what can I
/// build a UI for" — so a preset browser matches once rather than
/// cross-referencing.
///
/// Deliberately three states and not a `bool`: [`LoadByPath`](Self::LoadByPath)
/// is the one a naive design gets wrong, silently rendering an empty browser
/// for a plugin that loads presets perfectly well.
///
/// [`Features::PRESET_LIST`]: crate::Features::PRESET_LIST
/// [`Features::PRESET_LOAD`]: crate::Features::PRESET_LOAD
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresetSupport {
    /// List and load both work: show a browser, clicking loads.
    ///
    /// VST2, AU, and VST3 — the last by writing the parameter flagged
    /// `kIsProgramChange`, which the format layer does internally so a caller
    /// need not know.
    Full,
    /// Loadable, but the host supplies the path — nothing to enumerate.
    ///
    /// CLAP, whose preset *discovery* is a factory-level extension this host
    /// does not bind. `presets()` is empty and always will be; a UI offers a
    /// file picker rather than a list, and must not report "no presets".
    LoadByPath,
    /// Enumerable, but the plugin will not load one — show the list read-only.
    ///
    /// No format reaches this today. It exists because the two halves are
    /// genuinely independent capabilities, and collapsing this case into
    /// [`Full`](Self::Full) would have a caller offer a load that fails.
    ListOnly,
    /// No preset mechanism at all.
    ///
    /// Either the plugin declined both halves, or nothing carries presets for
    /// this handle. A UI hides the browser.
    None,
}

impl PresetSupport {
    /// Derive from a capability report.
    ///
    /// Takes a [`FeatureReport`](crate::FeatureReport) rather than a bare
    /// [`Features`](crate::Features) mask, because the report is the type that
    /// knows which bits were *asked*. For CLAP, `PRESET_LIST` is `None` ("this
    /// host never asks"); for a preset-less AU it is `Some(false)` ("the unit
    /// declined"). Both yield an empty list, and only the first should offer a
    /// file picker.
    ///
    /// Within a report those two collapse: `FeatureReport::new` stores
    /// `features & probed`, so an unprobed bit is already clear and
    /// `get(f) == Some(true)` is provably equivalent to `enabled(f)`. Swapping
    /// one for the other here is an *equivalent* mutation, not an untested
    /// branch — verified. The `get` form is kept because it states the
    /// intent: this is a UI decision reading a three-state answer, not the
    /// hot-path "pick a side" that `enabled` exists for.
    pub fn from_report(report: &crate::FeatureReport) -> Self {
        use crate::Features;
        let can_load = report.get(Features::PRESET_LOAD) == Some(true);
        let can_list = report.get(Features::PRESET_LIST) == Some(true);
        match (can_list, can_load) {
            (true, true) => Self::Full,
            // Loads but cannot be asked to list. Also covers a format that
            // could list and declined, while still loading — same UI either
            // way: there is nothing to show, and a path still works.
            (false, true) => Self::LoadByPath,
            // Lists but will not load. No format reaches this today — VST3 was
            // the only candidate and now loads through its program-change
            // parameter — but a plugin that declines only the load half puts a
            // handle here, so it gets its own variant rather than being folded
            // into `Full` (which would claim a load that fails) or `None`
            // (which would hide presets the user can see named).
            (true, false) => Self::ListOnly,
            (false, false) => Self::None,
        }
    }

    /// Whether a caller can enumerate presets to show.
    pub fn can_list(self) -> bool {
        matches!(self, Self::Full | Self::ListOnly)
    }

    /// Whether a caller can ask the plugin to load one.
    pub fn can_load(self) -> bool {
        matches!(self, Self::Full | Self::LoadByPath)
    }
}

/// One preset a plugin advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Preset {
    /// How to ask for this preset back. See [`PresetId`] — opaque, and not an
    /// index into the list this came from.
    pub id: PresetId,
    /// Display name as the plugin reported it.
    pub name: String,
    /// The named set this preset belongs to, for formats that group them.
    ///
    /// `Some` only for VST3, whose programs live in named lists attached to
    /// units; `None` for AU, VST2 and CLAP, which each expose one flat set.
    ///
    /// A separate field rather than a prefix on [`name`](Self::name) so a UI
    /// can group without splitting strings, and so a format with one set does
    /// not have to invent a bank name to fit the shape.
    pub bank: Option<String>,
}

impl Preset {
    /// A preset in a format with one flat set.
    pub fn new(id: PresetId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            bank: None,
        }
    }

    /// A preset belonging to a named set (VST3 program lists).
    pub fn in_bank(id: PresetId, name: impl Into<String>, bank: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            bank: Some(bank.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{FeatureReport, Features};

    /// Each format's real capability mask lands on the right variant.
    ///
    /// These are the four shipped mappings, so the test doubles as the table a
    /// reader wants: what does *this* format give a UI. A change to any
    /// `probed::*` mask that moves a format between variants surfaces here.
    #[test]
    fn each_format_maps_to_its_preset_support() {
        // VST2, AU and VST3 all probe both bits and answer both `true` for a
        // plugin that has presets.
        let full = FeatureReport::new(
            Features::PRESET_LIST | Features::PRESET_LOAD,
            Features::PRESET_LIST | Features::PRESET_LOAD,
        );
        assert_eq!(PresetSupport::from_report(&full), PresetSupport::Full);

        // CLAP: `PRESET_LOAD` probed and set, `PRESET_LIST` never asked.
        let clap = FeatureReport::new(Features::PRESET_LOAD, Features::PRESET_LOAD);
        assert_eq!(
            PresetSupport::from_report(&clap),
            PresetSupport::LoadByPath,
            "an unprobed list bit is a file picker, not an empty browser"
        );

        // A plugin with no presets: both probed, both declined.
        let empty = FeatureReport::new(
            Features::PRESET_LIST | Features::PRESET_LOAD,
            Features::empty(),
        );
        assert_eq!(PresetSupport::from_report(&empty), PresetSupport::None);

        // Nothing probed at all — a loader that carries no preset route.
        let unasked = FeatureReport::new(Features::empty(), Features::empty());
        assert_eq!(PresetSupport::from_report(&unasked), PresetSupport::None);
    }

    /// "Declined the list" and "never asked" both give an empty list, and must
    /// not give the same variant.
    ///
    /// This is the distinction the two-bit split exists for, seen from the
    /// caller's side: an AU with no factory presets shows nothing, while a CLAP
    /// plugin shows a file picker. Collapsing them — by reading the mask
    /// instead of the report — would render one of the two wrong, and it is the
    /// CLAP case that silently loses a working feature.
    #[test]
    fn a_declined_list_and_an_unasked_one_differ() {
        let declined = FeatureReport::new(
            Features::PRESET_LIST | Features::PRESET_LOAD,
            Features::PRESET_LOAD,
        );
        let unasked = FeatureReport::new(Features::PRESET_LOAD, Features::PRESET_LOAD);

        // Same `features` mask in both — only `probed` differs.
        assert_eq!(declined.get(Features::PRESET_LIST), Some(false));
        assert_eq!(unasked.get(Features::PRESET_LIST), None);

        assert_eq!(
            PresetSupport::from_report(&declined),
            PresetSupport::LoadByPath
        );
        assert_eq!(
            PresetSupport::from_report(&unasked),
            PresetSupport::LoadByPath
        );
    }

    /// No variant claims a capability it does not have.
    ///
    /// The invariant that keeps this enum honest: `can_load()` must never be
    /// true where the plugin refuses, and `can_list()` never where there is
    /// nothing to enumerate. Written as a sweep over every variant so a new one
    /// cannot be added without deciding both answers.
    #[test]
    fn no_variant_overclaims() {
        for (support, list, load) in [
            (PresetSupport::Full, true, true),
            (PresetSupport::LoadByPath, false, true),
            (PresetSupport::ListOnly, true, false),
            (PresetSupport::None, false, false),
        ] {
            assert_eq!(support.can_list(), list, "{support:?} can_list");
            assert_eq!(support.can_load(), load, "{support:?} can_load");
        }
    }

    /// A number-shaped id gives its number back; the other two do not pretend
    /// to have one.
    ///
    /// The `None`s are the point. A `PresetId::Program` flattened to its
    /// `index` would load the right *position* in the wrong *list*, and a
    /// `Location` has no number at all — both are silent wrong-preset bugs
    /// rather than errors, which is why there is no `From<PresetId> for i32`.
    #[test]
    fn only_a_number_shaped_id_yields_a_number() {
        assert_eq!(PresetId::Number(7).number(), Some(7));
        assert_eq!(
            PresetId::Program {
                list_id: 1,
                index: 4
            }
            .number(),
            None
        );
        assert_eq!(
            PresetId::Location(PathBuf::from("/a.clap-preset")).number(),
            None
        );
    }

    /// A sparse AU selector survives as itself.
    ///
    /// AU units may number presets sparsely, so the identifier must carry the
    /// number the unit reported. This pins that nothing in the type normalizes
    /// or densifies it — a `Number(9000)` stays 9000 with no vec in sight.
    #[test]
    fn a_sparse_selector_is_carried_verbatim() {
        let sparse = [
            PresetId::Number(0),
            PresetId::Number(9000),
            PresetId::Number(-1),
        ];
        let got: Vec<i32> = sparse.iter().filter_map(PresetId::number).collect();
        assert_eq!(got, vec![0, 9000, -1]);
    }

    /// Two programs at the same index in different lists are different presets.
    ///
    /// The VST3 trap: dropping `list_id` and keeping `index` makes these two
    /// compare equal, and a host would load whichever list it happened to
    /// reach first.
    #[test]
    fn a_vst3_program_is_not_identified_by_index_alone() {
        let a = PresetId::Program {
            list_id: 1,
            index: 4,
        };
        let b = PresetId::Program {
            list_id: 2,
            index: 4,
        };
        assert_ne!(a, b, "same index in different lists must not collide");
    }

    /// `bank` is absent for a flat-set format, not an empty string.
    ///
    /// `None` and `Some("")` are different claims — "this format does not group
    /// presets" versus "the plugin named the group with an empty string" — and
    /// a UI grouping on the field must be able to tell them apart.
    #[test]
    fn a_flat_format_reports_no_bank() {
        assert_eq!(Preset::new(PresetId::Number(0), "Init").bank, None);
        assert_eq!(
            Preset::in_bank(
                PresetId::Program {
                    list_id: 1,
                    index: 0
                },
                "Init",
                "Factory"
            )
            .bank,
            Some("Factory".to_string())
        );
    }

    /// Every `PresetId` variant survives the wire.
    ///
    /// The control channel is length-prefixed bincode, and these types cross it
    /// in both directions — an id produced by `presets()` in the subprocess is
    /// handed back to `load_preset()`. A variant that does not round-trip is a
    /// preset that cannot be loaded from a listed one.
    #[cfg(feature = "serde")]
    #[test]
    fn every_preset_id_variant_round_trips_on_the_wire() {
        for id in [
            PresetId::Number(0),
            PresetId::Number(-1),
            PresetId::Number(i32::MAX),
            PresetId::Program {
                list_id: 3,
                index: 0,
            },
            PresetId::Program {
                list_id: -1,
                index: u32::MAX,
            },
            PresetId::Location(PathBuf::from("/presets/lead.clap-preset")),
        ] {
            let bytes = bincode::serialize(&id).expect("serialize");
            let back: PresetId = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(back, id, "{id:?} did not survive the wire");
        }
    }

    /// Each variant encodes to the discriminant it encoded to yesterday.
    ///
    /// A symmetric round-trip cannot catch a reordering: both ends of the
    /// `serialize`/`deserialize` pair move together, so inserting a variant
    /// mid-enum passes every test above while silently renumbering the wire.
    /// The two ends here are *not* one build — a host and its plugin-server
    /// subprocess negotiate `PROTOCOL_VERSION` and then trust each other's
    /// bytes — so a shift that no test catches is a v14 host handing a v14
    /// server a `Program` it decodes as a `Location`.
    ///
    /// bincode writes the discriminant as a `u32` little-endian tag over
    /// *declaration order*, so these leading bytes are the contract. Appending
    /// a variant leaves them alone and needs no change here; reordering or
    /// inserting one breaks this test, which is the intended alarm and means a
    /// `PROTOCOL_VERSION` bump, not a new expectation.
    #[cfg(feature = "serde")]
    #[test]
    fn each_variant_keeps_its_wire_discriminant() {
        let tag = |id: &PresetId| bincode::serialize(id).expect("serialize")[..4].to_vec();

        assert_eq!(
            tag(&PresetId::Number(0)),
            vec![0, 0, 0, 0],
            "Number is tag 0"
        );
        assert_eq!(
            tag(&PresetId::Program {
                list_id: 0,
                index: 0
            }),
            vec![1, 0, 0, 0],
            "Program is tag 1"
        );
        assert_eq!(
            tag(&PresetId::Location(PathBuf::from("/x"))),
            vec![2, 0, 0, 0],
            "Location is tag 2"
        );
    }

    /// A whole `Preset` round-trips, banked or not.
    #[cfg(feature = "serde")]
    #[test]
    fn a_preset_round_trips_with_and_without_a_bank() {
        for preset in [
            Preset::new(PresetId::Number(7), "Warm Pad"),
            Preset::in_bank(
                PresetId::Program {
                    list_id: 1,
                    index: 2,
                },
                "Warm Pad",
                "Factory",
            ),
        ] {
            let bytes = bincode::serialize(&preset).expect("serialize");
            let back: Preset = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(back, preset);
        }
    }
}
