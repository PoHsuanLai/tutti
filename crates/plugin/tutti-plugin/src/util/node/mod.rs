//! fundsp audio-node primitives shared by every host path.
//!
//! Both the out-of-process node ([`crate::host::node::PluginClient`]) and the
//! in-process paths (the in-crate VST2 node, and `dawai-wasm-plugin`'s WASM
//! node) build on these. They carry no IPC or format knowledge — just the
//! pieces a fundsp `AudioUnit` needs to behave like a plugin.

pub(crate) mod listeners;
pub mod midi;
pub(crate) mod node_id;
pub mod signal;

// `ParameterChangeSink` is public so `crate::backend` can re-export it for
// out-of-crate in-process loaders (e.g. `dawai-wasm-plugin`). The refresh /
// invalidate sinks stay crate-internal — only the out-of-process bridge fires
// them, so in-process loaders never construct one.
pub use listeners::ParameterChangeSink;
pub(crate) use listeners::{InvalidateSink, RefreshSink};
pub use midi::Midi;
pub use signal::route_with_latency;
