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
//! # The lifecycle, and why one type carries all of it
//!
//! Every plugin format has the same underlying shape — a plugin is *loaded*
//! (library mapped, instance created, parameters and editor reachable) and then
//! *activated* (buffers allocated at a fixed rate and block size, processing
//! legal). VST3 and CLAP model that as two types with ownership-consuming
//! transitions; AU carries the stage in an internal enum. VST2 has neither:
//! [`Vst2Instance`] is one fused type, and that is the format's own contract
//! showing through rather than a shortcut.
//!
//! `effOpen`, `effSetSampleRate`, `effSetBlockSize` and the first
//! `effMainsChanged(1)` all run inside [`Vst2Instance::load`], so the instance
//! is ready to process the moment it exists. A `Vst2Loaded` type would sit over
//! a window no caller can observe and would carry no operations the fused type
//! does not — the split would buy a type boundary that guards nothing.
//!
//! The general rule, of which this crate is the negative case: **a type-state
//! split earns its keep only when the earlier state has genuinely distinct
//! operations.** VST3 and CLAP clear that bar — their parameter trees, program
//! lists and state save/restore are legal before any buffer exists, and "loaded
//! but not processing" is a state a host spends real time in, so a stale handle
//! to a deactivated plugin is worth making unrepresentable. VST2's pre-resume
//! window has no such surface: it exists only for the length of a constructor.
//! Splitting it would name a distinction the format does not make.
//!
//! ## Suspend and resume are a bracket, not a stage
//!
//! [`suspend`](Vst2Instance::suspend) and [`resume`](Vst2Instance::resume) are
//! ordinary `&mut self` methods over a `resumed` flag, and the flag is not a
//! demoted lifecycle stage. In VST2 the suspended state is a *reconfiguration
//! bracket* — the thing a host does around a sample-rate or block-size change,
//! because plugins reallocate rate-dependent buffers in `effMainsChanged` and
//! assume they are not concurrently processing. It is not a state a host parks
//! in and does other work from; there is no VST2 operation that is legal only
//! while suspended and interesting to a host, which is exactly what a stage
//! would need in order to be worth naming.
//!
//! That is why the bracket is spelled as a private
//! `suspend_for_reconfigure` / `restore_after_reconfigure` pair rather than as
//! two public transitions, and why it is **edge-triggered**: the suspend half
//! reports whether it actually dispatched, and the restore half takes that
//! answer, so a plugin already suspended on entry stays suspended on exit.
//! [`set_sample_rate`](Vst2Instance::set_sample_rate),
//! [`set_block_size`](Vst2Instance::set_block_size) and
//! [`reset_processing_state`](Vst2Instance::reset_processing_state) all share
//! it. An unconditional `suspend(); set(); resume()` would instead resume a
//! plugin the caller had deliberately stopped, and — since `effMainsChanged` is
//! not documented as idempotent — churn a full buffer teardown and reallocation
//! on every setter call.
//!
//! The comparative treatment across all four formats, and the rule for when
//! each modelling strategy applies, is in `tutti-plugin`'s crate docs under
//! *The plugin state machine*.
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
//! let inputs: Vec<Vec<f32>> =
//!     (0..meta.num_inputs.count()).map(|_| vec![0.0; 512]).collect();
//! let mut outputs: Vec<Vec<f32>> =
//!     (0..meta.num_outputs.count()).map(|_| vec![0.0; 512]).collect();
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
//!
//! # Parameters address positionally — VST2 alone among the four
//!
//! `getParameter(effect, index)` and `numParams` are both `i32` in the ABI, and
//! consecutive parameters really are consecutive. That is the whole reason
//! [`tutti_plugin_types::ParamAddress`] has two arms: the other three formats
//! hand out an opaque, plugin-chosen handle on which arithmetic means nothing.
//!
//! ```no_run
//! # use std::path::Path;
//! # use tutti_plugin_types::ParamAddress;
//! # use tutti_vst2_host::Vst2Instance;
//! # fn ex() -> tutti_vst2_host::Result<()> {
//! let plugin = Vst2Instance::load(Path::new("/usr/lib/vst/MyPlugin.so"), 48_000.0, 512)?;
//!
//! for info in plugin.parameter_list() {
//!     // Always the `Index` arm here. A VST2 host may iterate positions; a
//!     // VST3/CLAP/AU host may not, which is what the enum keeps apart.
//!     let ParamAddress::Index(index) = info.id else {
//!         unreachable!("VST2 addresses parameters positionally")
//!     };
//!     println!("{index}: {} ({:?})", info.qualified_name(), info.bounds());
//! }
//!
//! // The write takes the raw `i32` position and a normalized `0..=1` value —
//! // a C ABI, which is where the engine's unit newtypes stop.
//! plugin.set_parameter(0, 0.5);
//! # Ok(()) }
//! ```

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
    ChannelLayout, EditorSize, MidiEvent, MidiEventVec, ParameterInfo, PluginInfo, ProcessContext,
    Samples, TimeSignature, TransportInfo, Vst2Category, WindowHandle,
};

// Test-only global allocator for RT-safety regression tests. Panics on
// any heap allocation inside `assert_no_alloc::assert_no_alloc(..)`
// scopes. Matches the wiring used by clap-host and vst3-host.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
