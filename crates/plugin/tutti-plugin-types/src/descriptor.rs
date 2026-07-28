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
    /// `true` if the plugin reports an editor / GUI.
    ///
    /// Persisted in the catalog and read at **browse time, before any load**,
    /// so the app can show a GUI badge without instantiating the plugin. The
    /// post-load authoritative copy is `LoadedPlugin.features` /
    /// [`Features::EDITOR`](crate::Features); the loader sets both.
    pub has_editor: bool,
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
            has_editor: false,
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
