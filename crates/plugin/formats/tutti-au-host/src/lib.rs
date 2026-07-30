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
pub mod bus;

#[cfg(target_os = "macos")]
pub mod stream;

#[cfg(target_os = "macos")]
pub mod channel_layout;

#[cfg(target_os = "macos")]
pub mod identity;

#[cfg(target_os = "macos")]
pub mod midi_map;

#[cfg(target_os = "macos")]
pub mod midi_out;

#[cfg(target_os = "macos")]
mod buffer;

#[cfg(target_os = "macos")]
pub mod instance;

#[cfg(target_os = "macos")]
pub mod offline;

#[cfg(target_os = "macos")]
pub mod transport;

#[cfg(target_os = "macos")]
pub mod parameters;

#[cfg(target_os = "macos")]
pub mod preset;

#[cfg(target_os = "macos")]
pub mod aupreset;

#[cfg(target_os = "macos")]
pub mod listener;

#[cfg(target_os = "macos")]
pub mod render_notify;

#[cfg(target_os = "macos")]
pub mod editor;

pub use component::{AuComponentInfo, AuType};
pub use error::{AuError, PresetFileError, PresetMismatch, Result};

// Shared host vocabulary re-exported so consumers can stay format-agnostic.
// `WindowHandle` is consumed by the GUI bridge; `MidiEvent` is the input type of
// `AuInstance::send_midi`; `TransportInfo` is the input type of
// `AuInstance::set_transport`, which publishes it for the AU's host callbacks to
// pull during render.
pub use tutti_plugin_types::{EditorSize, MidiEvent, TransportInfo, WindowHandle};

// Bus topology vocabulary. Unlike the parameter types below, these ARE flat
// re-exports: `bus_count` / `bus_layout` / `supported_channel_configs` are
// inherent methods on `AuInstance`, so a caller that reaches those methods needs
// their argument and return types in scope without a second import path.
#[cfg(target_os = "macos")]
pub use bus::{AuChannelConfig, AuChannelCount, BusDirection};
// Channel *order* vocabulary, flat-re-exported for the same reason the bus
// vocabulary above is: `supported_layout_tags` / `layout_tag` / `set_layout_tag`
// are inherent methods on `AuInstance`, so a caller cannot name their argument
// or return type without this.
#[cfg(target_os = "macos")]
pub use channel_layout::AuLayoutTag;
// `AuMidiOutput` is the registration a host holds to keep a MIDI-output callback
// installed — dropping it is what withdraws the callback, so the type has to be
// nameable in a struct field. `MidiOutSink` is the argument to
// `install_midi_output`, and `MidiOutputInfo` its capability-query return.
#[cfg(target_os = "macos")]
pub use editor::AuEditor;
#[cfg(target_os = "macos")]
pub use handle::AuHandle;
#[cfg(target_os = "macos")]
pub use instance::{AuInstance, AuLoaded, AuReady};
// Flat-re-exported for the same reason `TransportState` below is: `PushScratch`
// is the argument type of `offline::process_push`, so a host cannot drive the
// push render path without being able to name it, and there is no shared
// `tutti_plugin_types` vocabulary for a per-bus buffer-list arena to translate
// into. The two `process_*` functions stay behind `offline::` — they are `unsafe`
// and take a raw `AudioUnit`, so reaching them should be as explicit as their
// contract.
// Flat-re-exported for the reason `AuLayoutTag` is: `AuMidiMapping` is the
// argument and return type of the five `*_parameter_midi_mapping*` methods on
// `AuInstance`, and `MidiTrigger` is the field of it a caller must construct, so
// neither is avoidable at the call site. There is no shared
// `tutti_plugin_types` mapping vocabulary to translate into — VST3's equivalent
// is a query, not a table, so the two formats have no common shape.
#[cfg(target_os = "macos")]
pub use midi_map::{AuMidiMapping, MidiTrigger};
#[cfg(target_os = "macos")]
pub use midi_out::{AuMidiOutput, MidiOutSink, MidiOutputInfo};
#[cfg(target_os = "macos")]
pub use offline::{PushScratch, RENDER_QUALITY_MAX};
// `AuParameter`/`ParamRange`/`ParamView`/`ParameterUnit` are AU-internal param
// vocabulary — reachable via `tutti_au_host::parameters::*` for the loader, but
// not surfaced as flat crate-root re-exports. Consumers speak the shared
// `tutti_plugin_types::ParameterInfo` produced by the loader's trait impl.
//
// `AuPreset` is flat-re-exported where `AuParameter` is not, because it is the
// return type of `AuInstance::factory_presets`/`current_preset`: there is no
// shared `tutti_plugin_types` preset vocabulary to translate into, so a caller
// has to be able to name it without reaching into a submodule.
#[cfg(target_os = "macos")]
pub use preset::AuPreset;
// Flat-re-exported for the same reason `AuPreset` is: `AuPresetIdentity` is the
// return type of `AuInstance::load_preset_file` and of `read_preset_metadata`,
// which is the function a preset browser is built on, so a caller cannot name
// what it gets back without it.
#[cfg(target_os = "macos")]
pub use aupreset::{read_preset_metadata, AuPresetIdentity};
// Flat-re-exported for the same reason `AuPreset` is: `AuParameterListener` is
// the type a host names to hold a registration, `AuEvent` is what its callback
// receives, and `EventAddress` is an argument to every `watch_*` method. All
// three are unavoidable at the call site, and there is no shared
// `tutti_plugin_types` notification vocabulary to translate into.
#[cfg(target_os = "macos")]
pub use listener::{AuEvent, AuParameterListener, EventAddress};
// Flat-re-exported for the reason `AuParameterListener` is: `RenderNotify` is
// the handle a host holds, `RenderNotification`/`RenderPhase` are what its
// callback receives, and `ParamEvent`/`ScheduleAddress` are the arguments to
// `render_notify::schedule` — which is the only sanctioned place to call it, so
// every one of these is unavoidable at the call site.
#[cfg(target_os = "macos")]
pub use render_notify::{
    ParamEvent, RenderNotification, RenderNotify, RenderPhase, RenderUnit, ScheduleAddress,
};
#[cfg(target_os = "macos")]
pub use stream::{AuBusLayout, StreamConfig};
// `TransportState` is flat-re-exported for the same reason the bus vocabulary
// above is: it is the return type of `AuInstance::install_host_callbacks` and
// the thing a host writes each block, so a caller cannot use that method
// without being able to name it.
#[cfg(target_os = "macos")]
pub use transport::TransportState;
