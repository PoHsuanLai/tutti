//! Transport / playback position snapshot.
//!
//! Re-export of the cross-format [`tutti_plugin_types::TransportInfo`] so
//! the IPC protocol speaks the same type as the host crates do.

pub use tutti_plugin_types::TransportInfo;
