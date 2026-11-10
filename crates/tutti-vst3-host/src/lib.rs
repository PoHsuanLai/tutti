#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every line of VST3 FFI `unsafe` lives in `tutti-vst3-ffi`.
#![forbid(unsafe_code)]

mod error;
mod events;
pub(crate) mod helpers;
pub mod host;
pub mod types;

pub use error::{LoadCause, LoadStage, Result, Vst3Error};
pub use events::{
    Chord, Data, Event, EventExt, EventHeader, EventKind, ExpressionInt, ExpressionText,
    ExpressionValue, InputEvents, K_DATA_TYPE_MIDI_SYSEX, LegacyCcOut, MidiEncoder, NoteOff,
    NoteOn, Payload, PolyPressure, Scale, Stager, TextRef, ToEvent, Utf16Chord, Utf16Scale,
    Utf16Text, drain_midi, expression_type_from_id, expression_type_id,
};
pub use host::{
    FactoryInfo, Library, LoadOptions, ProcessFailed, ProcessorAny, RestartOutcome, Vst3Controller,
    Vst3Loaded, Vst3Processor, descriptor_of, factory_flags,
};
pub use types::{
    AudioBuffer, BufferPtrs, BusInfo, ContentScale, EditorCapabilities, EditorSize, KeyEvent,
    KeyModifiers, KeyOutcome, MidiEvent, NoteExpressionIntValue, NoteExpressionType,
    NoteExpressionValue, ParameterChanges, ParameterPoint, ParameterQueue, PlainRange, PluginInfo,
    PrefetchSupport, ProcessMode, ProcessOutput, ProcessOutputRef, Sample, TransportInfo,
    Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3ProgramListInfo, Vst3Sample,
    Vst3UnitInfo, WindowHandle, keyswitch_type, note_expression_flags, parameter_flags,
    physical_ui_type, process_context_flags, to_process_context, unit_ids,
};

pub use tutti_vst3_ffi::{
    BusDirection, MediaType, NOTIFICATION_CAPACITY, PROGRESS_DESCRIPTION_CAPACITY,
    ParameterEditEvent, PluginNotification, ProgressDescription, ProgressEvent, RestartFlags,
    UnitEvent, Vst3Error as StatusError,
};

#[cfg(feature = "conformance")]
pub use tutti_vst3_ffi::RunLoopActivity;

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
