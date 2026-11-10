#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// The errors a plugin host returns: the process edge's own ([`BridgeError`]
/// and the fork's) and the format-agnostic ones every host shares.
pub mod error {
    pub use tutti_plugin_ipc::{BridgeError, PluginForkError, PluginRenderFault, Result};
    pub use tutti_plugin_types::{Delivered, EditorError, LoadStage, PluginError, StateError};
}
pub use error::{BridgeError, EditorError, LoadStage, PluginForkError, PluginRenderFault, Result};

mod format;
mod host;
pub(crate) mod util;

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
    pub use crate::host::node::{Listener, ParameterChangeSink};
}

// Loads a VST2 plugin in the host process; most callers use `Plugin::open`,
// which dispatches here for `.vst` files.
#[cfg(feature = "vst2")]
pub use format::vst2_in_process::{
    BoundInProcessVst2Client, InProcessVst2Client, InProcessVst2Controls,
    load_client as in_process_vst2_client,
};

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
    pub use crate::host::plugins::{PluginId, Plugins};
    pub use tutti_plugin_ipc::{AudioConfig, CatalogConfig, NO_SCAN_DIRS};
    pub use tutti_plugin_types::{PluginDescriptor, PluginRole};
}

// Per-plugin handles: the graph node and the main-thread control surface.
pub use host::handles;

// Host policy over a handle, written once for every host: the parameter
// mirror and the job that fills it (nothing here spawns it), the death
// debounce, and when a plugin must be told the render mode.
pub use host::liveness::{Liveness, Verdict};
pub use host::param_values::{ParamRead, PluginParamValues};
pub use host::render_mode::needs_announcement;

// Wire-contract types shared with `tutti-plugin-server`.
pub mod server;
