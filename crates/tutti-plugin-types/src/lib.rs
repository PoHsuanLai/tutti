#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod automation;
mod automation_mode;
pub mod bundle;
mod channels;
mod classification;
mod descriptor;
mod driver;
mod editor;
mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod features;
mod format_host;
mod harmony;
mod host;
mod lifecycle;
mod load_stage;
mod main_thread;
mod metadata;
mod midi;
mod midi1_decoder;
mod note_expression;
mod note_id;
mod order;
mod parameters;
mod points;
mod presets;
#[cfg(feature = "probe-wire")]
mod probe_wire;
mod process;
mod render_mode;
mod requests;
mod sample_format;
mod sysex;
mod transport;

// Test-only global allocator for the no-alloc gates: panics on any heap
// allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

pub use tutti_rt::{RequestBit, RequestBits, RequestWord};
pub use tutti_types::{ChannelLayout, ChannelTopology, Speaker};
/// Re-exported so every format crate spells a sample count the same way
/// without each taking its own `tutti-types` dependency — `ChannelLayout`
/// above is here for the same reason.
pub use tutti_types::{MaxBlock, SampleRate, Samples};

mod layout_support;
pub use layout_support::LayoutSupport;
// Musical vocabulary carried on `TransportInfo`. Re-exported for the same reason
// as `ChannelLayout`: format hosts speak these at their ABI boundary and should
// not need a `tutti-types` dependency of their own to name them.
pub use tutti_types::{BarNumber, BeatsPerBar, NoteValue, TimeSignature};

pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use automation_mode::AutomationMode;
pub use channels::{AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, BufferPtrs, Sample};
pub use classification::{
    ClapFeature, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories, clap_features_role,
};
pub use descriptor::{AuComponentType, EditorPresence, PluginClass, PluginDescriptor};
pub use driver::{Hand, Hosted, Lifecycle, Seat};
pub use editor::{
    AspectRatio, ContentScale, EditorCapabilities, EditorError, EditorSize, InvalidContentScale,
    ResizeHints, WindowHandle,
};
pub use host::{Control, Controller, Loaded, PluginIo, Processor, Refused, Stuck};
pub use order::{StableOrder, insertion_sort_by_key};
pub use points::{PointBuf, PointPool};
pub use requests::{Edit, EditDrain, EditSink, edits, taken};
// Exported as `PluginResult` only: the bare name `Result` shadows std's under a
// glob import.
pub use error::{Delivered, PluginError, Result as PluginResult, StateError};
pub use features::{FeatureReport, Features};
pub use format_host::{
    PluginAudio, PluginEditorHost, PluginInstance, PluginMeta, PluginParams, PluginPresets,
    PluginState,
};
pub use harmony::{
    ChordChanges, ChordValue, HARMONY_CAPACITY, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, ScaleChanges, ScaleValue,
};
pub use lifecycle::{
    EFFECT_CAPACITY, Effect, Effects, Event, FDS_PER_TICK, FdId, HostOp, MAX_STEP_EFFECTS, Notice,
    Prepare, Reply, Request, RescanScope, RestartRules, Stage, State, TIMERS_PER_TICK, TimerId,
    standing, step,
};
pub use load_stage::LoadStage;
pub use main_thread::{MainThread, NotMainThread, defer_to_main, run_deferred};
pub use metadata::{BusChannels, BusTopologies, Layout, LoadedPlugin, PluginTail};
pub use midi::{MIDI_STACK_CAPACITY, MidiEventVec, RT_MIDI_CAPACITY, RtMidiEvents, WireMidi};
pub use midi1_decoder::{
    Midi1Decoder, Midi1Message, Midi1Splitter, SYSEX_OUT_CAPACITY, note_on_velocity_midi1,
};
// `NoteExpressionVec` is the alias the change list is built from, so a caller
// assembling one has to name it.
pub use note_expression::{
    NoteExpressionChanges, NoteExpressionType, NoteExpressionValue, NoteExpressionVec,
};
pub use note_id::{MAX_HOST_NOTE_ID, note_id_for, note_id_to_channel_note};
pub use parameters::{
    InvalidNormalized, Normalized, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterInfo,
};
pub use presets::{Preset, PresetId, PresetSupport};
#[cfg(feature = "probe-wire")]
pub use probe_wire::{
    ProbeOutcome, ResultError, decode_result, encode_result, read_result, result_tmp_path,
    write_result,
};
pub use process::{ExpressiveContext, ProcessContext, ProcessOutput};
pub use render_mode::RenderMode;
pub use sample_format::{SampleFormat, negotiate};
pub use sysex::{
    SYSEX_MESSAGE_CAPACITY, SysExAssembler, SysExRef, SysExStep, is_framed, push_sysex,
    push_sysex_stack, unframed,
};
pub use transport::{
    BarInfo, LoopRegion, MusicalTiming, PlayheadWatch, TransportFlags, TransportInfo,
    TransportPosition, is_usable,
};
