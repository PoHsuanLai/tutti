//! Reusable plumbing with no plugin- or format-specific knowledge.
//!
//! - [`transport`] — the IPC substrate: shared-memory audio slab + the
//!   control socket. Pure transport; knows nothing about plugins.
//! - [`config`] — host configuration ([`BridgeConfig`](config::BridgeConfig)
//!   per-subprocess; [`CatalogConfig`](config::CatalogConfig) +
//!   [`AudioConfig`](config::AudioConfig) app-facing).
//! - [`node`] — node primitives shared by every host path (in-process and
//!   out-of-process): the parameter-change sinks a plugin node reports
//!   through.
//! - [`window`] — platform window-handle plumbing for plugin editors.

pub mod config;
pub mod node;
pub mod transport;
pub mod window;
