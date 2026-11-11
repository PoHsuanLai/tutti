//! Format-specific FFI — the only code in this crate that touches a concrete
//! plugin SDK.
//!
//! Everything else (catalog, IPC client, graph node, control surface) is
//! format-agnostic. Per-format knowledge is confined here, in two groups:
//!
//! - [`gui`] — **in-process editors** for the out-of-process formats
//!   (VST3/CLAP/AU). Their *audio* runs in the `tutti-plugin-server`
//!   subprocess (via the `tutti-*-host` crates), but the editor must open in
//!   the host process because platform GUI toolkits are process-local, so each
//!   format dlopens the plugin a second time purely for its window.
//! - `in_process` — the **in-process audio + editor** node, generic over any
//!   `tutti_plugin_types::InProcess` format and naming none. VST2 runs through
//!   it: unlike VST3/CLAP/AU, VST2 fuses the editor and audio processor into
//!   one `AEffect`, so the editor can't live in a different process from audio
//!   without remote-rendering pixels. Hosting in-process is the standard
//!   tradeoff (Ableton/Logic/Reaper all do it for VST2): no subprocess crash
//!   isolation, but a native editor. CLAP and VST3 implement `InProcess` too
//!   but are not routed here yet: they still run in a `plugin-server`.

pub mod gui;

pub mod in_process;
