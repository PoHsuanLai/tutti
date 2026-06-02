//! Subprocess lifecycle — locate, spawn, hand-shake, and probe the
//! `plugin-server` child process.

pub mod bundle;
mod launch;
mod locate;
mod probe;

pub use bundle::resolve_bundle;
pub use launch::launch;
pub use probe::probe_metadata;
