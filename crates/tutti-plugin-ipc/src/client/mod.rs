//! Plugin bridge layer.
//!
//! [`audio::AudioBridge`] is the RT-safe command/response bus to the
//! plugin-server subprocess. [`composite::PluginBridge`] composites it with
//! an in-process GUI instance (from the host's [`EditorLoader`]) for a unified
//! host surface, or with no loader the editor in the server
//! ([`remote_editor`]'s `RemoteEditor`).

pub mod audio;
pub mod composite;
#[cfg(test)]
mod hostile_peer_tests;
mod remote_editor;

pub use composite::{EditorLoader, PluginBridge};
