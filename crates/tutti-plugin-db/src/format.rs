//! The plugin format, and the catalog-identity types re-exported beside it.

use serde::{Deserialize, Serialize};

// Catalog-identity types, defined in `tutti-plugin-types` so the format host
// crates can name them without depending on `tutti-plugin`.
pub use tutti_plugin_types::{AuComponentType, PluginClass, PluginDescriptor};

// The classification vocabularies each format reports, and the normalized
// `PluginRole` derived from all four. Defined in `tutti-plugin-types` so the
// persisted database stays nameable with no format feature enabled.
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
    /// The plugin file extensions (without the dot) a scan recognizes.
    pub const EXTENSIONS: &'static [&'static str] = &EXTENSION_NAMES;

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
