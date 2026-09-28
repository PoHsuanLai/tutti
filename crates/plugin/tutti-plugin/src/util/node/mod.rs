//! Node primitives shared by every host path.
//!
//! Both the out-of-process node ([`crate::host::node::PluginClient`]) and the
//! in-process paths (the in-crate VST2 node, and any out-of-crate loader)
//! build on these. They carry no IPC or format knowledge — just the
//! parameter-change sinks a plugin node reports through.

pub(crate) mod listeners;

// `ParameterChangeSink` is public so `crate::backend` can re-export it for
// out-of-crate in-process loaders. The refresh / invalidate sinks stay
// crate-internal — only the out-of-process bridge fires them, so in-process
// loaders never construct one.
pub use listeners::ParameterChangeSink;
pub(crate) use listeners::{InvalidateSink, RefreshSink};
