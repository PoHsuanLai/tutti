//! Pure data: one row in the plugin database, plus the catalog-identity
//! [`PluginDescriptor`] it carries.

use crate::error::BridgeError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk record for a single discovered plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    pub path: PathBuf,
    pub format: PluginFormat,
    pub descriptor: PluginDescriptor,
    /// Seconds since the Unix epoch of the plugin file's last modification.
    pub modification_time: u64,
    /// Blacklist state. `Ok` means the plugin is loadable.
    #[serde(default)]
    pub blacklist: Blacklist,
    /// If set, this record came from an extension's manifest
    /// `audio_plugins` field (manifest-bundled WASM audio plugin) and
    /// should be unregistered when the owning extension deactivates.
    /// `None` for standalone plugins discovered by the scanner.
    #[serde(default)]
    pub extension_id: Option<String>,
    /// Position within the owning extension's `audio_plugins` manifest
    /// list. `Some(0)` for the first bundled plugin, `Some(1)` for the
    /// second, etc. `None` for standalone scanner-discovered plugins.
    /// Used by editor extensions to address bundled DSP plugins
    /// positionally (e.g., `audio_plugin(0).set_parameter(...)`).
    #[serde(default)]
    pub manifest_index: Option<u32>,
}

/// Blacklist state for a record. Folds the old `blacklisted: bool` +
/// `blacklist_reason: Option<String>` pair so a reason can't drift from
/// the flag.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum Blacklist {
    #[default]
    Ok,
    Blacklisted {
        reason: String,
    },
}

impl Blacklist {
    pub fn is_blacklisted(&self) -> bool {
        matches!(self, Blacklist::Blacklisted { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Blacklist::Ok => None,
            Blacklist::Blacklisted { reason } => Some(reason),
        }
    }
}

/// Catalog identity for a discovered plugin — the static, scan-time data that
/// the plugin database persists and the DAW app reads (browser listing, dedup).
///
/// Distinct from `tutti_plugin_types::LoadedPlugin`, which carries the runtime
/// engine-wiring data (bus widths, latency) produced at *load* time and never
/// persisted. This type stays in `tutti-plugin` rather than the format-agnostic
/// vocab crate because it embeds [`PluginClass`], which speaks per-format
/// vocabulary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum PluginClass {
    /// Classification unavailable — the plugin couldn't be probed (e.g. a
    /// blacklisted record) or came from a filename-only fallback.
    #[default]
    Unknown,
    /// VST2 plugin category (`effFlagsIsSynth` / `getPlugCategory`).
    Vst2 { category: Vst2Category },
    /// VST3 `PClassInfo2::subCategories`, e.g. `"Fx|Reverb"`, `"Instrument|Synth"`.
    Vst3 { category: String },
    /// CLAP feature tags, e.g. `["instrument", "synthesizer"]`, `["audio-effect"]`.
    Clap { features: Vec<String> },
    /// Apple AudioUnit component type (`aufx`, `aumu`, `aumf`, `aumi`, …).
    Au { component_type: AuComponentType },
    /// WASM audio plugin (`dawai:audio-plugin`). Its WIT world exposes no
    /// category vocabulary, only whether it consumes MIDI — carried verbatim.
    Wasm { receives_midi: bool },
}

/// Mirror of the VST2 plugin category. Self-contained so the wire vocab doesn't
/// depend on `tutti-vst2-host`; the VST2 loader maps its native category here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Vst2Category {
    Unknown,
    Effect,
    Synth,
    Analysis,
    Mastering,
    Spacializer,
    RoomFx,
    SurroundFx,
    Restoration,
    OfflineProcess,
    Shell,
    Generator,
}

/// Mirror of the AudioUnit component type. Self-contained so the wire vocab
/// doesn't depend on `tutti-au-host`; the AU loader maps its native `AuType`
/// here. `Unknown` carries the raw four-char code for forward-compat.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
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

/// Audio plugin format.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    Vst3,
    Vst2,
    Clap,
    AudioUnit,
    /// WASM Component implementing the `dawai:audio-plugin@0.1.0` world.
    Wasm,
}

impl PluginFormat {
    /// Short string used to build plugin IDs (e.g., `"vst3.my_reverb"`).
    pub fn extension_id(&self) -> &'static str {
        match self {
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Vst2 => "vst2",
            PluginFormat::Clap => "clap",
            PluginFormat::AudioUnit => "au",
            PluginFormat::Wasm => "wasm",
        }
    }
}

impl PluginRecord {
    /// Plugin file extensions a scanner / asset path recognises.
    pub const EXTENSIONS: &'static [&'static str] =
        &["vst3", "vst", "dll", "so", "clap", "component", "wasm"];

    /// Probe a plugin file into a full record. Spawns a
    /// `tutti-plugin-server` subprocess to read metadata, falling back to
    /// filename-derived metadata if the server binary isn't on PATH.
    ///
    /// Yields the same row a [`crate::catalog::PluginCatalog`] would store,
    /// so the asset-loader path and the scanner path converge on one shape.
    pub fn probe(path: &Path) -> Result<Self, BridgeError> {
        let format = super::fs::format_from_path(path).ok_or_else(|| BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: crate::error::LoadStage::Opening,
            reason: "unrecognized plugin extension".to_string(),
        })?;

        let descriptor = crate::host::subprocess::probe_metadata(path)?;
        let modification_time = super::fs::file_modification_time(path).unwrap_or(0);

        Ok(PluginRecord {
            path: path.to_path_buf(),
            format,
            descriptor,
            modification_time,
            blacklist: Blacklist::Ok,
            extension_id: None,
            manifest_index: None,
        })
    }
}
