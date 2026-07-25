//! Host orchestration — the logic that loads, wires, and controls plugins.
//!
//! Format-agnostic (per-format FFI lives in [`crate::format`]) and
//! transport-agnostic (the IPC substrate lives in [`crate::util`]). What's
//! here is the actual plumbing of *being a plugin host*:
//!
//! - [`discovery`] — scanning plugin directories, the on-disk catalog, dedup.
//! - [`subprocess`] — spawning / locating / probing the `tutti-plugin-server`.
//! - [`ipc_client`] — the host-process client for an out-of-process plugin:
//!   the RT-safe audio command bus plus the composited control surface.
//! - [`node`] — the [`PluginClient`](node::PluginClient) fundsp node that the
//!   out-of-process plugin presents to the audio graph.
//! - [`handles`] — the public [`PluginHandle`](handles::PluginHandle) control
//!   surface and the granular capability traits (`HostParams`/`HostState`/
//!   `HostEditor`) each backend implements the subset of.
//! - [`builder`] / [`plugins`] — the public load API.

pub mod builder;
pub mod discovery;
pub mod handles;
pub mod ipc_client;
pub mod node;
pub mod plugins;
pub mod subprocess;
