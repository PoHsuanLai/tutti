//! Subprocess lifecycle — locate, spawn, hand-shake, and probe the
//! `plugin-server` child process.

pub mod bundle;
mod launch;
mod locate;
mod probe;

pub use bundle::resolve_bundle;
pub use launch::launch;
pub use probe::probe_metadata;

// The `*_with` entry points and `locate::ServerLocator` are deliberately NOT
// re-exported here. They exist so a test can name the server binary as an
// argument instead of through the `TUTTI_PLUGIN_SERVER` process-global; no
// production caller chooses a binary, so exporting them would add public API
// with no consumer. `launch` and `probe_metadata` remain the whole shipped
// surface, and both keep the environment-and-`PATH` search as their default.
