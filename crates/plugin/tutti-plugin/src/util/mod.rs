//! Reusable plumbing with no plugin- or format-specific knowledge.
//!
//! - [`transport`] — the IPC substrate: shared-memory audio slab + the
//!   control socket. Pure transport; knows nothing about plugins.
//! - [`config`] — host configuration ([`BridgeConfig`](config::BridgeConfig)
//!   per-subprocess; [`CatalogConfig`](config::CatalogConfig) +
//!   [`AudioConfig`](config::AudioConfig) app-facing).
//! - [`node`] — fundsp audio-node primitives shared by every host path
//!   (in-process and out-of-process): the MIDI inbox, the parameter/latency
//!   change sinks, the signal-routing helper, and the node-id fingerprint.
//! - [`window`] — platform window-handle plumbing for plugin editors.

pub mod config;
pub mod node;
pub mod transport;
pub mod window;
