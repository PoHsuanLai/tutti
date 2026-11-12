#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

/// The errors a plugin host returns that `tutti-plugin-types` does not hold:
/// the fork's render fault, beside the format-agnostic ones every host shares.
pub mod error {
    pub use tutti_plugin_ipc::PluginRenderFault;
    pub use tutti_plugin_types::{Delivered, EditorError, LoadStage, PluginError, StateError};
}
pub use error::{EditorError, LoadStage, PluginRenderFault};

mod format;
mod host;

pub use tutti_plugin_ipc::BridgeConfig;

// The main-thread token the editor, state and polling entries take; a host
// claims it once, on its UI thread, with `MainThread::claim`.
pub use tutti_plugin_types::{MainThread, NotMainThread};
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
// `AnyThread::parameter_descriptors` lists `ParamId`s, and
// `set_parameter_value` takes a `Normalized`.
pub use tutti_plugin_types::{Normalized, ParamId};

/// Discovering, persisting, and loading plugins.
///
/// Discovery itself lives in `tutti-plugin-db`; this module supplies what needs
/// the plugin server and loads what was found.
///
/// **Bring your own store.** [`discover`](tutti_plugin_db::discover) walks
/// directories and returns paths; [`probe_record`](catalog::probe_record)
/// turns one path into the [`Delta`](tutti_plugin_db::Delta) that records it.
/// Two functions and a plain table, no feature flags:
///
/// ```no_run
/// use tutti_plugin::catalog::probe_record;
/// use tutti_plugin_db::{PluginDb, discover};
/// # let dirs = vec![std::path::PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
/// let mut db = PluginDb::new();
/// for (path, _format) in discover(&dirs) {
///     if let Ok(delta) = probe_record(path) {
///         db.apply(delta);
///     }
/// }
/// ```
///
/// **Or let the library manage it.** [`Plugins`](catalog::Plugins) wraps a
/// [`PluginDb`](tutti_plugin_db::PluginDb) and adds the two things the pure path
/// cannot give you: incremental rescan (probe only what changed, via stored
/// mtimes, on a worker thread while the catalog stays readable: the difference
/// between a multi-minute startup and an instant one) and crash recovery (a
/// plugin that hard-crashes the scanner is auto-blacklisted on the next run).
///
/// With the opt-in `json` feature, `Plugins::load` and `Plugins::save` keep the
/// table in a versioned JSON file. Without it, build the `PluginDb` from your
/// own store and pass it to `Plugins::with_db`.
pub mod catalog {
    pub use crate::host::db_probe::{
        DEFAULT_PROBE_TIMEOUT, probe_failure, probe_record, probe_record_with,
    };
    pub use tutti_plugin_types::{Helper, Locator};
    // Named in `Plugin::role` and `Plugins::iter`.
    pub use crate::host::plugin::{BoundPlugin, Plugin};
    pub use crate::host::plugin_ref::{PluginRef, PluginState};
    pub use crate::host::plugins::Plugins;
    pub use tutti_plugin_ipc::{AudioConfig, CatalogConfig, NO_SCAN_DIRS};
    pub use tutti_plugin_types::{PluginDescriptor, PluginRole};
}

// The graph node of a served plugin and its two states. The control handle,
// the node's controls and the host policy over a handle are `tutti-plugin-host`'s.
pub use host::handles;
