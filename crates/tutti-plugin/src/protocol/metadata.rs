//! Plugin metadata — name, vendor, I/O counts, editor info.
//!
//! These types now live in `tutti-plugin-types` (shared with the host
//! crates); re-exported here so `crate::protocol::{PluginInfo, ...}` and the
//! wire path keep working. Wire data: rides on `BridgeMessage::PluginLoaded`.

pub use tutti_plugin_types::{AudioIO, BusDirection, BusLayout, PluginInfo};
