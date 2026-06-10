//! Plugin load metadata on the IPC wire.
//!
//! The catalog-identity type ([`PluginDescriptor`], with its per-format
//! [`PluginClass`]) lives in `crate::discovery::record`; the runtime
//! engine-wiring type ([`LoadedPlugin`]) lives in the format-agnostic
//! `tutti-plugin-types`. Both are re-exported here so `crate::protocol::{...}`
//! and the wire path keep a single import point. Wire data: rides on
//! `BridgeMessage::PluginLoaded` (descriptor + loaded) / probe replies
//! (descriptor only).

pub use crate::discovery::record::{AuComponentType, PluginClass, PluginDescriptor, Vst2Category};
pub use tutti_plugin_types::{BusChannels, LoadedPlugin};
