#![doc = include_str!("../README.md")]

mod automation;
mod automation_mode;
pub mod bundle;
mod channels;
mod classification;
mod descriptor;
mod editor;
mod error;
pub mod features;
mod format_host;
mod harmony;
mod load_stage;
mod main_thread;
mod metadata;
mod midi;
mod note_expression;
mod note_id;
mod parameters;
mod presets;
mod process;
mod render_mode;
mod transport;

/// Re-exported so every format crate spells a sample count the same way
/// without each taking its own `tutti-types` dependency — `ChannelLayout`
/// above is here for the same reason.
pub use tutti_types::Samples;
pub use tutti_types::{ChannelLayout, ChannelTopology, Speaker};

mod layout_support;
pub use layout_support::LayoutSupport;
// Musical vocabulary carried on `TransportInfo`. Re-exported for the same reason
// as `ChannelLayout`: format hosts speak these at their ABI boundary and should
// not need a `tutti-types` dependency of their own to name them.
pub use tutti_types::meter::{BarNumber, BeatsPerBar, NoteValue, TimeSignature};

pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use automation_mode::AutomationMode;
pub use channels::{AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, BufferPtrs, Sample};
pub use classification::{
    clap_features_role, ClapFeature, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories,
};
pub use descriptor::{AuComponentType, EditorPresence, PluginClass, PluginDescriptor};
pub use editor::{
    AspectRatio, EditorCapabilities, EditorError, EditorSize, ResizeHints, WindowHandle,
};
// Exported as `PluginResult` only: the bare name `Result` shadows std's under a
// glob import.
pub use error::{Delivered, PluginError, Result as PluginResult, StateError};
pub use features::{FeatureReport, Features};
pub use format_host::{
    PluginAudio, PluginEditorHost, PluginInstance, PluginMeta, PluginParams, PluginPresets,
    PluginState,
};
pub use harmony::{
    ChordChanges, ChordValue, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, ScaleChanges, ScaleValue,
};
pub use load_stage::LoadStage;
pub use main_thread::{assert_main_thread, mark_main_thread};
pub use metadata::{BusChannels, BusTopologies, LoadedPlugin, PluginTail};
pub use midi::{MidiEventVec, RtMidiEvents, MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY};
// `NoteExpressionVec` is the SmallVec alias the change list is built from, so
// a caller assembling one has to name it.
pub use note_expression::{
    NoteExpressionChanges, NoteExpressionType, NoteExpressionValue, NoteExpressionVec,
};
pub use note_id::{note_id_for, note_id_to_channel_note, MAX_HOST_NOTE_ID};
pub use parameters::{
    Normalized, ParamAddress, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterInfo,
};
pub use presets::{Preset, PresetId, PresetSupport};
pub use process::{ExpressiveContext, ProcessContext, ProcessOutput};
pub use render_mode::RenderMode;
pub use transport::{
    is_usable, BarInfo, LoopRegion, MusicalTiming, TransportFlags, TransportInfo, TransportPosition,
};

/// Re-export of the workspace-wide MIDI event so host crates don't all
/// need to add a direct `tutti-midi-types` dependency just for the type.
pub use tutti_midi_types::MidiEvent;
