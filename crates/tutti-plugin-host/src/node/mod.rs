//! The plugin node, over any [`Loaded`](tutti_plugin_types::Loaded) hosting: an
//! [`InProcess`](tutti_plugin_types::InProcess) format ([`load_client`]), or a
//! plugin served by another process ([`client_over`], PR 3a of design 021 step 4b).
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
//! `tutti-plugin`'s `Plugin::open` (feature `vst2`), and
//! [`load_client`] takes any other `InProcess` format the caller names.

mod client;
mod control_backend;
mod fork;
mod graph;
mod instance;
mod loader;

pub use client::{InProcClient, PluginNode};
pub use loader::{client_over, load_client, load_client_restoring};
