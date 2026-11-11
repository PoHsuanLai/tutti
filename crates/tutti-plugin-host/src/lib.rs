//! The format-generic half of plugin hosting (design 021, step 4a).
//!
//! What a loaded plugin's host side needs whatever process the plugin runs in,
//! and whatever its format: the main-thread control surface
//! ([`PluginHandle`], over the capability traits a backend implements), the
//! node's controls and meter, the transport it tells the plugin and the
//! parameter changes it delivers, the host policy written once over a handle
//! ([`Liveness`], [`PluginParamValues`], [`needs_announcement`]), and the
//! in-process node ([`PluginNode`], [`load_client`]) over any
//! [`InProcess`](tutti_plugin_types::InProcess) format.
//!
//! It names no format host, no wire and no server: the process edge is
//! `tutti-plugin-ipc`'s, and `tutti-plugin` puts a served plugin behind these
//! types. Nothing here is a dependency of either.

// One `unsafe` block, the window decode of `in_process`'s `HostEditor` (the raw
// pointer `PluginHandle::open_editor` extracts): `deny` rather than `forbid` so
// that one site can say so, with its `SAFETY` comment. Nothing else here is.
#![deny(unsafe_code)]

mod capabilities;
mod control_handle;
mod controls;
mod in_process;
mod listeners;
mod liveness;
mod meter;
mod param_delivery;
mod param_input;
mod param_values;
mod render_mode;
mod segment_points;
mod transport_source;
mod transport_walk;
mod window;

pub use capabilities::{
    HostAutomationState, HostEditor, HostParams, HostPresets, HostRenderMode, HostState,
};
pub use control_handle::{OptionalCapabilities, PluginHandle, PluginStatus};
pub use controls::{ChunkCounters, ForkedSources, NodeControls, PluginControls, lock_tail};
pub use in_process::{InProcClient, PluginNode, load_client, load_client_restoring};
pub use listeners::{InvalidateSink, Listener, ParameterChangeSink, RefreshSink};
pub use liveness::{Liveness, Verdict};
pub use meter::PluginMeter;
pub use param_delivery::{DeliveryCounters, ParamDelivery, ParamDeliveryStats};
pub use param_input::take as take_param_input;
pub use param_values::{ParamRead, PluginParamValues};
pub use render_mode::needs_announcement;
pub use segment_points::SegmentPoints;
pub use transport_source::{
    BlockTransport, ProjectTime, Snapshot, SteadyTime, TransportSource, transport_info,
};
pub use transport_walk::{Call, TransportWalk};
