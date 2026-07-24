//! Audio Unit (AUv2) plugin hosting for macOS.
//!
//! Low-level bindings to Apple's AudioToolbox framework for hosting AUv2
//! plugins. Follows the same pattern as `vst3-host` and `clap-host`.
//!
//! # Platform
//!
//! macOS-only. On other platforms the crate compiles but exposes no public
//! functionality.
//!
//! # Example
//!
//! ```rust,no_run
//! # #[cfg(target_os = "macos")]
//! # {
//! use tutti_au_host::component::{enumerate_components_of_type, AuType};
//! use tutti_au_host::instance::AuInstance;
//!
//! let effects = enumerate_components_of_type(AuType::Effect);
//! if let Some(info) = effects.first() {
//!     let mut au = unsafe { AuInstance::new(info.component, 44100.0, 512) }.unwrap();
//!     au.initialize().unwrap();
//!
//!     let input = vec![vec![0.0f32; 512]; 2];
//!     let mut output = vec![vec![0.0f32; 512]; 2];
//!     let in_refs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
//!     let mut out_refs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
//!     au.process(&in_refs, &mut out_refs, 512).unwrap();
//! }
//! # }
//! ```

pub mod error;

#[cfg(target_os = "macos")]
pub mod types;

#[cfg(target_os = "macos")]
mod cf;

#[cfg(target_os = "macos")]
mod ffi;

pub mod component;

#[cfg(target_os = "macos")]
pub mod handle;

#[cfg(target_os = "macos")]
pub mod stream;

#[cfg(target_os = "macos")]
mod buffer;

#[cfg(target_os = "macos")]
pub mod instance;

#[cfg(target_os = "macos")]
pub mod parameters;

#[cfg(target_os = "macos")]
pub mod editor;

pub use component::{AuComponentInfo, AuType};
pub use error::{AuError, Result};

// Shared host vocabulary re-exported so consumers can stay format-agnostic.
// `WindowHandle` is consumed by the GUI bridge; `MidiEvent` is the input type of
// `AuInstance::send_midi`. `TransportInfo` is not re-exported — AUv2 host
// callbacks are unwired, so nothing here speaks transport.
pub use tutti_plugin_types::{EditorSize, MidiEvent, WindowHandle};

#[cfg(target_os = "macos")]
pub use editor::AuEditor;
#[cfg(target_os = "macos")]
pub use handle::AuHandle;
#[cfg(target_os = "macos")]
pub use instance::{AuInstance, AuLoaded, AuReady};
// `AuParameter`/`ParamRange`/`ParamView`/`ParameterUnit` are AU-internal param
// vocabulary — reachable via `tutti_au_host::parameters::*` for the loader, but
// not surfaced as flat crate-root re-exports. Consumers speak the shared
// `tutti_plugin_types::ParameterInfo` produced by the loader's trait impl.
#[cfg(target_os = "macos")]
pub use stream::{ChannelLayout, StreamConfig};
