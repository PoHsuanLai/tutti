//! Shared value vocabulary for tutti's plugin host crates.
//!
//! Each format-specific host crate (`tutti-vst2-host`, `tutti-vst3-host`,
//! `tutti-clap-host`, `tutti-au-host`) re-exports these types from its own
//! public API so callers can stay format-agnostic when only the shared
//! surface is in play.

pub mod automation;
pub mod automation_mode;
pub mod channels;
pub mod classification;
pub mod descriptor;
pub mod editor;
pub mod error;
pub mod features;
pub mod format_host;
pub mod harmony;
pub mod load_stage;
pub mod main_thread;
pub mod metadata;
pub mod midi;
pub mod note_expression;
pub mod note_id;
pub mod parameters;
pub mod presets;
pub mod process;
pub mod render_mode;
pub mod transport;

/// Re-exported so every format crate spells a sample count the same way
/// without each taking its own `tutti-types` dependency — `ChannelLayout`
/// above is here for the same reason.
pub use tutti_types::Samples;
pub use tutti_types::{ChannelLayout, ChannelTopology, Speaker};
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
pub use error::{PluginError, Result, Result as PluginResult};
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
pub use metadata::{BusChannels, LoadedPlugin, PluginTail};
pub use midi::{MidiEventVec, RtMidiEvents, MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY};
pub use note_expression::{NoteExpressionChanges, NoteExpressionType, NoteExpressionValue};
pub use note_id::{note_id_for, note_id_to_channel_note, MAX_HOST_NOTE_ID};
pub use parameters::{ParamAddress, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterInfo};
pub use presets::{Preset, PresetId, PresetSupport};
pub use process::{ExpressiveContext, ProcessContext, ProcessOutput};
pub use render_mode::RenderMode;
pub use transport::{
    is_usable, BarInfo, LoopRegion, MusicalTiming, TransportFlags, TransportInfo, TransportPosition,
};

/// Re-export of the workspace-wide MIDI event so host crates don't all
/// need to add a direct `tutti-midi-types` dependency just for the type.
pub use tutti_midi_types::MidiEvent;
