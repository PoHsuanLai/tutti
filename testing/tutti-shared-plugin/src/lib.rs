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

#[cfg(target_os = "macos")]
mod au;
#[cfg(target_os = "macos")]
pub use au::{
    AU_ASYNC_SUBTYPE, AU_ID, AU_MANUFACTURER, AU_NAME, AU_SUBTYPE, AU_TYPE, AU_VERSION,
    PlugInInterface, TuttiSharedAuFactory, register_au, register_au_async_only,
};
