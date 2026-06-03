//! Pure data: one row in the plugin database.

use crate::error::BridgeError;
use crate::protocol::PluginInfo;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk record for a single discovered plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    pub path: PathBuf,
    pub format: PluginFormat,
    pub metadata: PluginInfo,
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

        let metadata = crate::subprocess::probe_metadata(path)?;
        let modification_time = super::fs::file_modification_time(path).unwrap_or(0);

        Ok(PluginRecord {
            path: path.to_path_buf(),
            format,
            metadata,
            modification_time,
            blacklist: Blacklist::Ok,
            extension_id: None,
            manifest_index: None,
        })
    }
}
