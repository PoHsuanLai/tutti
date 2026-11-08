//! One test plugin, one library.
//!
//! [`core`] is what the plugin does and records, written once and free of any
//! plugin SDK. Each format's shell decodes its host's calls onto it and holds
//! only what that SDK forces.

pub mod core;
pub mod transcript;

mod clap;
mod vst2;
mod vst3;
