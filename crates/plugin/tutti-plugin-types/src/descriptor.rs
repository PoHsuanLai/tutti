//! Catalog identity for a discovered plugin — the static, scan-time data the
//! plugin database persists and the DAW app reads (browser listing, dedup).
//!
//! Distinct from [`LoadedPlugin`](crate::LoadedPlugin), which carries the
//! runtime engine-wiring data (bus widths, latency) produced at *load* time and
//! never persisted. These live in the format-agnostic vocab crate so the four
//! format host crates can name them without depending on `tutti-plugin`; the
//! per-format [`PluginClass`] inner types are self-contained mirrors (not the
//! host crates' own enums), so the wire vocab never depends on the optional,
//! feature-gated FFI host crates.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Catalog identity for a discovered plugin — the static, scan-time data that
/// the plugin database persists and the DAW app reads (browser listing, dedup).
///
/// Distinct from [`LoadedPlugin`](crate::LoadedPlugin), which carries the
/// runtime engine-wiring data (bus widths, latency) produced at *load* time and
/// never persisted.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PluginDescriptor {
    /// Stable plugin id (also the database/dedup/blacklist key).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Vendor / author string (may be empty).
    pub vendor: String,
    /// Version string (may be empty).
    pub version: String,
    /// The plugin's native classification, carried verbatim from its format.
    /// The DAW app interprets this (synth vs effect, browser category, MIDI
    /// routing) — tutti does not flatten it into a common "kind".
    pub class: PluginClass,
    /// Whether the plugin reports an editor / GUI.
    ///
    /// Persisted in the catalog and read at **browse time, before any load**,
    /// so the app can show a GUI badge without instantiating the plugin. The
    /// post-load authoritative copy is `LoadedPlugin.features` /
    /// [`Features::EDITOR`](crate::Features); the loader sets both.
    ///
    /// Three-valued because the scan path cannot answer it: AU and VST3 probe
    /// without instantiating, and an editor is a property of an instance. Those
    /// paths used to persist `false`, which a badge reads as "no GUI" — for a
    /// plugin that may well have one. See [`EditorPresence`].
    ///
    /// `serde(default)` is load-bearing here, unlike on the bincode-only wire
    /// types: this record is persisted as **JSON**, and an existing catalog was
    /// written before the field existed. Without the attribute that is a parse
    /// error, and `PluginDatabase::load` quarantines the entire file — every
    /// scan result and blacklist entry discarded because one field was added.
    /// `Unknown` is the right value to default to: the old `false` it replaces
    /// was itself a guess in every record a probe wrote.
    #[cfg_attr(feature = "serde", serde(default))]
    pub editor: EditorPresence,
}

/// Whether a plugin has an editor, and whether anyone has actually looked.
///
/// [`Unknown`](Self::Unknown) is not a hedge: a probe reads the plugin's static
/// registry entry without instantiating it, and an editor is a property of an
/// instance. Reporting `false` there is a guess that gets *persisted* to the
/// catalog and then read at browse time as though it were an answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum EditorPresence {
    /// Nobody has instantiated the plugin to find out.
    ///
    /// The default, so a descriptor built by struct-update or `..Default` claims
    /// nothing rather than claiming absence.
    #[default]
    Unknown,
    /// The plugin was asked and reports no editor.
    Absent,
    /// The plugin was asked and reports an editor.
    Present,
}

impl EditorPresence {
    /// Build from a live plugin's answer. Never yields
    /// [`Unknown`](Self::Unknown) — that variant is for paths that did not ask.
    pub fn measured(has_editor: bool) -> Self {
        if has_editor {
            Self::Present
        } else {
            Self::Absent
        }
    }

    /// `true` only when the plugin was asked and said yes.
    ///
    /// The conservative read, for a caller that must produce a bool: an
    /// unexamined plugin is not claimed to have an editor. A UI that wants to
    /// distinguish "no GUI" from "not yet known" should match on the variant
    /// instead — that is why this is not a `From` impl.
    pub fn is_present(self) -> bool {
        self == Self::Present
    }
}

impl PluginDescriptor {
    /// A minimal descriptor with just id + name; everything else defaulted.
    /// Used by tests and filename-fallback probing.
    pub fn new(id: impl Into<String>, name: impl Into<String>, class: PluginClass) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            vendor: String::new(),
            version: String::new(),
            class,
            editor: EditorPresence::Unknown,
        }
    }
}

/// Each plugin format's native classification, carried verbatim across the IPC
/// wire and into the persisted catalog. Defined here because `tutti-plugin`
/// already enumerates every format; the DAW app matches on the variant once
/// (browser bucketing, MIDI-routing decisions) instead of consuming a lossy
/// shared "kind".
///
/// The inner types are self-contained mirrors (not the host crates' own enums)
/// so this wire vocab never depends on the optional, feature-gated FFI host
/// crates — the deserializing client may have different format features enabled
/// than the server that produced the value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PluginClass {
    /// Classification unavailable — the plugin couldn't be probed (e.g. a
    /// blacklisted record) or came from a filename-only fallback.
    #[default]
    Unknown,
    /// VST2 plugin category (`effFlagsIsSynth` / `getPlugCategory`).
    Vst2 { category: crate::Vst2Category },
    /// VST3 `PClassInfo2::subCategories`, e.g. `"Fx|Reverb"`, `"Instrument|Synth"`.
    Vst3 { category: String },
    /// CLAP feature tags, e.g. `["instrument", "synthesizer"]`, `["audio-effect"]`.
    Clap { features: Vec<String> },
    /// Apple AudioUnit component type (`aufx`, `aumu`, `aumf`, `aumi`, …).
    Au { component_type: AuComponentType },
}

impl PluginClass {
    /// The plugin format's short name (`"vst2"`, `"vst3"`, `"clap"`, `"au"`,
    /// or `"unknown"`). Used e.g. to fill
    /// [`EditorError::GuiNotSupported`](crate::error::EditorError::GuiNotSupported)
    /// with which format has no hostable editor.
    pub fn format_name(&self) -> &'static str {
        match self {
            PluginClass::Unknown => "unknown",
            PluginClass::Vst2 { .. } => "vst2",
            PluginClass::Vst3 { .. } => "vst3",
            PluginClass::Clap { .. } => "clap",
            PluginClass::Au { .. } => "au",
        }
    }
}

/// Mirror of the AudioUnit component type. Self-contained so the wire vocab
/// doesn't depend on `tutti-au-host`; the AU loader maps its native `AuType`
/// here. `Unknown` carries the raw four-char code for forward-compat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum AuComponentType {
    Effect,
    Instrument,
    Generator,
    MusicEffect,
    Mixer,
    Converter,
    Output,
    MidiProcessor,
    Unknown(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Not asked" and "asked, no editor" are different answers.
    ///
    /// The whole reason the type is three-valued: the AU and VST3 probe paths
    /// cannot instantiate, so a bool forced them to persist `false` — which a
    /// browse-time GUI badge reads as "no GUI".
    #[test]
    fn an_unexamined_plugin_is_not_a_plugin_without_an_editor() {
        assert_ne!(EditorPresence::Unknown, EditorPresence::Absent);
        assert!(!EditorPresence::Unknown.is_present());
        assert!(!EditorPresence::Absent.is_present());
        assert!(EditorPresence::Present.is_present());
    }

    /// `measured` never yields `Unknown` — it is only reachable by a caller that
    /// asked the plugin, so both of its outputs are real answers.
    #[test]
    fn a_measured_answer_is_never_unknown() {
        assert_eq!(EditorPresence::measured(true), EditorPresence::Present);
        assert_eq!(EditorPresence::measured(false), EditorPresence::Absent);
        for b in [true, false] {
            assert_ne!(EditorPresence::measured(b), EditorPresence::Unknown);
        }
    }

    /// A default-constructed descriptor claims nothing.
    ///
    /// `PluginDescriptor` derives `Default` and is built by struct-update in
    /// several places; if the default were `Absent`, every such site would
    /// silently assert "no editor" instead of staying silent.
    #[test]
    fn a_defaulted_descriptor_claims_nothing_about_its_editor() {
        assert_eq!(PluginDescriptor::default().editor, EditorPresence::Unknown);
        assert_eq!(
            PluginDescriptor::new("id", "name", PluginClass::Unknown).editor,
            EditorPresence::Unknown
        );
    }

    /// A catalog written before this field existed still loads.
    ///
    /// The plugin database is JSON (`discovery/database.rs`), not bincode, so it
    /// is self-describing and an old record simply lacks the key. Without
    /// `serde(default)` that is a parse error, and `load` quarantines the whole
    /// file — every scan result and blacklist entry gone because one field was
    /// added. Measured here rather than assumed.
    #[cfg(feature = "serde")]
    #[test]
    fn a_catalog_record_without_the_field_still_loads() {
        let old = r#"{"id":"au.appl.dely","name":"AUDelay","vendor":"Apple",
                      "version":"1.6.0","class":"Unknown"}"#;
        let d: PluginDescriptor = serde_json::from_str(old).expect(
            "a record written before `editor` existed must still parse — the \
             database quarantines the whole file on a parse error",
        );
        assert_eq!(d.editor, EditorPresence::Unknown);
        assert_eq!(d.name, "AUDelay");
    }

    /// The variant survives the bincode wire both IPC peers speak.
    ///
    /// It is also persisted to the plugin catalog, so a decode that collapsed
    /// `Unknown` into `Absent` would bake a scan-time guess into the database.
    #[cfg(feature = "serde")]
    #[test]
    fn the_editor_presence_survives_the_bincode_round_trip() {
        for want in [
            EditorPresence::Unknown,
            EditorPresence::Absent,
            EditorPresence::Present,
        ] {
            let mut d = PluginDescriptor::new("id", "name", PluginClass::Unknown);
            d.editor = want;
            let bytes = bincode::serialize(&d).expect("serialize");
            let back: PluginDescriptor = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(back.editor, want);
        }
    }
}
