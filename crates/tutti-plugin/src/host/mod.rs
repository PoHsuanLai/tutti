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
//! - [`handles`] — the public [`PluginHandle`](handles::PluginHandle) control
//!   surface and the granular capability traits (`HostParams`/`HostState`/
//!   `HostEditor`) each backend implements the subset of.
//! - [`plugin`] / [`plugins`] — the public load API.
//! - [`param_values`], [`liveness`], [`render_mode`] — host policy written
//!   once: the parameter mirror and its read job, the death debounce, and
//!   when to announce a render mode.

pub mod db_probe;
pub mod handles;
pub mod liveness;
pub mod node;
pub mod param_values;
pub mod plugin;
pub mod plugins;
pub mod render_mode;
pub(crate) mod subprocess_backend;
