//! Host orchestration — the logic that loads, wires, and controls plugins.
//!
//! Format-agnostic (per-format FFI lives in [`crate::format`]) and
//! transport-agnostic (the process edge, the wire and the client of a served
//! plugin are `tutti-plugin-ipc`'s). What's here is the actual plumbing of
//! *being a plugin host*:
//!
//! - [`db_probe`] — the subprocess probe that `tutti-plugin-db` scans with.
//! - `subprocess_backend` — a served plugin's client behind the handles'
//!   capability traits.
//! - [`node`] — the [`PluginClient`](node::PluginClient) graph node that the
//!   out-of-process plugin presents to the audio graph.
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
pub(crate) mod subprocess_backend;
