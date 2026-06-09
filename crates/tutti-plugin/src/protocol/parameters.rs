//! Plugin parameter wire types.
//!
//! `ParameterInfo` / `ParameterFlags` and the automation points/queues now
//! live in `tutti-plugin-types` (shared with the host crates); re-exported
//! here so `crate::protocol::{ParameterInfo, ...}` keeps working.

pub use tutti_plugin_types::{
    ParameterChanges, ParameterFlags, ParameterInfo, ParameterPoint, ParameterQueue,
};
