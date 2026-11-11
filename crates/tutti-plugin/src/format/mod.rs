//! Format-specific FFI — the only code in this crate that touches a concrete
//! plugin SDK.
//!
//! Everything else (catalog, IPC client, graph node, control surface) is
//! format-agnostic. Per-format knowledge is confined here:
//!
//! - [`gui`] — **in-process editors** for the out-of-process formats
//!   (VST3/CLAP/AU). Their *audio* runs in the `tutti-plugin-server`
//!   subprocess (via the `tutti-*-host` crates), but the editor must open in
//!   the host process because platform GUI toolkits are process-local, so each
//!   format dlopens the plugin a second time purely for its window.
//!
//! The in-process audio + editor node, generic over any
//! `tutti_plugin_types::InProcess` format, is `tutti-plugin-host`'s.

pub mod gui;
