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
pub use events::{ClapEvent, EventList, InputEventList, OutputEventList, ParamBounds};
pub use host::{ClapHost, HostState, InputStream, OutputStream};
pub use instance::{
    ClapController, ClapEditor, ClapLoaded, ClapProcessContext, ClapProcessor, LoadOptions,
    descriptor_of,
};
// The CLAP-native, voice-addressed note expression is re-exported under its
// own distinct name (it does NOT shadow the shared
// `tutti_plugin_types::NoteExpressionValue`). The native parameter types
// (`ClapParamInfo` / `ClapParamFlags`) stay crate-private: the boundary speaks
// the shared `ParameterInfo` via `ClapLoaded::parameter_list`.
pub use types::ClapNoteExpression;
pub use types::{
    AmbisonicConfig, AmbisonicNormalization, AmbisonicOrdering, AudioBuffer, AudioBuffer32,
    AudioBuffer64, AudioPortConfig, AudioPortFlags, AudioPortInfo, ChannelLayout, Color,
    ContentScale, EditorCapabilities, EditorSize, NoteDialect, NoteDialects, NoteExpressionType,
    NoteName, NotePortInfo, ParamAutomationState, ParameterChanges, ParameterPoint, ParameterQueue,
    PluginInfo, StateContext, SurroundChannel, TransportInfo, VoiceInfo, WindowHandle, WireMidi,
};

// Test-only global allocator for RT-safety regression tests.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
