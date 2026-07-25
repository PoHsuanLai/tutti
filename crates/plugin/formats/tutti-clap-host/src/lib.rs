//! Safe, ergonomic API for hosting CLAP audio plugins.
//!
//! `clap-host` wraps [CLAP](https://cleveraudio.org/) FFI so you can load
//! plugins, drive audio/MIDI processing, and respond to host-side callbacks
//! without writing `unsafe` yourself. MIDI uses the workspace-wide
//! [`tutti_midi_types::MidiEvent`] UMP type (re-exported as [`MidiEvent`]).
//!
//! ## Example
//!
//! ```ignore
//! use tutti_clap_host::{ClapLoaded, MidiEvent, ProcessContext, TransportInfo};
//!
//! // Load (GUI/params/state usable here), then activate to process audio.
//! let loaded = ClapLoaded::load("/path/to/plugin.clap", 44100.0, 512)?;
//! let mut active = loaded.activate::<f32>().map_err(|(_, e)| e)?;
//! let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);
//! active.process(&mut buffer, &ProcessContext {
//!     midi: &[MidiEvent::note_on(0, 0, 60, 16384)],
//!     transport: Some(&transport),
//!     ..Default::default()
//! })?;
//! ```

pub mod error;
pub mod events;
pub mod host;
pub mod instance;
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
pub use instance::{ClapActive, ClapLoaded, ClapSample, ProcessContext};
// `ParamMapping` (param-indication) is part of the speculative surface — gated.
#[cfg(feature = "clap-extras")]
pub use instance::ParamMapping;
#[cfg(all(unix, feature = "clap-extras"))]
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
    RemoteControlsPage, TrackInfo, TransportRequest, TriggerInfo, TuningInfo, UndoChange,
    UndoDeltaProperties,
};

// Test-only global allocator for RT-safety regression tests.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
