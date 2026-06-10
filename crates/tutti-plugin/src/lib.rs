//! Out-of-process audio plugin host.
//!
//! Loads VST2, VST3, CLAP, and Audio Unit plugins in isolated subprocesses,
//! bridges audio + MIDI over shared memory, and exposes each plugin as a
//! fundsp [`AudioUnit`] node. Crashes in a plugin stay contained to its
//! subprocess — the host keeps running and reports the error.
//!
//! # Quick start
//!
//! ```no_run
//! # #[cfg(feature = "json")]
//! # fn ex(window: &impl raw_window_handle::HasWindowHandle)
//! # -> tutti_plugin::Result<()> {
//! use std::path::PathBuf;
//! use tutti_plugin::catalog::PluginsConfig;
//!
//! let plugins = PluginsConfig::new(
//!     PathBuf::from("/my/app/plugin-db.json"),
//!     vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")],
//! )
//! .build()
//! .with_fresh_scan();
//! let (unit, handle) = plugins.load_by_name("TAL-NoiseMaker", 48000.0)?;
//!
//! // `unit` is a `Box<dyn AudioUnit>` that goes into your fundsp graph.
//! // `handle` is the main-thread control surface.
//! let size = handle.open_editor(window)
//!     .map_err(|e| tutti_plugin::BridgeError::EditorError(e.to_string()))?;
//! println!("editor opened at {}x{}", size.width, size.height);
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
//! # Module map
//!
//! - [`catalog`] — discovering, persisting, and loading plugins
//! - [`handles`] — [`PluginClient`][handles::PluginClient] and
//!   [`PluginHandle`][handles::PluginHandle]
//! - [`metadata`] — plugin + parameter descriptors
//! - [`server`] — wire contract for `tutti-plugin-server` only
//! - [`BridgeConfig`] at the crate root for low-level bridge tuning
//!
//! # Features
//!
//! - `json` *(default)* — JSON-file-backed [`catalog::JsonCatalog`]. Disable
//!   with `default-features = false` to drop the `serde_json` dep when
//!   you're bringing your own [`catalog::PluginCatalog`] impl.
//! - `vst3`, `clap`, `au` — in-process GUI support. Loads the plugin
//!   library in the *host* process for editor rendering only; audio still
//!   runs out-of-process.
//! - `vst2` — out-of-process VST2 hosting (audio + MIDI + parameters +
//!   state). No in-process editor yet — opening the editor returns an
//!   error. Requires `tutti-plugin-server` to be built with the matching
//!   `vst2` feature.
//!
//! In-process WASM Component Model audio plugins (`dawai:audio-plugin@0.1.0`)
//! live in the separate `tutti-wasm-plugin` crate, which reuses this crate's
//! [`backend`] machinery. They never go through `tutti-plugin-server` — the
//! wasmtime sandbox provides equivalent isolation to a subprocess.
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

/// Building blocks for out-of-crate in-process loaders.
///
/// **Not part of the general API.** These let a sibling crate (e.g.
/// `tutti-wasm-plugin`) implement [`ControlBackend`] over its own plugin
/// and hand the result to
/// [`PluginHandle::from_backend`](handles::PluginHandle::from_backend),
/// reusing this crate's main-thread control surface and audio-node wiring
/// without re-implementing them. End users loading plugins should stick to
/// [`catalog`] and [`handles`].
pub mod backend {
    pub use crate::host::node::{
        route_with_latency, LatencyChangeSink, Midi, ParameterChangeSink,
    };
    pub use crate::host::handles::control_backend::ControlBackend;
    pub use crate::util::node::node_id::PLUGIN_CLIENT_ID;
}

/// Load a VST2 plugin in-process (audio + native editor on the host
/// process). See [`format::vst2_in_process::load`] for details. Available
/// behind the `vst2-in-process` feature.
#[cfg(feature = "vst2-in-process")]
pub use format::vst2_in_process::load as in_process_vst2;

// WASM Component Model audio plugins live in the `tutti-wasm-plugin` crate
// (`tutti_wasm_plugin::load`) — extracted so the heavy wasmtime dependency
// stays out of this crate. They reuse this crate's [`backend`] machinery.

/// Discovering, persisting, and loading plugins.
///
/// [`Plugins`](catalog::Plugins) is the primary entry point — it wraps a
/// [`PluginCatalog`](catalog::PluginCatalog) (a pluggable record store) and
/// exposes a fluent API for scanning plugin directories and loading plugins
/// by name or id.
///
/// The default catalog is a JSON file on disk
/// ([`JsonCatalog`](crate::host::discovery::JsonCatalog), behind the `json` feature).
/// Ship your own [`PluginCatalog`](catalog::PluginCatalog) impl for SQLite,
/// in-memory, or any other persistence.
pub mod catalog {
    #[cfg(feature = "json")]
    pub use crate::host::discovery::JsonCatalog;
    pub use crate::host::discovery::{
        AuComponentType, Blacklist, CatalogExt, PluginCatalog, PluginClass, PluginDescriptor,
        PluginFormat, PluginRecord, PluginScanner, ScanHandle, ScanPhase, ScanProgress, ScanResult,
        Vst2Category,
    };
    pub use crate::host::plugins::{PluginId, Plugins};
    pub use crate::util::config::PluginsConfig;
}

/// Per-plugin handles — [`PluginClient`](handles::PluginClient) (audio graph
/// node) and [`PluginHandle`](handles::PluginHandle) (main-thread control).
pub use host::handles;

/// Plugin + parameter descriptors.
///
/// These types describe the plugin itself (identity, audio I/O, whether
/// it has an editor) and the parameters it exposes (name, range, flags).
/// They're also part of the host↔server wire contract — re-exported
/// through [`server`] for that path.
pub mod metadata {
    pub use crate::protocol::{
        AuComponentType, BusChannels, LoadedPlugin, ParameterFlags, ParameterInfo, PluginClass,
        PluginDescriptor, SampleFormat, TransportInfo, Vst2Category,
    };
}

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
