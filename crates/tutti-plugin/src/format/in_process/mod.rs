//! The in-process plugin node, over any [`InProcess`](tutti_plugin_types::InProcess)
//! format.
//!
//! The plugin's `Lifecycle` (doc 016 §2.6) runs on the control side; its
//! realtime `Processor` is lent to the audio thread through a
//! `tutti_rt::handoff`, which takes no lock. A block with no Processor in hand
//! (one recalled for a rate change, or refused its rate) renders silence and
//! is counted (`unlent_blocks`); one the Processor failed renders silence too
//! (`failed_blocks`). The handle's control surface is the plugin's
//! [`Control`](tutti_plugin_types::Control).
//!
//! Nothing here names a format: VST2 is routed through it by
//! [`Plugin::open`](crate::catalog::Plugin::open) (feature `vst2`), and
//! [`load_client`] takes any other `InProcess` format the caller names.

mod client;
mod control_backend;
mod fork;
mod instance;
mod loader;
mod node;

pub use client::{InProcClient, PluginNode};
pub use loader::{load_client, load_client_restoring};
