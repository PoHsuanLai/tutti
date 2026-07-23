//! Host↔server wire contract.
//!
//! Not part of the end-user API — these types exist so `tutti-plugin-server`
//! can build the other end of the IPC. Regular library users should not
//! reach into this module.
//!
//! Contains the [`PluginInstance`](crate::server::PluginInstance) trait
//! (what a loader must implement) and re-exports every wire-frame type the
//! IPC carries.
//!
//! The trait itself and its per-block process types now live in
//! `tutti-plugin-types` as [`PluginFormatHost`](tutti_plugin_types::PluginFormatHost)
//! (so any crate can implement it without depending on `tutti-plugin`);
//! `PluginInstance` here is a re-export alias kept for the existing call sites.

pub use crate::host::subprocess::resolve_bundle;
pub use crate::protocol::audio::{
    AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, Sample,
};

pub use crate::protocol::{
    AuComponentType, BridgeMessage, BusChannels, ChordChanges, ChordValue, Features, HostMessage,
    IpcMidiEvent, IpcMidiEventVec, LoadedPlugin, MidiEvent, MidiEventVec, NoteExpressionChanges,
    NoteExpressionIntChanges, NoteExpressionIntValue, NoteExpressionTextChanges,
    NoteExpressionTextValue, NoteExpressionType, NoteExpressionValue, ParameterChanges,
    ParameterFlags, ParameterInfo, ParameterPoint, ParameterQueue, PluginClass, PluginDescriptor,
    ProcessAudioData, SampleFormat, ScaleChanges, ScaleValue, SlabLayout, TransportInfo,
    Vst2Category, MIDI_STACK_CAPACITY, PROTOCOL_VERSION,
};
pub use crate::util::config::BridgeConfig;
pub use crate::util::transport::shm::AudioSlab;
pub use crate::util::window::{EditorSize, WindowHandle};
/// The unified plugin-format host trait, re-exported from `tutti-plugin-types`
/// under its historical `PluginInstance` name so existing `impl PluginInstance`
/// / `dyn PluginInstance` / `Box<dyn PluginInstance>` sites keep resolving.
pub use tutti_plugin_types::PluginFormatHost as PluginInstance;
/// The lean, format-agnostic error the trait returns, plus its `Result` alias
/// and the shared `ParameterInfo` builders — re-exported so the loaders reach
/// them through the same `tutti_plugin::server::*` import point.
pub use tutti_plugin_types::{make_param_info, PluginError, PluginResult, ALL_AUTOMATABLE};
/// Per-block process inputs/outputs, re-exported from `tutti-plugin-types`.
pub use tutti_plugin_types::{ExpressiveContext, ProcessContext, ProcessOutput};
