#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// The VST2 FFI layer is `tutti-vst2-ffi`, the host half of our fork of vst-rs, which holds
// every VST2 `unsafe`. Aliased to `vst` at the crate root so every `vst::` path in the
// submodules resolves unchanged.
extern crate tutti_vst2_ffi as vst;

use vst::{midi, scratch};

mod error;
pub mod types;

mod control;
mod controller;
mod editor;
mod host;
mod hosting;
mod loaded;
mod param_properties;
mod parameters;
mod processor;
mod state;
mod time_info;
mod transport_cell;

pub use controller::Vst2Controller;
pub use error::{LoadStage, Result, Vst2Error};
pub use host::ParameterChange;
pub use loaded::Vst2Loaded;
pub use processor::Vst2Processor;
// `effGetParameterProperties` + the MIDI-metadata family. VST2 has no
// CC→parameter mapping query at all — see the module docs for the opcode
// evidence — so this is the whole of its parameter/MIDI metadata surface.
pub use param_properties::{
    FloatSteps, IntegerRange, IntegerSteps, MAX_MIDI_KEY, MidiKeyName, MidiProgram,
    MidiProgramCategory, NUM_MIDI_CHANNELS, ParameterCategory, ParameterProperties,
    ParameterPropertyFlags,
};
pub use types::{
    ChannelLayout, EditorSize, MidiEventVec, ParameterInfo, PluginInfo, Samples, TimeSignature,
    TransportInfo, Vst2Category, Vst2ProcessContext, WindowHandle, WireMidi,
};

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)`
// scopes. Matches the wiring used by clap-host and vst3-host.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
