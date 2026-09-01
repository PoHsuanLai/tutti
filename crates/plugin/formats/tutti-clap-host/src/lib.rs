#![doc = include_str!("../README.md")]

mod error;
mod events;
pub mod host;
pub mod instance;
pub mod topology;
pub mod types;

/// Copy a nul-terminated C string into an owned `String`, substituting lossy
/// replacement for invalid UTF-8. Returns an empty string if `ptr` is null.
///
/// # Safety
/// `ptr` must be null or point to a valid, nul-terminated C string.
pub(crate) unsafe fn cstr_to_string(ptr: *const std::ffi::c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

pub use error::{ClapError, LoadStage, Result};
pub use events::{ClapEvent, EventList, InputEventList, OutputEventList};
pub use host::{ClapHost, HostState, InputStream, OutputStream};
pub use instance::{ClapActive, ClapLoaded, ClapSample, ClapProcessContext};
// `ParamMapping` (param-indication) is part of the speculative surface — gated.
#[cfg(feature = "clap-extras")]
pub use instance::ParamMapping;
#[cfg(all(unix, feature = "clap-extras"))]
// CLAP channel map <-> the shared ChannelTopology. Flat-re-exported so a
// caller converting a layout does not have to name the module.
pub use topology::{channel_map_of, topology_of};

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
    AudioBuffer64, AudioPortConfig, AudioPortFlags, AudioPortInfo, ChannelLayout,
    EditorCapabilities, EditorSize, MidiEvent, NoteDialect, NoteDialects, NoteExpressionType,
    NoteName, NotePortInfo, ParameterChanges, ParameterPoint, ParameterQueue, PluginInfo,
    StateContext, SurroundChannel, TransportInfo, VoiceInfo, WindowHandle,
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
