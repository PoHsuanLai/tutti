//! VST2 plugin hosting.
//!
//! Loads VST2 plugins (`.vst`, `.dll`, `.so`), drives audio + MIDI
//! processing, exposes parameters and state save/restore, and embeds
//! the plugin's native editor into a host-supplied window. Mirrors the
//! architecture of the workspace's `vst3-host`, `clap-host`, and
//! `au-host` sibling crates.
//!
//! Built on top of the [`vst`](https://docs.rs/vst) crate, which handles
//! the AEffect-level FFI. This crate adds the pieces a host actually
//! needs: pre-allocated render scratch buffers, MIDI codec, `audioMaster`
//! callback wiring, transport-info bookkeeping, state save/restore, and
//! editor lifecycle.
//!
//! # Architectural note
//!
//! Unlike VST3/CLAP/AU, VST2 fuses the editor and audio processor into a
//! single `AEffect` instance — you cannot host the editor in one process
//! and audio in another against the same plugin. Callers must accept
//! in-process hosting. Subprocess isolation, where useful, is the caller's
//! responsibility (e.g., the `tutti-plugin-server` subprocess wraps this
//! crate to keep a crashing VST2 from killing the host).
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use tutti_vst2_host::{ProcessContext, RenderScratch, Vst2Instance};
//!
//! let mut plugin = Vst2Instance::load(
//!     Path::new("/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst"),
//!     48_000.0,
//!     512,
//! )?;
//!
//! let meta = plugin.metadata().clone();
//! let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);
//!
//! let inputs: Vec<Vec<f32>> = (0..meta.num_inputs).map(|_| vec![0.0; 512]).collect();
//! let mut outputs: Vec<Vec<f32>> = (0..meta.num_outputs).map(|_| vec![0.0; 512]).collect();
//!
//! let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
//! let mut out_refs: Vec<&mut [f32]> =
//!     outputs.iter_mut().map(|v| v.as_mut_slice()).collect();
//!
//! let ctx = ProcessContext::new(48_000.0);
//! let _midi_out: &tutti_vst2_host::MidiEventVec =
//!     plugin.process_f32(&in_refs, &mut out_refs, 512, &ctx, &mut scratch);
//! # Ok::<(), tutti_vst2_host::Vst2Error>(())
//! ```

pub mod error;
pub mod types;

mod editor;
mod handle;
mod host;
mod instance;
mod midi;
mod parameters;
mod process;
mod scratch;
mod state;
mod time_info;

pub use error::{LoadStage, Result, Vst2Error};
pub use host::ParameterChange;
pub use instance::Vst2Instance;
pub use scratch::RenderScratch;
pub use types::{
    EditorSize, MidiEvent, MidiEventVec, ParameterInfo, PluginInfo, ProcessContext, TransportInfo,
    Vst2Category, WindowHandle,
};

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)`
// scopes. Matches the wiring used by clap-host and vst3-host.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
