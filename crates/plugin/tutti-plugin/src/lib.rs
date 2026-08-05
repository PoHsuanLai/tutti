//! Out-of-process audio plugin host.
//!
//! Loads VST2, VST3, CLAP, and Audio Unit plugins in isolated subprocesses,
//! bridges audio + MIDI over shared memory, and exposes each plugin as a
//! fundsp [`AudioUnit`] node. Crashes in a plugin stay contained to its
//! subprocess — the host keeps running and reports the error.
//!
//! # Quick start
//!
//! With the `json` feature for ready-made persistence (see [Features](#features)
//! — nothing is on by default):
//!
//! ```no_run
//! # #[cfg(feature = "json")]
//! # fn ex(window: &impl raw_window_handle::HasWindowHandle)
//! # -> tutti_plugin::Result<()> {
//! use std::path::PathBuf;
//! use tutti_plugin::catalog::{CatalogConfig, Plugin, Plugins};
//!
//! let plugins = Plugins::with_json_catalog(CatalogConfig::new(
//!     PathBuf::from("/my/app/plugin-db.json"),
//!     vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")],
//! ))
//! .with_fresh_scan();
//! // The catalog discovers; `Plugin::open` loads. A host that already knows
//! // the path can skip the catalog entirely.
//! let id = plugins.find("TAL-NoiseMaker").expect("scanned");
//! let plugin = Plugin::open(id.path(), 48000.0)?;
//!
//! // The main-thread control surface. Clone it before taking the node below —
//! // `into_unit` consumes the `Plugin`.
//! let handle = plugin.handle().clone();
//! let size = handle.open_editor(window)
//!     .map_err(|e| tutti_plugin::BridgeError::EditorError(e.to_string()))?;
//! println!("editor opened at {}x{}", size.width, size.height);
//!
//! // Then hand the audio node to your fundsp graph.
//! let unit = plugin.into_unit();
//! # Ok(()) }
//! ```
//!
//! # Architecture
//!
//! Every plugin runs in its own `tutti-plugin-server` subprocess. The host
//! talks to it over two channels:
//!
//! - **Control** (Unix socket / named pipe) — editor open/close, parameter
//!   reads, state save/restore. Main-thread, blocking.
//! - **Audio** (shared memory slab) — audio + MIDI + parameter changes per
//!   block. Audio-thread, lock-free.
//!
//! This split lets the audio path stay RT-safe while the main thread does
//! IPC freely for anything editor-related.
//!
//! # Two handles per plugin
//!
//! Loading a plugin returns two values:
//!
//! - [`handles::PluginClient`] — the audio-graph node. Owns the audio path;
//!   fundsp clones and routes it.
//! - [`handles::PluginHandle`] — the main-thread control surface. Editor,
//!   parameters, state. Cheap to clone (`Arc`-shared).
//!
//! Both share the subprocess lifetime — the plugin dies only when the
//! last of either drops.
//!
//! # Design principles
//!
//! The rules this crate obeys; new formats and per-block inputs should follow
//! them. Fuller rationale + the per-format capability table are in the crate
//! README.
//!
//! 1. **Define the functionality we support, then score each format against
//!    it.** [`Features`] is a fixed list of the capabilities we handle; each
//!    format either supports a row or doesn't (see the capability table in the
//!    README). Don't instead collect everything the formats emit into a neutral
//!    superset — that leaks format names into shared types and grows a special
//!    case per format.
//! 2. **Capabilities are data, not types.** Abilities ride as a [`Features`]
//!    bitset and gate sends by flag — never by matching the format, never a
//!    per-capability trait — because a loaded plugin is `Box<dyn PluginInstance>`
//!    across IPC and cannot be downcast.
//! 3. **Share the slot, not the value.** Host-installed per-block sources
//!    (MIDI, harmony, transport, automation) live in a shared
//!    `Arc<ArcSwapOption<…>>`, not a per-clone `Option`, because fundsp runs a
//!    different clone than the setter mutates — a per-clone field is a silent
//!    no-op. See [`handles::PluginClient`] and the `input_slot` module.
//! 4. **Unify by mechanism, separate by trigger.** Collapse same-mechanism
//!    code (the per-block producers became one `InputSlot`); keep systems that
//!    react to different `Changed<T>` triggers separate — merging them would
//!    couple unrelated edits.
//!
//! [`Features`]: crate::protocol::Features
//!
//! # Module map
//!
//! - [`catalog`] — discovering, persisting, and loading plugins (incl. the
//!   [`PluginDescriptor`][catalog::PluginDescriptor] /
//!   [`PluginClass`][catalog::PluginClass] identity types)
//! - [`handles`] — [`PluginClient`][handles::PluginClient] and
//!   [`PluginHandle`][handles::PluginHandle]
//! - [`server`] — wire contract for `tutti-plugin-server` only (the full set
//!   of frame types, incl. [`ParameterInfo`][server::ParameterInfo])
//! - [`BridgeConfig`] at the crate root for low-level bridge tuning
//!
//! # Features
//!
//! **Nothing is on by default.** This is a library: persistence and format
//! support are the embedding app's choices, so you wire up exactly what you
//! use and the default build pulls no `serde_json` and no format FFI.
//!
//! - `json` — JSON-file-backed [`catalog::JsonCatalog`]. One ready-made
//!   [`catalog::PluginCatalog`] impl, not the shape of the API: implement the
//!   trait over your own store instead, or skip it entirely and use the pure
//!   [`catalog::discover`] / [`PluginRecord::probe`][catalog::PluginRecord::probe]
//!   pair.
//! - `vst3`, `clap`, `au` — in-process GUI support. Loads the plugin
//!   library in the *host* process for editor rendering only; audio still
//!   runs out-of-process.
//! - `vst2` — in-process VST2 hosting (audio + MIDI + parameters + state +
//!   native editor). Unlike VST3/CLAP/AU, VST2 is always in-process: its
//!   `AEffect` fuses the editor and audio processor into one instance, so
//!   the two cannot live in separate processes.
//!
//! This crate hosts the four industry plugin formats. A host that defines its
//! *own* format can still reuse the control surface and audio-node wiring here
//! by implementing the [`backend`] traits over its own loader.
//!
//! [`AudioUnit`]: tutti_core::AudioUnit

pub mod error;
pub use error::{BridgeError, EditorError, LoadStage, Result};

mod format;
mod host;
mod util;

pub(crate) mod protocol;

#[cfg(feature = "au")]
pub use host::builder::au;
#[cfg(feature = "clap")]
pub use host::builder::clap;
#[cfg(feature = "vst2")]
pub use host::builder::vst2;
#[cfg(feature = "vst3")]
pub use host::builder::vst3;
pub use host::builder::PluginBuilder;
pub use util::config::BridgeConfig;

/// Mark the calling thread as the host's main/UI thread, enabling the
/// debug-only main-thread affinity assertions in the editor/state paths
/// (see [`tutti_plugin_types::assert_main_thread`]). Call once, on the UI
/// thread, at host startup. No-op if never called.
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

/// Building blocks for out-of-crate in-process loaders.
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
    pub use crate::host::node::{route_with_latency, Midi, ParameterChangeSink};
    pub use crate::util::node::node_id::PLUGIN_CLIENT_ID;
}

/// Load a VST2 plugin in-process (audio + native editor on the host
/// process). See [`format::vst2_in_process::load`] for details. Available
/// behind the `vst2` feature.
#[cfg(feature = "vst2")]
pub use format::vst2_in_process::load as in_process_vst2;

/// [`in_process_vst2`], keeping the concrete node instead of boxing it.
///
/// The node implements `AudioUnit` twice — once at f32, once at f64 — and a
/// `Box<dyn AudioUnit>` erases the second. A caller driving the f64 path, or
/// one needing the node's own surface (its MIDI port, its render mode), takes
/// this instead.
#[cfg(feature = "vst2")]
pub use format::vst2_in_process::{load_client as in_process_vst2_client, InProcessVst2Client};

/// Discovering, persisting, and loading plugins — pick your layer.
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
/// [`JsonCatalog`](catalog::JsonCatalog) is one such store, behind the opt-in
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
    pub use crate::util::config::{AudioConfig, CatalogConfig};
}

/// Per-plugin handles — [`PluginClient`](handles::PluginClient) (audio graph
/// node) and [`PluginHandle`](handles::PluginHandle) (main-thread control).
pub use host::handles;

/// Internal module exposed publicly for submodule lookup. Use the
/// [`catalog`] namespace instead — this is here for rustdoc linking only.
#[doc(hidden)]
pub use host::discovery;

/// Wire-contract types for `tutti-plugin-server`.
///
/// **Not for general use.** This re-export exists so the server crate
/// can share IPC struct definitions without depending on host internals.
/// End users building applications should stick to [`catalog`] and
/// [`handles`].
pub mod server;
