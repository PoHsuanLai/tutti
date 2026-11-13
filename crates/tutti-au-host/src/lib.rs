#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every raw AudioToolbox, CoreAudio and CoreFoundation call is in `tutti-au-ffi`;
// this crate holds the logic over its safe handles.
#![forbid(unsafe_code)]

pub mod error;

pub mod component;

mod unit_code;

// The platform seam: every difference between macOS and the rest is behind this
// one module. macOS is the AudioToolbox implementation (`macos/`); elsewhere
// it is the few items the portable modules above lean on (`other.rs`).
#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
mod platform;
#[cfg(not(target_os = "macos"))]
#[path = "other.rs"]
mod platform;
// Re-exported whole, so each platform module is also a path of the crate
// (`tutti_au_host::offline`, `crate::bus`). Off macOS there is nothing public.
#[allow(unused_imports)]
pub use platform::*;

pub use component::{AuComponentInfo, AuType};
pub use error::{AuError, LoadFailedError, LoadStage, PresetFileError, PresetMismatch, Result};
pub use unit_code::{unit_measures, unit_role};

// Shared host vocabulary re-exported so consumers can stay format-agnostic.
// `WindowHandle` is consumed by the GUI bridge; `TransportInfo` is the input type of
// `AuLoaded::set_transport`, which publishes it for the AU's host callbacks to
// pull during render.
pub use tutti_plugin_types::{EditorSize, TransportInfo, WindowHandle};

// Unit types this crate's API hands back: `Samples` from `get_latency`,
// `Seconds` from `get_tail_time`. Re-exported because a consumer that cannot
// name a returned type cannot bind it — `tutti-plugin-server` depends on this
// crate but not on `tutti-types`.
pub use tutti_types::Samples;
pub use tutti_types::value::units::Seconds;
