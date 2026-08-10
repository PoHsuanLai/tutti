//! Type-safe Rust library for hosting VST3 audio plugins via their native COM
//! interfaces.
//!
//! The public surface is organised around a three-stage lifecycle encoded in the
//! type system: a [`Vst3Library`] holds the loaded DSO and factory, a
//! [`Vst3Loaded`] is a `initialize()`'d plugin suitable for GUI/parameter work,
//! and a [`Vst3Instance`] adds the activation state required for
//! [`Vst3Instance::process`]. Transitions between stages move ownership, so the
//! compiler rejects calls that would be invalid for the current state.
//!
//! # Example
//!
//! Walk the three stages, then render one block. `no_run`: the load needs a
//! real `.vst3` bundle on disk.
//!
//! ```no_run
//! use std::path::Path;
//! use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
//! use tutti_vst3_host::{
//!     AudioBuffer, MidiEvent, TransportInfo, Vst3InputEvents, Vst3Loaded,
//! };
//!
//! // Stage 2: initialized. Parameters and the editor are reachable here, with
//! // no activation cost paid.
//! let loaded = Vst3Loaded::load(Path::new("/usr/lib/vst3/MyPlugin.vst3"))?;
//! println!("{} by {}", loaded.info().name, loaded.info().vendor);
//!
//! // Stage 3: activated. `T` fixes the sample width — `f64` errors out unless
//! // the plugin advertises 64-bit support.
//! let mut plugin = loaded.activate::<f32>(48_000.0, 512)?;
//!
//! // 512 is the block length in FRAMES; each channel slice holds that many.
//! let silence = vec![0.0f32; 512];
//! let inputs: [&[f32]; 2] = [&silence, &silence];
//! let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
//! let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
//! let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 48_000.0);
//!
//! // The sample rate stays a raw `f64` here: this is a C ABI, which is where
//! // the engine's unit newtypes deliberately stop.
//! let midi = [MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xC000)];
//! let events = Vst3InputEvents {
//!     midi: &midi,
//!     ..Default::default()
//! };
//! let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);
//!
//! let out = plugin.process(&mut buffer, &events, None, &transport);
//! println!("plugin emitted {} MIDI events", out.midi_events.len());
//! # Ok::<(), tutti_vst3_host::Vst3Error>(())
//! ```
//!
//! VST3 addresses parameters by an opaque, plugin-chosen `ParamID` — see
//! `tutti_plugin_types::ParamAddress`, whose other arm exists for VST2's
//! positional index.

pub(crate) mod com;
pub mod error;
pub(crate) mod helpers;
pub mod host;
pub mod types;

pub use error::{LoadStage, Result, Vst3Error};
pub use host::{
    factory_flags, FactoryInfo, PluginNotifications, RestartOutcome, Vst3Instance, Vst3Library,
    Vst3Loaded,
};
pub use types::{
    automation_state, keyswitch_type, note_expression_flags, parameter_flags, physical_ui_type,
    prefetchable_support, process_context_flags, to_process_context, unit_ids, vst3_to_chord,
    vst3_to_note_expression, vst3_to_note_expression_int, vst3_to_note_expression_text,
    vst3_to_scale, AudioBuffer, BufferPtrs, BusInfo, ChordValue, EditorCapabilities, EditorSize,
    MidiEvent, NoteExpressionIntValue, NoteExpressionText, NoteExpressionType, NoteExpressionValue,
    ParameterChanges, ParameterPoint, ParameterQueue, PluginInfo, ProcessMode, ProcessOutput,
    ProcessOutputRef, Sample, ScaleValue, TransportInfo, Vst3InputEvents, Vst3KeyswitchInfo,
    Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3ProgramListInfo, Vst3Sample, Vst3UnitInfo,
    WindowHandle,
};

pub use com::{ParameterEditEvent, ProgressEvent, RestartFlags, UnitEvent};

#[cfg(feature = "conformance")]
pub use com::RunLoopActivity;

// No `///` here: a doc comment on a `pub mod` re-resolves its intra-doc links in
// this parent scope, where `Vst3Event`'s methods are not in scope. The module's
// own `//!` below is the correct home for both the prose and the links.
pub mod events {
    //! Tagged-enum wrappers over VST3's typed event structs, plus the event-type
    //! discriminant constants.
    //!
    //! Re-exported here so downstream crates that need to construct
    //! `NoteOnEvent`/`NoteOffEvent`/etc. literally (rather than going through the
    //! [`Vst3Event::to_midi`] / [`Vst3Event::from_midi`] helpers) can do so
    //! without depending on the raw `vst3` crate.

    pub use crate::types::{
        ChordEvent, DataEvent, EventHeader, LegacyMidiCcOutEvent, NoteExpressionIntValueEvent,
        NoteExpressionTextEvent, NoteExpressionValueEvent, NoteOffEvent, NoteOnEvent,
        PolyPressureEvent, ScaleEvent, TextRef, Vst3Event, K_CHORD_EVENT, K_DATA_EVENT,
        K_LEGACY_MIDI_CC_OUT_EVENT, K_NOTE_EXPRESSION_INT_VALUE_EVENT,
        K_NOTE_EXPRESSION_TEXT_EVENT, K_NOTE_EXPRESSION_VALUE_EVENT, K_NOTE_OFF_EVENT,
        K_NOTE_ON_EVENT, K_POLY_PRESSURE_EVENT, K_SCALE_EVENT,
    };
}

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
