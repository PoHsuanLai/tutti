//! The macOS implementation: every module that talks to AudioToolbox through
//! `tutti-au-ffi`. `lib.rs` gates this one module on the platform and
//! re-exports it at the crate root, so each path below is also a path of the
//! crate (`crate::types`, `tutti_au_host::offline`, ...).

pub(crate) mod aupreset;
pub(crate) mod automation;
pub(crate) mod bus;
pub(crate) mod channel_layout;
pub(crate) mod control;
pub(crate) mod controller;
pub(crate) mod discovery;
pub(crate) mod editor_instance;
pub(crate) mod edits;
mod error;
pub(crate) mod handle;
pub(crate) mod hosting;
pub(crate) mod identity;
pub(crate) mod instance;
pub(crate) mod lifecycle;
pub(crate) mod listener;
pub(crate) mod load;
pub(crate) mod marks;
pub(crate) mod midi_map;
pub mod midi_out;
pub mod offline;
pub mod parameters;
pub(crate) mod preset;
pub(crate) mod processor;
pub(crate) mod ranges;
pub mod render_notify;
pub(crate) mod stream;
pub(crate) mod topology;
pub(crate) mod transport;
pub mod types;
pub(crate) mod watch;

/// How an [`AuType::Unknown`](crate::component::AuType::Unknown) code reads:
/// its four characters.
pub(crate) fn unknown_code(code: u32) -> String {
    types::fourcc_to_string(code)
}

/// Decodes a well-known AudioUnit `OSStatus` into a short description, falling
/// back to `"unknown error"`. Shared by `AuError::message` and the `Display`
/// text of `AuError::OsStatus` / `AuError::RenderFailed` so the two cannot
/// drift.
pub(crate) fn os_status_message(code: i32) -> &'static str {
    tutti_au_ffi::Status::from_raw(code).message()
}

// Bus topology vocabulary. Unlike the parameter types below, these ARE flat
// re-exports: `bus_count` / `bus_layout` / `supported_channel_configs` are
// inherent methods on `AuLoaded`, so a caller that reaches those methods needs
// their argument and return types in scope without a second import path.
pub use bus::{AuChannelConfig, AuChannelCount, BusDirection};
// Channel *order* vocabulary, flat-re-exported for the same reason the bus
// vocabulary above is: `supported_layout_tags` / `layout_tag` / `set_layout_tag`
// are inherent methods on `AuLoaded`, so a caller cannot name their argument
// or return type without this.
pub use channel_layout::AuLayoutTag;
pub use controller::AuController;
pub use editor_instance::AuEditorInstance;
pub use handle::AuHandle;
pub use instance::AuLoaded;
pub use processor::AuProcessor;
// Tag <-> ChannelTopology, flat-re-exported like `AuLayoutTag`.
pub use topology::topology_of;
pub use tutti_au_ffi::{AuEditor, Element, Property, Scope, Unit, property};
// Flat-re-exported for the reason `AuLayoutTag` is: `AuMidiMapping` is the
// argument and return type of the five `*_parameter_midi_mapping*` methods on
// `AuLoaded`, and `MidiTrigger` is the field of it a caller must construct, so
// neither is avoidable at the call site. There is no shared
// `tutti_plugin_types` mapping vocabulary to translate into — VST3's equivalent
// is a query, not a table, so the two formats have no common shape.
pub use midi_map::{AuMidiMapping, MidiTrigger};
// `AuMidiOutput` is the registration a host holds to keep a MIDI-output callback
// installed — dropping it is what withdraws the callback, so the type has to be
// nameable in a struct field. `MidiOutSink` is the argument to
// `install_midi_output`, and `MidiOutputInfo` its capability-query return.
pub use midi_out::{AuMidiOutput, MidiOutSink, MidiOutputInfo};
// Flat-re-exported for the same reason `TransportState` below is: `PushScratch`
// is the argument type of `offline::process_push`, so a host cannot drive the
// push render path without being able to name it, and there is no shared
// `tutti_plugin_types` vocabulary for a per-bus buffer-list arena to translate
// into. The two `process_*` functions stay behind `offline::` — they are `unsafe`
// and take a raw `AudioUnit`, so reaching them should be as explicit as their
// contract.
pub use offline::{PushScratch, RENDER_QUALITY_MAX};
// `AuParameter`/`ParamRange`/`ParamView`/`ParameterUnit` are AU-internal param
// vocabulary — reachable via `tutti_au_host::parameters::*` for the loader, but
// not surfaced as flat crate-root re-exports. Consumers speak the shared
// `tutti_plugin_types::ParameterInfo` produced by the loader's trait impl.
//
// `AuPreset` is flat-re-exported where `AuParameter` is not, because it is the
// return type of `AuLoaded::factory_presets`/`current_preset`: there is no
// shared `tutti_plugin_types` preset vocabulary to translate into, so a caller
// has to be able to name it without reaching into a submodule.
pub use preset::AuPreset;
// Flat-re-exported for the same reason `AuPreset` is: `AuPresetIdentity` is the
// return type of `AuLoaded::load_preset_file` and of `read_preset_metadata`,
// which is the function a preset browser is built on, so a caller cannot name
// what it gets back without it.
pub use aupreset::{AuPresetIdentity, read_preset_metadata};
// Flat-re-exported for the same reason `AuPreset` is: `AuParameterListener` is
// the type a host names to hold a registration, `AuEvent` is what its callback
// receives, and `EventAddress` is an argument to every `watch_*` method. All
// three are unavoidable at the call site, and there is no shared
// `tutti_plugin_types` notification vocabulary to translate into.
pub use listener::{
    AuEvent, AuParameterListener, EventAddress, emit_gesture, notify_all_parameters,
};
// Flat-re-exported for the reason `AuParameterListener` is: `RenderNotify` is
// the handle a host holds, `RenderNotification`/`RenderPhase` are what its
// callback receives, and `ParamEvent`/`ScheduleAddress` are the arguments to
// `render_notify::schedule` — which is the only sanctioned place to call it, so
// every one of these is unavoidable at the call site.
pub use render_notify::{
    ParamEvent, RenderNotification, RenderNotify, RenderPhase, RenderUnit, ScheduleAddress,
};
pub use stream::{AuBusLayout, StreamConfig};
// `TransportState` is flat-re-exported for the same reason the bus vocabulary
// above is: it is the return type of `AuLoaded::install_host_callbacks` and
// the thing a host writes each block, so a caller cannot use that method
// without being able to name it.
pub use tutti_au_ffi::TransportState;
