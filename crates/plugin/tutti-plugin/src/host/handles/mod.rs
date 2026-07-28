//! Per-plugin handles — the two values a loaded plugin yields.
//!
//! Every loaded plugin produces both a [`PluginClient`] (the audio-graph
//! node, owned by fundsp) and a [`PluginHandle`] (the main-thread control
//! surface — editor, parameters, state). They share subprocess lifetime via
//! `Arc`: the plugin stays alive as long as either does.
//!
//! The control surface splits into granular [`capabilities`] traits
//! ([`HostParams`](capabilities::HostParams) / [`HostState`](capabilities::HostState)
//! / [`HostEditor`](capabilities::HostEditor)), so out-of-process VST3/CLAP/AU and
//! in-process VST2 hosting each implement exactly the subset they honor while
//! sharing one [`PluginHandle`] surface.

pub(crate) mod capabilities;
pub(crate) mod control_handle;

// The public plugin→host notification vocabulary: `on_refresh` delivers
// `PluginRefresh` (cosmetic), `on_invalidate` delivers `PluginInvalidation`
// (structural). `ResyncKind` stays exported as the underlying wire signal.
pub use crate::host::ipc_client::audio::{PluginInvalidation, PluginRefresh, ResyncKind};
pub use crate::host::node::PluginClient;
pub use crate::host::node::{
    HarmonySource, LfoCurve, LfoOffset, NoteExpressionSource, OffsetCurve, ParamAutomationSource,
    PluginParamTarget, TimedChord, TimedParam, TimedScale,
};
// The LFO shape vocabulary + the modulation-target surface (from `tutti-mod`,
// via `tutti-units`), so the app can build an [`LfoCurve`] / route to a
// [`PluginParamTarget`] without naming `tutti-units` directly.
pub use crate::protocol::{ChordValue, ScaleValue};
pub use crate::util::window::{EditorCapabilities, EditorSize};
pub use control_handle::PluginHandle;
pub use tutti_units::{LfoShape, ModParams, ModTarget};

/// In-process VST2 audio-graph node. Used when a host loads VST2 plugins
/// directly in the host process (via `in_process_vst2`). Hosts that dispatch
/// MIDI to plugins through their own routing layer can downcast graph nodes to
/// this type to read their `MidiUnitId`.
#[cfg(feature = "vst2")]
pub use crate::format::vst2_in_process::InProcessVst2Client;
