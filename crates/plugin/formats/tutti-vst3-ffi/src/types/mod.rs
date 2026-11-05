//! The VST3 value vocabulary: events, the info records a plugin reports,
//! the process context and the sample-format tag.

mod audio;
mod events;
mod info;
mod transport;

pub use audio::{K_SAMPLE_32_INT, K_SAMPLE_64_INT, ProcessMode, Vst3Sample};
pub use events::{
    ChordEvent, ChordValue, DataEvent, EventHeader, K_CHORD_EVENT, K_DATA_EVENT,
    K_DATA_TYPE_MIDI_SYSEX, K_LEGACY_MIDI_CC_OUT_EVENT, K_NOTE_EXPRESSION_INT_VALUE_EVENT,
    K_NOTE_EXPRESSION_TEXT_EVENT, K_NOTE_EXPRESSION_VALUE_EVENT, K_NOTE_OFF_EVENT, K_NOTE_ON_EVENT,
    K_POLY_PRESSURE_EVENT, K_SCALE_EVENT, LegacyMidiCcOutEvent, NoteExpressionIntValue,
    NoteExpressionIntValueEvent, NoteExpressionText, NoteExpressionTextEvent,
    NoteExpressionValueEvent, NoteOffEvent, NoteOnEvent, PolyPressureEvent, ScaleEvent, ScaleValue,
    SysExEvent, TextArena, TextRef, Vst3Event, Vst3InputEvents, from_c_event, midi1_data_event,
    note_expression_to_vst3, note_expression_type_from_id, note_expression_type_to_id, to_c_event,
    vst3_event_from_midi, vst3_to_chord, vst3_to_note_expression, vst3_to_note_expression_int,
    vst3_to_note_expression_text, vst3_to_scale,
};
pub use info::{
    BusInfo, Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3ProgramListInfo,
    Vst3UnitInfo, automation_state, keyswitch_type, note_expression_flags, parameter_flags,
    physical_ui_type, prefetchable_support, unit_ids,
};
pub use transport::{process_context_flags, to_process_context};
