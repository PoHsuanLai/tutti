#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` the CLAP host needs is in `tutti-clap-ffi`.
#![forbid(unsafe_code)]

mod error;
mod events;
pub mod host;
pub mod instance;
pub mod topology;
pub mod types;

pub use error::{ClapError, LoadStage, Result};
pub use events::{ClapEvent, EventList, InputEventList, OutputEventList};
pub use host::{ClapHost, HostState, InputStream, OutputStream};
pub use instance::{
    ClapController, ClapEditor, ClapLoaded, ClapProcessContext, ClapProcessor, LoadOptions,
    descriptor_of,
};
// `ParamMapping` (param-indication) is part of the speculative surface — gated.
#[cfg(feature = "clap-extras")]
pub use instance::ParamMapping;
#[cfg(all(unix, feature = "clap-extras"))]
// CLAP channel map <-> the shared ChannelTopology. Flat-re-exported so a
// caller converting a layout does not have to name the module.
pub use topology::topology_of;

// `cfg`-gated to match the item: `PosixFdFlags` is `#[cfg(unix)]` (it models
// `CLAP_EXT_POSIX_FD_SUPPORT`, which has no Windows counterpart), so an
// unconditional re-export is an unresolved import on Windows and nothing at
// all on Unix. Same shape as the `tutti-au-host` topology re-export.
#[cfg(unix)]
pub use types::PosixFdFlags;
// The CLAP-native, voice-addressed note expression is re-exported under its
// own distinct name (it does NOT shadow the shared
// `tutti_plugin_types::NoteExpressionValue`). The native parameter types
// (`ClapParamInfo` / `ClapParamFlags`) stay crate-private: the boundary speaks
// the shared `ParameterInfo` via `ClapLoaded::parameter_list`.
pub use types::ClapNoteExpression;
// Speculative types whose only accessors sit behind `clap-extras`
// (param-indication, remote-controls, context-menus, triggers, tuning, undo,
// track-info, audio-port reconfiguration, transport-control). Their
// definitions stay compiled (some are referenced by always-on host callbacks),
// but the public re-export is gated so the default API surface stays lean.
pub use types::{
    AmbisonicConfig, AmbisonicNormalization, AmbisonicOrdering, AudioBuffer, AudioBuffer32,
    AudioBuffer64, AudioPortConfig, AudioPortFlags, AudioPortInfo, ChannelLayout, ContentScale,
    EditorCapabilities, EditorSize, NoteDialect, NoteDialects, NoteExpressionType, NoteName,
    NotePortInfo, ParameterChanges, ParameterPoint, ParameterQueue, PluginInfo, StateContext,
    SurroundChannel, TransportInfo, VoiceInfo, WindowHandle, WireMidi,
};
#[cfg(feature = "clap-extras")]
pub use types::{
    AudioPortConfigRequest, Color, ContextMenuItem, ContextMenuTarget, ParamAutomationState,
    RemoteControlsPage, TrackAudio, TrackInfo, TrackPortType, TransportRequest, TriggerInfo,
    TuningInfo, UndoChange, UndoDeltaProperties,
};

// Test-only global allocator for RT-safety regression tests.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
