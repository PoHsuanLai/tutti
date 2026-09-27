//! The host↔server wire contract, shared with `tutti-plugin-server`.
//!
//! **Not for general use.** These types exist so `tutti-plugin-server` can
//! build the other end of the IPC. Applications should stick to
//! [`catalog`](crate::catalog) and [`handles`](crate::handles); the value types
//! they need are re-exported there or at the crate root.
//!
//! Contains the fine-grained plugin-instance capability traits
//! ([`PluginMeta`], [`PluginAudio`], [`PluginParams`], [`PluginState`],
//! [`PluginEditorHost`]) and the [`PluginInstance`] bundle a loader satisfies,
//! plus re-exports of every wire-frame type the IPC carries.
//!
//! The traits and their per-block process types are defined in
//! `tutti-plugin-types`, so any crate can implement them without depending on
//! `tutti-plugin`; this module re-exports them.

// Splitting and reassembling plugin state across frames.
//
// Re-exported here rather than left crate-private because both ends of the
// wire must chunk *identically* — a server that split differently from the
// host would produce sequences the host refuses. One implementation, shared,
// is the only way that stays true.
pub use crate::util::transport::state_chunk;
// The host and the server must name the endpoint the same way or they never
// meet, so the server binds with the very function the host dials with.
pub use crate::util::transport::control::socket_name;

pub use crate::host::subprocess::resolve_bundle;
pub use crate::protocol::audio::{
    AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, Sample,
};

pub use crate::protocol::{
    AuComponentType, AutomationMode, BridgeMessage, BusChannels, ChannelLayout, ChordChanges,
    ChordValue, ClapFeature, EditorPresence, Features, HostMessage, IpcMidiEvent, IpcMidiEventVec,
    LoadedPlugin, MidiEvent, MidiEventVec, Normalized, NoteExpressionChanges,
    NoteExpressionIntChanges, NoteExpressionIntValue, NoteExpressionTextChanges,
    NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue, ParamAddress, ParamFlags,
    ParamId, ParamRange, ParamSteps, ParameterChanges, ParameterInfo, ParameterPoint,
    ParameterQueue, PluginClass, PluginDescriptor, PluginTail, Preset, PresetId, ProcessAudioData,
    SampleFormat, Samples, ScaleChanges, ScaleValue, SlabLayout, TimeSignature, TransportInfo,
    Vst2Category, Vst3PlugType, Vst3SubCategories, MAX_FRAME_BYTES, MAX_STATE_BYTES,
    MIDI_STACK_CAPACITY, PROTOCOL_VERSION, STATE_CHUNK_BYTES,
};
pub use crate::util::config::BridgeConfig;
pub use crate::util::transport::shm::{AudioSlab, RING_SLOTS};
pub use crate::util::window::{EditorSize, WindowHandle};
// Per-format `probed` masks, shared by every loader in this crate and in
// `tutti-plugin-server`.
pub use tutti_plugin_types::features::probed;
// Paired capability flags plus the mask saying which were probed.
pub use tutti_plugin_types::FeatureReport;
// Whether a plugin is being rendered under realtime pressure. Configure-time,
// not per block.
pub use tutti_plugin_types::RenderMode;
// Per-block process inputs/outputs, re-exported from `tutti-plugin-types`.
pub use tutti_plugin_types::{ExpressiveContext, ProcessContext, ProcessOutput};
// The fine-grained plugin-instance capability traits plus the
// `PluginInstance` bundle. A loader implements the small traits and gets
// `PluginInstance` via its blanket impl.
pub use tutti_plugin_types::{
    PluginAudio, PluginEditorHost, PluginInstance, PluginMeta, PluginParams, PluginPresets,
    PluginState,
};
// The lean, format-agnostic error the traits return, plus its `Result` alias.
pub use tutti_plugin_types::{PluginError, PluginResult};
