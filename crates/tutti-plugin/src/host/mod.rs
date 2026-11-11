//! Host orchestration — the logic that loads, wires, and controls plugins.
//!
//! Format-agnostic (per-format FFI lives in [`crate::format`]) and
//! transport-agnostic (the process edge, the wire and the client of a served
//! plugin are `tutti-plugin-ipc`'s). What's here is the actual plumbing of
//! *being a plugin host*:
//!
//! - [`db_probe`] — the subprocess probe that `tutti-plugin-db` scans with.
//! - [`node`] — the [`PluginClient`](node::PluginClient): the generic plugin
//!   node over the served hosting, and what the host keeps beside it.
//! - [`handles`] — the client's typestates ([`PluginClient`](node::PluginClient)
//!   is born `Unbound` and binds into a graph node).
//! - [`plugin`] / [`plugins`] — the public load API.
//!
//! The control handle, the node's controls and the host policy written once
//! over a handle live in `tutti-plugin-host`.

pub mod db_probe;
pub mod handles;
pub mod node;
pub mod plugin;
pub mod plugins;
pub(crate) mod probe;
