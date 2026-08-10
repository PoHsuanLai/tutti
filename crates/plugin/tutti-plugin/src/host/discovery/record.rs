//! Pure data: one row in the plugin database, plus the catalog-identity
//! [`PluginDescriptor`] it carries.

use crate::error::BridgeError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk record for a single discovered plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    /// Where the plugin lives on disk.
    pub path: PathBuf,
    /// Which format's loader can open it.
    pub format: PluginFormat,
    /// Catalog identity — id, name, vendor, version, class, editor presence.
    pub descriptor: PluginDescriptor,
    /// Seconds since the Unix epoch of the plugin file's last modification.
    pub modification_time: u64,
    /// Blacklist state. `Ok` means the plugin is loadable.
    #[serde(default)]
    pub blacklist: Blacklist,
}

/// Blacklist state for a record.
///
/// A sum type rather than a flag beside an `Option<String>`, so a reason cannot
/// drift from the state that justifies it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum Blacklist {
    /// The plugin is loadable.
    #[default]
    Ok,
    /// The plugin brought a scan down and will not be opened.
    Blacklisted {
        /// Why, so a host can report it and offer to load the plugin anyway.
        reason: String,
    },
}

impl Blacklist {
    /// Returns whether this record is barred from loading.
    pub fn is_blacklisted(&self) -> bool {
        matches!(self, Blacklist::Blacklisted { .. })
    }

    /// Returns why the plugin was blacklisted, or `None` if it was not.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Blacklist::Ok => None,
            Blacklist::Blacklisted { reason } => Some(reason),
        }
    }
}

/// Catalog-identity types, defined in `tutti-plugin-types` so crates depending
/// only on the shared vocabulary — the four format host crates — can name them
/// without pulling in `tutti-plugin`.
pub use tutti_plugin_types::{AuComponentType, PluginClass, PluginDescriptor};

/// The VST2 plugin-category mirror, which the VST2 loader maps its native
/// category into.
///
/// Canonical in `tutti-plugin-types`, the unconditional shared dep, so this
/// persisted wire vocabulary stays nameable without the optional `vst2` feature.
pub use tutti_plugin_types::Vst2Category;

/// The classification vocabularies the other three formats report, and the
/// normalized [`PluginRole`] derived from all four.
///
/// Canonical in `tutti-plugin-types` for the same reason as [`Vst2Category`]:
/// the persisted catalog stays nameable with no format feature enabled.
pub use tutti_plugin_types::{ClapFeature, PluginRole, Vst3PlugType, Vst3SubCategories};

/// Audio plugin format.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    /// Steinberg VST3.
    Vst3,
    /// Steinberg VST2.
    Vst2,
    /// CLAP.
    Clap,
    /// Apple Audio Unit (macOS only).
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

// Compile-time projection of the extension column out of `FORMAT_BY_EXTENSION`.
// Derived rather than hand-written because the two lists silently disagreeing is
// a whole-platform outage: an extension advertised here but rejected by
// `format_from_path` makes every plugin of that format undiscoverable, with no
// error anywhere.
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
    /// the one `FORMAT_BY_EXTENSION` table that
    /// [`super::fs::format_from_path`] matches against, so the advertised list
    /// and the accepted list are the same list.
    pub const EXTENSIONS: &'static [&'static str] = &EXTENSION_NAMES;

    /// What this plugin is, normalized across the formats — the browser-facing
    /// question, without reaching through `descriptor.class` and matching each
    /// format by hand.
    pub fn role(&self) -> PluginRole {
        self.descriptor.class.role()
    }

    /// Whether this plugin is a note-driven sound source.
    ///
    /// Sugar over [`role`](Self::role) for the common two-way browser split.
    /// A [`Generator`](PluginRole::Generator) is deliberately **not** an
    /// instrument: it makes sound without being played.
    pub fn is_instrument(&self) -> bool {
        self.role() == PluginRole::Instrument
    }

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
