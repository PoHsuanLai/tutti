//! Shared value vocabulary for tutti's plugin host crates.
//!
//! Each format-specific host crate (`tutti-vst2-host`, `tutti-vst3-host`,
//! `tutti-clap-host`, `tutti-au-host`) re-exports these types from its own
//! public API so callers can stay format-agnostic when only the shared
//! surface is in play.

pub mod automation;
pub mod channels;
pub mod classification;
pub mod editor;
pub mod features;
pub mod harmony;
pub mod load_stage;
pub mod main_thread;
pub mod metadata;
pub mod note_expression;
pub mod note_id;
pub mod parameters;
pub mod transport;

pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use channels::{AudioBuffer, AudioBuffer32, AudioBuffer64, BufferPtrs, Sample};
pub use classification::Vst2Category;
pub use features::Features;
pub use main_thread::{assert_main_thread, mark_main_thread};
pub use editor::{
    AspectRatio, EditorCapabilities, EditorError, EditorSize, ResizeHints, WindowHandle,
};
pub use harmony::{
    ChordChanges, ChordValue, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, ScaleChanges, ScaleValue,
};
pub use load_stage::LoadStage;
pub use metadata::{BusChannels, LoadedPlugin};
pub use note_expression::{NoteExpressionChanges, NoteExpressionType, NoteExpressionValue};
pub use note_id::{note_id_for, note_id_to_channel_note, MAX_HOST_NOTE_ID};
pub use parameters::{ParameterFlags, ParameterInfo};
pub use transport::{
    BarInfo, LoopRegion, MusicalTiming, TransportInfo, TransportPosition, TransportState,
};

/// Re-export of the workspace-wide MIDI event so host crates don't all
/// need to add a direct `tutti-midi-types` dependency just for the type.
pub use tutti_midi_types::MidiEvent;
