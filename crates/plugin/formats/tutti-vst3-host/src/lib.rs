#![doc = include_str!("../README.md")]

pub(crate) mod com;
mod error;
pub(crate) mod helpers;
pub mod host;
pub mod types;

pub use error::{LoadStage, Result, Vst3Error};
pub use host::{
    factory_flags, FactoryInfo, PluginNotifications, RestartOutcome, Vst3Instance, Vst3Library,
    Vst3Loaded,
};
pub use types::{
    automation_state, keyswitch_type, note_expression_flags, parameter_flags, physical_ui_type,
    prefetchable_support, process_context_flags, to_process_context, unit_ids, vst3_to_chord,
    vst3_to_note_expression, vst3_to_note_expression_int, vst3_to_note_expression_text,
    vst3_to_scale, AudioBuffer, BufferPtrs, BusInfo, ChordValue, EditorCapabilities, EditorSize,
    MidiEvent, NoteExpressionIntValue, NoteExpressionText, NoteExpressionType, NoteExpressionValue,
    ParameterChanges, ParameterPoint, ParameterQueue, PluginInfo, ProcessMode, ProcessOutput,
    ProcessOutputRef, Sample, ScaleValue, TransportInfo, Vst3InputEvents, Vst3KeyswitchInfo,
    Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3ProgramListInfo, Vst3Sample, Vst3UnitInfo,
    WindowHandle,
};

pub use com::{ParameterEditEvent, ProgressEvent, RestartFlags, UnitEvent};

#[cfg(feature = "conformance")]
pub use com::RunLoopActivity;

// No `///` here: a doc comment on a `pub mod` re-resolves its intra-doc links in
// this parent scope, where `Vst3Event`'s methods are not in scope. The module's
// own `//!` below is the correct home for both the prose and the links.
pub mod events {
    //! Tagged-enum wrappers over VST3's typed event structs, plus the event-type
    //! discriminant constants.
    //!
    //! Re-exported here so downstream crates that need to construct
    //! `NoteOnEvent`/`NoteOffEvent`/etc. literally (rather than going through the
    //! [`Vst3Event::to_midi`] / [`Vst3Event::from_midi`] helpers) can do so
    //! without depending on the raw `vst3` crate.

    pub use crate::types::{
        ChordEvent, DataEvent, EventHeader, LegacyMidiCcOutEvent, NoteExpressionIntValueEvent,
        NoteExpressionTextEvent, NoteExpressionValueEvent, NoteOffEvent, NoteOnEvent,
        PolyPressureEvent, ScaleEvent, TextRef, Vst3Event, K_CHORD_EVENT, K_DATA_EVENT,
        K_LEGACY_MIDI_CC_OUT_EVENT, K_NOTE_EXPRESSION_INT_VALUE_EVENT,
        K_NOTE_EXPRESSION_TEXT_EVENT, K_NOTE_EXPRESSION_VALUE_EVENT, K_NOTE_OFF_EVENT,
        K_NOTE_ON_EVENT, K_POLY_PRESSURE_EVENT, K_SCALE_EVENT,
    };
}

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
