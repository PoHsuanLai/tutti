//! fundsp audio-node primitives shared by every host path.
//!
//! Both the out-of-process node ([`crate::host::node::PluginClient`]) and the
//! in-process paths (the in-crate VST2 node, and `tutti-wasm-plugin`'s WASM
//! node) build on these. They carry no IPC or format knowledge — just the
//! pieces a fundsp `AudioUnit` needs to behave like a plugin.

pub(crate) mod listeners;
pub mod midi;
pub(crate) mod node_id;
pub mod signal;

// Public so `crate::backend` can re-export them for out-of-crate in-process
// loaders (e.g. `tutti-wasm-plugin`). `ResyncSink` stays crate-internal.
pub(crate) use listeners::ResyncSink;
pub use listeners::{LatencyChangeSink, ParameterChangeSink};
pub use midi::Midi;
pub use signal::route_with_latency;
