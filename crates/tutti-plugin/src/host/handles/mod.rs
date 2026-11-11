//! The client of a served plugin: the graph node [`PluginClient`] and its two
//! states.
//!
//! Every loaded plugin produces both a [`PluginClient`] (the audio-graph
//! node, owned by the graph's executor once inserted) and, from
//! [`PluginClient::handle`], a [`PluginHandle`](tutti_plugin_host::PluginHandle)
//! (the main-thread control surface: editor, parameters, state). They share
//! subprocess lifetime via `Arc`: the plugin stays alive as long as either
//! does. The control surface, its capability traits and the node's controls
//! are `tutti-plugin-host`'s.

pub use crate::host::node::{Bound, PluginClient, TransportView, Unbound};
