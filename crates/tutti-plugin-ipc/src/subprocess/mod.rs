//! Subprocess lifecycle: locate, spawn and hand-shake the `plugin-server` child
//! process.

pub mod bundle;
mod launch;
mod reaped;

pub use bundle::resolve_bundle;
pub use launch::{LaunchedServer, launch};
pub use reaped::Reaped;

// The `*_with` entry points are deliberately NOT re-exported. They exist so a
// test can name the server binary with a `tutti_plugin_types::Locator` instead
// of through the `TUTTI_PLUGIN_SERVER` process-global; no production caller
// chooses a binary. `launch` is the shipped surface, and it keeps the
// environment-and-`PATH` search.
