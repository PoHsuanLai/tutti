#![doc = include_str!("../README.md")]

// The VST2 FFI layer is the vendored fork of vst-rs (`vst-tutti`), which adds
// the host-side audioMaster callbacks upstream swallows. Aliased to `vst` at the
// crate root so every `vst::` path in the submodules resolves unchanged.
extern crate vst_tutti as vst;

mod error;
pub mod types;

mod editor;
mod handle;
mod host;
mod instance;
mod midi;
mod param_properties;
mod parameters;
mod process;
mod scratch;
mod state;
mod time_info;
mod transport_cell;

pub use error::{LoadStage, Result, Vst2Error};
pub use host::ParameterChange;
pub use instance::Vst2Instance;
// `effGetParameterProperties` + the MIDI-metadata family. VST2 has no
// CC→parameter mapping query at all — see the module docs for the opcode
// evidence — so this is the whole of its parameter/MIDI metadata surface.
pub use param_properties::{
    FloatSteps, IntegerRange, MidiKeyName, MidiProgram, MidiProgramCategory, ParameterCategory,
    ParameterProperties, ParameterPropertyFlags, MAX_MIDI_KEY, NUM_MIDI_CHANNELS,
};
pub use scratch::RenderScratch;
pub use types::{
    ChannelLayout, EditorSize, MidiEvent, MidiEventVec, ParamAddress, ParameterInfo, PluginInfo,
    Vst2ProcessContext, Samples, TimeSignature, TransportInfo, Vst2Category, WindowHandle,
};

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)`
// scopes. Matches the wiring used by clap-host and vst3-host.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
