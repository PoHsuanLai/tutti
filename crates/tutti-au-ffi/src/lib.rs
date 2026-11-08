#![doc = include_str!("../README.md")]
#![cfg(target_os = "macos")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod cf;
mod component;
mod editor;
mod error;
mod guard;
mod listener;
mod midi_out;
mod parameters;
mod plain;
mod render;
mod render_notify;
mod transport;
mod unit;

pub use cf::{
    NotADictionary, PresetFacts, cfstring_to_string, cfstring_to_string_checked, preset_facts,
    preset_with_identity,
};
pub use component::ComponentRef;
pub use editor::AuEditor;
pub use error::{Error, OsError, Result, check};
pub use listener::{
    AuEvent, AuParameterListener, EventAddress, emit_gesture, notify_all_parameters,
    set_parameter_notifying,
};
pub use midi_out::{
    AuMidiOutput, MidiOutSink, MidiOutputInfo, install as install_midi_output, midi_output_info,
};
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub use midi_out::{decode_packet_list_for_test, split_packet_list_for_test};
pub use plain::Plain;
pub use render::{
    BusLayout, PushScratch, Renderer, output_byte_sizes, process_push, process_push_multiple,
};
pub use render_notify::{
    ParamEvent, RenderNotification, RenderNotify, RenderPhase, ScheduleAddress, schedule,
};
pub use transport::{HostCallbacks, TransportState};
pub use unit::{PropertyInfo, RenderUnit, Unit};
