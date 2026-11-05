#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every line of VST3 FFI `unsafe` lives in `tutti-vst3-ffi`.
#![forbid(unsafe_code)]

mod error;
pub mod events;
pub(crate) mod helpers;
pub mod host;
pub mod types;

pub use error::{LoadStage, Result, Vst3Error};
pub use events::{
    EventExt, MidiEncoder, Stager, ToEvent, Utf16Chord, Utf16Scale, Utf16Text, Vst3InputEvents,
    drain_midi,
};
pub use host::{
    FactoryInfo, LoadOptions, ProcessFailed, RestartOutcome, Vst3Controller, Vst3Library,
    Vst3Loaded, Vst3Processor, Vst3ProcessorAny, descriptor_of, factory_flags,
};
pub use types::{
    AudioBuffer, BufferPtrs, BusInfo, ContentScale, EditorCapabilities, EditorSize, MidiEvent,
    NoteExpressionIntValue, NoteExpressionType, NoteExpressionValue, ParameterChanges,
    ParameterPoint, ParameterQueue, PluginInfo, ProcessMode, ProcessOutput, ProcessOutputRef,
    Sample, TransportInfo, Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo,
    Vst3ProgramListInfo, Vst3Sample, Vst3UnitInfo, WindowHandle, automation_state, keyswitch_type,
    note_expression_flags, parameter_flags, physical_ui_type, prefetchable_support,
    process_context_flags, to_process_context, unit_ids,
};

pub use tutti_vst3_ffi::{
    NOTIFICATION_CAPACITY, PROGRESS_DESCRIPTION_CAPACITY, ParameterEditEvent, PluginNotification,
    ProgressDescription, ProgressEvent, RestartFlags, UnitEvent,
};

#[cfg(feature = "conformance")]
pub use tutti_vst3_ffi::RunLoopActivity;

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
