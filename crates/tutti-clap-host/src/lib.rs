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
//! use tutti_clap_host::{ClapInstance, MidiEvent, ProcessContext, TransportInfo};
//!
//! let mut plugin = ClapInstance::load("/path/to/plugin.clap", 44100.0, 512)?;
//! let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);
//! plugin.process(&mut buffer, &ProcessContext {
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
pub use instance::{ClapInstance, ClapSample, ParamMapping, ProcessContext};
#[cfg(unix)]
pub use types::PosixFdFlags;
pub use types::{
    AmbisonicConfig, AmbisonicNormalization, AmbisonicOrdering, AudioBuffer, AudioBuffer32,
    AudioBuffer64, AudioPortConfig, AudioPortConfigRequest, AudioPortFlags, AudioPortInfo,
    AudioPortType, Color, ContextMenuItem, ContextMenuTarget, EditorCapabilities, EditorSize,
    MidiEvent, NoteDialect,
    NoteDialects, NoteExpressionType, NoteExpressionValue, NoteName, NotePortInfo,
    ParamAutomationState, ParameterChanges, ParameterFlags, ParameterInfo, ParameterPoint,
    ParameterQueue, PluginInfo, RemoteControlsPage, StateContext, SurroundChannel, TrackInfo,
    TransportInfo, TransportRequest, TriggerInfo, TuningInfo, UndoChange, UndoDeltaProperties,
    VoiceInfo, WindowHandle,
};

// Test-only global allocator for RT-safety regression tests.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
