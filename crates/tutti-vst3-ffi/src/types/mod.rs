//! The VST3 value vocabulary: events, the info records a plugin reports,
//! the process context and the sample-format tag.

mod audio;
mod events;

mod info;
mod transport;

pub use audio::{K_SAMPLE_32_INT, K_SAMPLE_64_INT, ProcessMode, SampleWidth};
pub use events::{
    Chord, Data, Event, EventHeader, EventKind, ExpressionInt, ExpressionText, ExpressionValue,
    K_DATA_TYPE_MIDI_SYSEX, LegacyCcOut, NoteOff, NoteOn, Payload, PolyPressure, Scale, TextArena,
    TextRef, UnknownEventKind,
};
pub use info::{
    BusInfo, Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3ProgramListInfo,
    Vst3UnitInfo, automation_state, keyswitch_type, note_expression_flags, parameter_flags,
    physical_ui_type, prefetchable_support, unit_ids,
};
pub use transport::{process_context_flags, to_process_context};
