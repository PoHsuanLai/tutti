//! Plugin bridge layer.
//!
//! [`audio::AudioBridge`] is the RT-safe command/response bus to the
//! plugin-server subprocess. [`composite::PluginBridge`] composites it
//! with an in-process GUI instance (from [`gui`]) for a unified host
//! surface.

pub mod audio;
pub mod composite;
pub mod gui;

pub use composite::PluginBridge;
pub(crate) use composite::SubprocessBackend;
