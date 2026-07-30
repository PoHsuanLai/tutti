//! Host↔server wire contract.
//!
//! Not part of the end-user API — these types exist so `tutti-plugin-server`
//! can build the other end of the IPC. Regular library users should not
//! reach into this module.
//!
//! Contains the fine-grained plugin-instance capability traits
//! ([`PluginMeta`](crate::server::PluginMeta), [`PluginAudio`](crate::server::PluginAudio),
//! [`PluginParams`](crate::server::PluginParams), [`PluginState`](crate::server::PluginState),
//! [`PluginEditorHost`](crate::server::PluginEditorHost)) and the
//! [`PluginInstance`](crate::server::PluginInstance) bundle a loader satisfies,
//! plus re-exports of every wire-frame type the IPC carries.
//!
//! The traits and their per-block process types live in `tutti-plugin-types`
//! (so any crate can implement them without depending on `tutti-plugin`);
//! this module re-exports them at the historical `tutti_plugin::server::*`
//! import point.

pub use crate::host::subprocess::resolve_bundle;
pub use crate::protocol::audio::{
    AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, Sample,
};

pub use crate::protocol::{
    AuComponentType, AutomationMode, BridgeMessage, BusChannels, ChannelLayout, ChordChanges,
    ChordValue, Features, HostMessage, IpcMidiEvent, IpcMidiEventVec, LoadedPlugin, MidiEvent,
    MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue,
    ParamFlags, ParamRange, ParamSteps, ParameterChanges, ParameterInfo, ParameterPoint,
    ParameterQueue, PluginClass, PluginDescriptor, ProcessAudioData, SampleFormat, ScaleChanges,
    ScaleValue, SlabLayout, TimeSignature, TransportInfo, Vst2Category, MIDI_STACK_CAPACITY,
    PROTOCOL_VERSION,
};
pub use crate::util::config::BridgeConfig;
pub use crate::util::transport::shm::{AudioSlab, RING_SLOTS};
pub use crate::util::window::{EditorSize, WindowHandle};
/// Per-block process inputs/outputs, re-exported from `tutti-plugin-types`.
pub use tutti_plugin_types::{ExpressiveContext, ProcessContext, ProcessOutput};
/// The fine-grained plugin-instance capability traits plus the
/// [`PluginInstance`](tutti_plugin_types::PluginInstance) bundle, re-exported
/// from `tutti-plugin-types` so the loaders reach them through the same
/// `tutti_plugin::server::*` import point. A loader implements the small traits
/// ([`PluginMeta`], [`PluginAudio`], [`PluginParams`], [`PluginState`],
/// [`PluginEditorHost`]) and gets `PluginInstance` via its blanket impl.
pub use tutti_plugin_types::{
    PluginAudio, PluginEditorHost, PluginInstance, PluginMeta, PluginParams, PluginState,
};
/// The lean, format-agnostic error the trait returns, plus its `Result` alias
/// and the shared `ParameterInfo` builders — re-exported so the loaders reach
/// them through the same `tutti_plugin::server::*` import point.
pub use tutti_plugin_types::{PluginError, PluginResult};
