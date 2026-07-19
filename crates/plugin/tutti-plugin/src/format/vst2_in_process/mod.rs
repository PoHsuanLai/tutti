//! In-process VST2 host.
//!
//! Pairs `tutti_vst2_host::Vst2Instance` with the same `PluginHandle` API the
//! out-of-process backends expose. The audio thread reaches the plugin
//! through `try_lock` — when the GUI thread is in the middle of a long
//! `editor_idle` call the audio thread emits silence and bumps a
//! contention counter rather than blocking.

mod audio_unit;
mod control_backend;
mod loader;

#[allow(unused_imports)]
pub use audio_unit::InProcessVst2Client;
pub use loader::load;
