//! Per-plugin handles — the two values a loaded plugin yields.
//!
//! Every loaded plugin produces both a [`PluginClient`] (the audio-graph
//! node, owned by the graph's executor once inserted) and a [`PluginHandle`] (the main-thread control
//! surface — editor, parameters, state). They share subprocess lifetime via
//! `Arc`: the plugin stays alive as long as either does.
//!
//! The control surface splits into granular capability traits
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
pub use crate::host::node::{
    Bound, ParamDeliveryStats, PluginClient, PluginControls, PluginMeter, Unbound,
};
// The per-block installer, reachable only when the plugin can receive that
// input. Named here because a caller matching on the `Option` a
// `PluginClient` accessor returns has to be able to name what is inside it.
pub use crate::host::node::TransportView;
// `ParamAddress` because a plugin's parameters are addressed by one
// (`HostParams::parameter_value`, `set_parameter_value`): a caller of those must be
// able to name it without reaching into the private `protocol` module.
// `ParamId` for the same reason one step further in — every `ParamAddress`
// variant wraps one, so naming the address without it builds nothing.
// `Normalized` for the same rule applied to values rather than addresses:
// `HostParams::set_parameter_value` takes one, so a caller that cannot name it
// cannot call the method, and an out-of-crate backend cannot implement the
// trait at all.
pub use crate::protocol::{
    ChordValue, Normalized, ParamAddress, ParamId, Preset, PresetId, ScaleValue,
};
pub use crate::util::window::{EditorCapabilities, EditorSize};
pub use control_handle::{OptionalCapabilities, PluginHandle, PluginStatus};

/// In-process VST2 graph node, and the controls inserting it hands back. Used
/// when a host loads VST2 plugins directly in the host process (via
/// `in_process_vst2_client`); it takes MIDI and parameter events on its event
/// input.
#[cfg(feature = "vst2")]
pub use crate::format::vst2_in_process::{InProcessVst2Client, InProcessVst2Controls};
