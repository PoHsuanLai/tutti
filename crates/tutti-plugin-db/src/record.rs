//! One row in the plugin catalog, plus the catalog-identity types it carries.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The catalog record for one discovered plugin.
///
/// Produced by a probe or by a scan, and stored by a
/// [`PluginCatalog`](crate::PluginCatalog).
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
    #[must_use]
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

// Catalog-identity types, defined in `tutti-plugin-types` so the format host
// crates can name them without depending on `tutti-plugin`.
pub use tutti_plugin_types::{AuComponentType, PluginClass, PluginDescriptor};

// The classification vocabularies each format reports, and the normalized
// `PluginRole` derived from all four. Defined in `tutti-plugin-types` so the
// persisted catalog stays nameable with no format feature enabled.
pub use tutti_plugin_types::Vst2Category;

pub use tutti_plugin_types::{ClapFeature, PluginRole, Vst3PlugType, Vst3SubCategories};

/// A plugin format, as recognized from a file's extension.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    /// Returns the short format name used to build plugin IDs (`"vst3"` in
    /// `"vst3.my_reverb"`).
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
    /// The plugin file extensions (without the dot) a scan recognizes.
    pub const EXTENSIONS: &'static [&'static str] = &EXTENSION_NAMES;

    /// Returns what this plugin is (instrument, effect, …), normalized across
    /// the formats.
    pub fn role(&self) -> PluginRole {
        self.descriptor.class.role()
    }

    /// Returns whether this plugin is a note-driven sound source.
    ///
    /// Sugar over [`role`](Self::role) for the common two-way browser split.
    /// A [`Generator`](PluginRole::Generator) is deliberately **not** an
    /// instrument: it makes sound without being played.
    #[must_use]
    pub fn is_instrument(&self) -> bool {
        self.role() == PluginRole::Instrument
    }
}
