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

/// Catalog-identity types, now homed in `tutti-plugin-types` so crates that
/// depend only on the shared vocab (the four format host crates) can name them
/// without pulling in `tutti-plugin`. Re-exported here so every existing
/// `crate::host::discovery::record::{PluginDescriptor, PluginClass,
/// AuComponentType}` path still resolves.
pub use tutti_plugin_types::{AuComponentType, PluginClass, PluginDescriptor};

/// The VST2 plugin-category mirror. Canonical definition lives in
/// `tutti-plugin-types` (the unconditional shared dep), so this persisted
/// wire vocab stays nameable without the optional `vst2` feature; the VST2
/// loader maps its native category into it.
pub use tutti_plugin_types::Vst2Category;

/// Audio plugin format.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    Vst3,
    Vst2,
    Clap,
    AudioUnit,
}

impl PluginFormat {
    /// Short string used to build plugin IDs (e.g., `"vst3.my_reverb"`).
    pub fn extension_id(&self) -> &'static str {
        match self {
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Vst2 => "vst2",
            PluginFormat::Clap => "clap",
            PluginFormat::AudioUnit => "au",
        }
    }
}

/// Compile-time projection of the extension column out of
/// [`super::fs::FORMAT_BY_EXTENSION`]. Keeping this derived (rather than a
/// second hand-written list) is the fix for the two lists disagreeing: this
/// const used to advertise `dll`/`so` while `format_from_path` rejected both,
/// so no VST2 plugin was discoverable on Windows or Linux.
const fn extension_names<const N: usize>() -> [&'static str; N] {
    let table = super::fs::FORMAT_BY_EXTENSION;
    assert!(
        table.len() == N,
        "extension_names::<N> must match FORMAT_BY_EXTENSION.len()"
    );
    let mut out = [""; N];
    let mut i = 0;
    while i < N {
        out[i] = table[i].0;
        i += 1;
    }
    out
}

const EXTENSION_NAMES: [&str; 6] = extension_names::<6>();

impl PluginRecord {
    /// Plugin file extensions a scanner / asset path recognises. Derived from
    /// the one [`super::fs::FORMAT_BY_EXTENSION`] table that
    /// [`super::fs::format_from_path`] matches against, so the advertised list
    /// and the accepted list are the same list.
    pub const EXTENSIONS: &'static [&'static str] = &EXTENSION_NAMES;

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
        })
    }
}
