//! Out-of-process audio plugin host.
//!
//! Loads VST2, VST3, CLAP, and Audio Unit plugins in isolated subprocesses,
//! bridges audio + MIDI over shared memory, and exposes each plugin as a
//! fundsp [`AudioUnit`] node. Crashes in a plugin stay contained to its
//! subprocess — the host keeps running and reports the error.
//!
//! # Quick start
//!
//! Load a plugin and put its node into a tutti graph. `no_run`: the load spawns
//! a subprocess against a real `.vst3` / `.clap` on disk.
//!
//! ```no_run
//! use tutti_core::{dsp::Net, SampleRate};
//! use tutti_plugin::catalog::Plugin;
//!
//! // `sample_rate` takes anything convertible to `SampleRate` — the engine's
//! // unit type, not a bare rate that could be a block size.
//! let plugin = Plugin::open("/usr/lib/vst3/MyPlugin.vst3", SampleRate::new(48_000.0))?;
//! println!("{} by {}", plugin.descriptor().name, plugin.descriptor().vendor);
//!
//! // Two handles, one subprocess. `into_parts` hands back both, because
//! // `into_unit` alone consumes the `Plugin` and the control surface is still
//! // wanted afterwards — the plugin dies when the last of either drops.
//! let (unit, handle) = plugin.into_parts();
//! println!("reported latency: {:?}", handle.loaded().latency());
//!
//! // The node is a fundsp `AudioUnit`, so it enters `Net` like any other.
//! let mut net = Net::new(0, 2);
//! let id = net.push(unit);
//! net.pipe_output(id);
//! net.commit();
//! # Ok::<(), tutti_plugin::BridgeError>(())
//! ```
//!
//! A host that does not already know the path discovers one first — see
//! [`catalog`], whose two pure functions ([`discover`](catalog::discover) and
//! [`PluginRecord::probe`](catalog::PluginRecord::probe)) need no feature flag,
//! and whose [`Plugins`](catalog::Plugins) adds incremental rescan and crash
//! recovery on top.
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
//! 1. **Define the functionality the host supports, then score each format
//!    against it.** [`Features`] is a fixed list of those capabilities; each
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
//! 5. **The plugin lifecycle stays inside the format crate.** Each format's
//!    state machine is modelled in that format's own vocabulary and never
//!    crosses the IPC boundary — see [the section below](#the-plugin-state-machine).
//!
//! [`Features`]: crate::protocol::Features
//!
//! # The plugin state machine
//!
//! The shared vocabulary describes a plugin as a set of *capabilities* —
//! `PluginMeta`, `PluginAudio`, `PluginParams` and their siblings in
//! `tutti_plugin_types::format_host` — and deliberately says nothing about what
//! state a plugin is in. That crate's docs argue the case; the consequence for
//! this one is that a lifecycle never reaches it. A plugin arrives already
//! loaded and activated, driven inside the subprocess by whichever format crate
//! owns it, and `BridgeMessage::PluginLoaded` reports only the outcome. A probe
//! never activates at all.
//!
//! What that buys is room. No format crate has to meet another in the middle, so
//! each one models its own lifecycle as tightly as its own contract allows — and
//! left to do that, the four land on three different answers. They are worth
//! reading together, because the differences are not stylistic: each is the
//! format's own rule showing through, and the same reasoning decides the shape
//! of any format added later.
//!
//! All four start from the same two states. A plugin is *loaded* — library
//! mapped, instance created, parameters and editor reachable — and later
//! *activated*, which allocates buffers at a fixed sample rate and block size
//! and makes `process` legal. Everything below is disagreement about what to do
//! with that shape.
//!
//! ## Which model each format gets, and why
//!
//! Three modelling strategies are in use, and the choice is forced by the
//! format's own contract rather than picked for consistency.
//!
//! **Consuming type-state — VST3 and CLAP.** `Vst3Loaded → Vst3Instance<T>` and
//! `ClapLoaded → ClapActive<T>`. Both formats define a large, fully legal
//! pre-activation surface: the parameter tree, units and program lists, note
//! expression, state save/restore and the editor are all reachable before any
//! audio buffer exists, and a host is *expected* to read them there. "Loaded but
//! not processing" is a state a user spends real time in, so it earns a type of
//! its own. The transitions take `self` by value and hand back the other type
//! (`activate(self) -> Result<Active>`, `deactivate(self) -> Loaded`), which is
//! what makes a stale handle to a deactivated plugin unrepresentable rather than
//! merely discouraged — the compiler rejects it, and no runtime `is_active`
//! check is needed on the process path. The `T` parameter fixes the sample width
//! at the same moment, because both formats commit to a sample format in the
//! same call that allocates the buffers.
//!
//! Two details differ, and each is the format's rule showing through. VST3
//! chooses its `ProcessMode` on the transition rather than on the instance,
//! because `setupProcessing` delivers it exactly once per activation. CLAP's
//! `activate` returns `Err((Self, ClapError))` — the *unconsumed* `ClapLoaded`
//! comes back on refusal, so a plugin that declines 64-bit audio can be retried
//! at `f32` without being reloaded.
//!
//! **One fused type — VST2.** `Vst2Instance` has no split, because VST2 has no
//! meaningful state to split off: `effOpen`, `effSetSampleRate`,
//! `effSetBlockSize` and the first `effMainsChanged(1)` all run during
//! construction, and the instance is ready to process the moment it exists. A
//! `Vst2Loaded` type would carry no operations the fused type does not, so the
//! split would buy a type boundary that guards nothing. Suspend and resume
//! remain as ordinary `&mut self` methods with a `resumed: bool`, because in
//! VST2 they are a *reconfiguration bracket* — the thing you do around a sample
//! rate change — and not a lifecycle stage a host parks in.
//!
//! **Internal state enum — AU.** `AuInstance` holds a private `State` of
//! `Loaded` / `Ready`, and `initialize` / `uninitialize` take `&mut self` and
//! return `Result<()>`. The consuming type-state is unavailable here because
//! both AU transitions are fallible in *both* directions: `AudioUnitInitialize`
//! can fail, and so can the uninitialize that would undo it. A consuming
//! transition must produce one of the two types, and a failed transition belongs
//! to neither — the unit is left in a state that is not the one it started in
//! and not the one it was going to. The enum can name that (its `Empty` variant
//! is the transient a `mem::replace` passes through); a pair of consuming
//! functions cannot without handing back a third type nobody wants. The price is
//! that misuse is a runtime `Uninitialized` error rather than a compile error.
//!
//! ## Choosing a shape for a fifth format
//!
//! Read together, the three answers reduce to one question asked twice. Do the
//! two states have genuinely different operations, and can the transition
//! between them fail in a way that belongs to neither? Two distinct surfaces and
//! a transition that always lands somewhere is the case a consuming type-state
//! was made for. A pre-activation state with nothing of its own to do should be
//! fused, because the extra type guards nothing. A transition that can fail in
//! both directions has to carry its state as data and pay for the check at
//! runtime, because there is no third type to return.
//!
//! The failure worth naming is the first one: reaching for a compile-time
//! guarantee the underlying contract cannot honour. That is how a type ends up
//! confidently describing a state the plugin is not actually in, which is worse
//! than the runtime check it replaced.
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
//! - `json` — JSON-file-backed `catalog::JsonCatalog`. One ready-made
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
// `PluginHandle::presets` hands back these, and `load_preset` takes one, so a
// caller must be able to name them without depending on tutti-plugin-types.
pub use tutti_plugin_types::{FeatureReport, Preset, PresetId, PresetSupport};

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
