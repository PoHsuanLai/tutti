//! Shared value vocabulary for tutti's plugin host crates.
//!
//! Each format-specific host crate (`tutti-vst2-host`, `tutti-vst3-host`,
//! `tutti-clap-host`, `tutti-au-host`) re-exports these types from its own
//! public API so callers can stay format-agnostic when only the shared
//! surface is in play.

pub mod audio;
pub mod automation;
pub mod editor;
pub mod load_stage;
pub mod main_thread;
pub mod metadata;
pub mod parameters;
pub mod transport;

pub use audio::{AudioBuffer, AudioBuffer32, AudioBuffer64, BufferPtrs, Sample};
pub use main_thread::{assert_main_thread, mark_main_thread};
pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use editor::{
    AspectRatio, EditorCapabilities, EditorError, EditorSize, ResizeHints, WindowHandle,
};
pub use load_stage::LoadStage;
pub use metadata::{AudioIO, BusDirection, BusLayout, PluginInfo};
pub use parameters::{ParameterFlags, ParameterInfo};
pub use transport::{
    BarInfo, LoopRegion, MusicalTiming, TransportInfo, TransportPosition, TransportState,
};

/// Re-export of the workspace-wide MIDI event so host crates don't all
/// need to add a direct `tutti-midi-types` dependency just for the type.
pub use tutti_midi_types::MidiEvent;
