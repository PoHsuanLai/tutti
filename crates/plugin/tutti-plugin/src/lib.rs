#![doc = include_str!("../README.md")]

pub mod error;
pub use error::{BridgeError, EditorError, LoadStage, PluginForkError, PluginRenderFault, Result};

mod format;
mod host;
pub(crate) mod util;

pub(crate) mod protocol;

pub use util::config::BridgeConfig;

// Enables the debug-only main-thread assertions in the editor and state paths.
pub use tutti_plugin_types::mark_main_thread;
// `PluginClient::set_automation_state` takes this, so callers must be able to
// name it without depending on tutti-plugin-types directly.
pub use tutti_plugin_types::AutomationMode;
// Likewise for `PluginHandle::set_render_mode` / `Plugin::set_render_mode`.
pub use tutti_plugin_types::RenderMode;
// `PluginHandle::loaded()` hands back a `LoadedPlugin` whose `features` field is
// this type, so a consumer that reads a capability bit — e.g. deciding whether
// to open an editor floating — must be able to name it.
pub use tutti_plugin_types::Features;
// `PluginHandle::presets` hands back these, and `load_preset` takes one, so a
// caller must be able to name them without depending on tutti-plugin-types.
pub use tutti_plugin_types::{FeatureReport, Preset, PresetId, PresetSupport};

/// Traits for plugging an out-of-crate loader into [`PluginHandle`](handles::PluginHandle).
///
/// **Not part of the general API.** These let an out-of-crate loader
/// implement the granular host-side capability traits
/// ([`HostParams`](backend::HostParams), [`HostState`](backend::HostState), and
/// optionally [`HostEditor`](backend::HostEditor)) over its own
/// plugin and hand the result to
/// [`PluginHandle::from_backend`](handles::PluginHandle::from_backend),
/// reusing this crate's main-thread control surface and audio-node wiring
/// without re-implementing them. End users loading plugins should stick to
/// [`catalog`] and [`handles`].
pub mod backend {
    pub use crate::host::handles::capabilities::{
        HostAutomationState, HostEditor, HostParams, HostPresets, HostState,
    };
    pub use crate::host::node::ParameterChangeSink;
}

// Loads a VST2 plugin in the host process; most callers use `Plugin::open`,
// which dispatches here for `.vst` files.
#[cfg(feature = "vst2")]
pub use format::vst2_in_process::{load_client as in_process_vst2_client, InProcessVst2Client};

/// Discovering, persisting, and loading plugins.
///
/// **Bring your own store.** [`discover`](catalog::discover) walks directories
/// and returns paths; [`PluginRecord::probe`](catalog::PluginRecord::probe)
/// turns one path into one record. Two pure functions, no state, no feature
/// flags — put the results wherever you like:
///
/// ```no_run
/// use tutti_plugin::catalog::{discover, PluginRecord};
/// # let dirs = vec![std::path::PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
/// let records: Vec<PluginRecord> = discover(&dirs)
///     .iter()
///     .filter_map(|(path, _format)| PluginRecord::probe(path).ok())
///     .collect();
/// ```
///
/// **Or let the library manage it.** [`Plugins`](catalog::Plugins) wraps a
/// [`PluginCatalog`](catalog::PluginCatalog) — a pluggable record store — and
/// adds the two things the pure path cannot give you: incremental rescan
/// (probe only what changed, via stored mtimes — the difference between a
/// multi-minute startup and an instant one) and crash recovery (a plugin that
/// hard-crashes the scanner is auto-blacklisted on the next run).
///
/// `JsonCatalog` is one such store, behind the opt-in
/// `json` feature. Implement [`PluginCatalog`](catalog::PluginCatalog) yourself
/// for SQLite, a CRDT, or an in-memory map.
pub mod catalog {
    #[cfg(feature = "json")]
    pub use crate::host::discovery::JsonCatalog;
    pub use crate::host::discovery::{
        discover, AuComponentType, Blacklist, CatalogExt, ClapFeature, PluginCatalog, PluginClass,
        PluginDescriptor, PluginFormat, PluginRecord, PluginRole, PluginScanner, ScanHandle,
        ScanPhase, ScanProgress, ScanResult, Vst2Category, Vst3PlugType, Vst3SubCategories,
    };
    pub use crate::host::plugin::Plugin;
    pub use crate::host::plugins::{PluginId, Plugins, ScanTicket};
    pub use crate::util::config::{AudioConfig, CatalogConfig, NO_SCAN_DIRS};
}

// Per-plugin handles: the graph node and the main-thread control surface.
pub use host::handles;

// Wire-contract types shared with `tutti-plugin-server`.
pub mod server;
