//! Per-plugin handles — the two values a loaded plugin yields.
//!
//! Every loaded plugin produces both a [`PluginClient`] (the audio-graph
//! node, owned by fundsp) and a [`PluginHandle`] (the main-thread control
//! surface — editor, parameters, state). They share subprocess lifetime via
//! `Arc`: the plugin stays alive as long as either does.
//!
//! The control surface dispatches every method through the [`ControlBackend`]
//! trait, so out-of-process VST3/CLAP/AU and in-process VST2/WASM hosting all
//! share one [`PluginHandle`] surface.

pub(crate) mod control_backend;
pub(crate) mod control_handle;

pub use crate::host::ipc_client::audio::ResyncKind;
pub use crate::host::node::PluginClient;
pub use crate::host::node::{
    HarmonySource, LfoCurve, ParamAutomationSource, PluginParamTarget, TimedChord, TimedParam,
    TimedScale,
};
// The LFO shape vocabulary + the modulation-target surface (from `tutti-mod`,
// via `tutti-units`), so the app can build an [`LfoCurve`] / route to a
// [`PluginParamTarget`] without naming `tutti-units` directly.
pub use tutti_units::{LfoShape, ModParams, ModTarget};
pub use crate::protocol::{ChordValue, ScaleValue};
pub use crate::util::window::{EditorCapabilities, EditorSize};
pub use control_handle::PluginHandle;

/// In-process VST2 audio-graph node. Used when a host loads VST2 plugins
/// directly in the host process (via `in_process_vst2`). Hosts that dispatch
/// MIDI to plugins through their own routing layer can downcast graph nodes to
/// this type to read their `MidiUnitId`.
#[cfg(feature = "vst2")]
pub use crate::format::vst2_in_process::InProcessVst2Client;

// The in-process WASM audio-graph node (`InProcessWasmClient`) lives in the
// `tutti-wasm-plugin` crate alongside its loader.
