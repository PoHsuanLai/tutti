//! Safe, ergonomic API for hosting CLAP audio plugins.
//!
//! `clap-host` wraps [CLAP](https://cleveraudio.org/) FFI so you can load
//! plugins, drive audio/MIDI processing, and respond to host-side callbacks
//! without writing `unsafe` yourself. MIDI uses the workspace-wide
//! [`tutti_midi_types::MidiEvent`] UMP type (re-exported as [`MidiEvent`]).
//!
//! # The lifecycle, and why it is shaped this way
//!
//! Hosting CLAP is a two-state problem: a plugin is first *loaded* — library
//! mapped, instance created, parameters and editor reachable — and only later
//! *activated*, which allocates buffers against a fixed sample rate and block
//! size and makes `process` legal. This crate spends a type on each:
//! [`ClapLoaded`] and [`ClapActive<T>`], with transitions that take `self` by
//! value.
//!
//! Every plugin format has that same underlying shape and they agree on almost
//! nothing else, which is why the four format crates deliberately share no
//! lifecycle type — the comparative account, and the rule for choosing between
//! the three modelling strategies in use, is in `tutti-plugin`'s crate docs
//! under *The plugin state machine*. What follows is why CLAP lands on this one.
//!
//! ## Why two types rather than one with an `is_active` flag
//!
//! Because "loaded but not processing" is a state a host spends real time in,
//! not a transient on the way to processing. CLAP's legal pre-activation
//! surface is large and this crate exposes all of it on [`ClapLoaded`]: the
//! parameter tree ([`parameter_list`](ClapLoaded::parameter_list),
//! [`set_parameter`](ClapLoaded::set_parameter),
//! [`value_to_text`](ClapLoaded::value_to_text)), port and note-port topology,
//! state save/restore and preset loading, the editor, and the whole
//! host-callback polling surface. A plugin browser that opens a GUI to audition
//! presets, or a project load that restores state before the transport rolls,
//! is a `ClapLoaded` doing its entire job without a buffer ever being
//! allocated. A state that has its own operations and its own duration earns a
//! type.
//!
//! Making the transitions consume `self` is what turns the guarantee from a
//! convention into a compile-time fact. [`activate`](ClapLoaded::activate) takes
//! the `ClapLoaded` and [`deactivate`](ClapActive::deactivate) takes the
//! `ClapActive` back, so a handle to a deactivated plugin is unrepresentable
//! rather than merely discouraged: there is no `process` on [`ClapLoaded`] to
//! call, and no surviving `ClapActive` to call it on. The audio path therefore
//! carries no `is_active` check at all — the branch a fused type would have to
//! run on every block does not exist, because the type system already ran it.
//! The `T` parameter is fixed at the same moment for the same reason: CLAP
//! commits to a sample width in the call that allocates the buffers, so
//! `ClapActive<f32>` and `ClapActive<f64>` are different types rather than one
//! type with a width field.
//!
//! ## Why `activate` returns `Err((Self, ClapError))`
//!
//! A refusal is not the end of the plugin, only of that configuration. CLAP
//! plugins routinely decline 64-bit audio, and a host that discovers this the
//! ordinary way — by asking — should not have to pay for a reload to fall back.
//! So the error variant carries the **unconsumed** [`ClapLoaded`] back beside
//! the [`ClapError`], and the caller retries at `f32` against the same mapped
//! library, the same instance, and the same already-read parameter tree.
//!
//! This is the cost of the consuming transition, paid back. A `Result<_,
//! ClapError>` would have destroyed the `ClapLoaded` on the one path where it is
//! still perfectly good, making "ask, and fall back" strictly more expensive
//! than "guess from [`supports_f64`](ClapLoaded::supports_f64) and hope". Both
//! failure paths preserve it: the `f64`-unsupported check returns before any FFI
//! runs, and a plugin-side `activate` refusal leaves the instance untouched and
//! not active. The `Err` is large, which is why the `clippy::result_large_err`
//! lint is suppressed at the function rather than obeyed — boxing would add a
//! heap allocation on the failure path in order to hide the exact ownership
//! return that is the point.
//!
//! ## Why `Deref` from `ClapActive` to `ClapLoaded` is sound
//!
//! A naive type-state split would strand the entire pre-activation surface:
//! params, ports, state, editor and polling are all `impl ClapLoaded`, so
//! activating would take them away — and a host needs every one of them *most*
//! while audio is running. [`ClapActive<T>`] therefore embeds its `ClapLoaded`
//! and reaches those methods through [`Deref`](std::ops::Deref), adding only
//! [`process`](ClapActive::process) and the reconfiguration methods on top.
//! Activation is additive rather than exclusive.
//!
//! That is sound because CLAP does not *revoke* the loaded-state operations on
//! activation; it re-tags some of their **threading** contracts. The two states
//! partition what is legal only in one direction — `process` requires active —
//! and never in the other. Where a contract does change with activation, the
//! condition is read at the call rather than assumed from the type, which works
//! precisely because the flag lives on the *inner* `ClapLoaded`:
//! [`flush_params`](ClapLoaded::flush_params) is tagged
//! `[active ? audio-thread : main-thread]`, and it branches on that live flag,
//! so the same code reached through `Deref` from a `ClapActive` takes an
//! [`AudioThreadClaim`](host::AudioThreadClaim) where a bare `ClapLoaded` would
//! assert the main thread. A method inherited from the loaded state is thus
//! never operating on a stale idea of which state it is in.
//!
//! The distinction that gates it is *active* versus *processing*, and they are
//! not the same fact. They disagree for the whole window between
//! [`activate`](ClapLoaded::activate) and the first
//! [`process`](ClapActive::process) — which is exactly when a host pushes
//! initial parameter values. Reading `processing` there takes the main-thread
//! branch against a plugin that considers itself active, and a plugin with a
//! validation layer reports the host for calling on the wrong thread while the
//! values are dropped in silence.
//!
//! [`DerefMut`](std::ops::DerefMut) is implemented alongside it, so the
//! `&mut self` half of the loaded surface — [`set_state`](ClapLoaded::set_state),
//! [`open_editor`](ClapLoaded::open_editor) — is reachable too. What `Deref`
//! does not expose is the transition itself: [`deactivate`](ClapActive::deactivate)
//! consumes the `ClapActive`, and no `&mut ClapLoaded` borrowed out of one can
//! reach a method that would move the instance between states.
//!
//! ## Example
//!
//! Load, activate, render one block. `no_run`: the load needs a real `.clap`
//! bundle on disk.
//!
//! ```no_run
//! use tutti_midi_types::{MidiChannel, MidiGroup};
//! use tutti_clap_host::{AudioBuffer32, ClapLoaded, MidiEvent, ProcessContext, TransportInfo};
//!
//! // `ClapLoaded` is the GUI / parameter / state stage; sample rate and the
//! // max block length are fixed here, so `activate` takes no arguments.
//! let loaded = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", 48_000.0, 512)?;
//! println!("{} by {}", loaded.info().name, loaded.info().vendor);
//!
//! // `activate` hands `self` back on refusal — a plugin that declines f64 can
//! // be retried at f32 without reloading.
//! let mut active = loaded.activate::<f32>().map_err(|(_, e)| e)?;
//!
//! // 512 is the block length in FRAMES; each channel slice holds that many.
//! let silence = vec![0.0f32; 512];
//! let inputs: [&[f32]; 2] = [&silence, &silence];
//! let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
//! let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
//! let mut buffer = AudioBuffer32::new(&inputs, &mut outputs, 48_000.0);
//!
//! let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);
//! let midi = [MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 16384)];
//! active.process(&mut buffer, &ProcessContext {
//!     midi: &midi,
//!     transport: Some(&transport),
//!     ..Default::default()
//! })?;
//! # Ok::<(), tutti_clap_host::ClapError>(())
//! ```
//!
//! CLAP addresses parameters by an opaque, plugin-chosen `clap_id`;
//! [`ClapLoaded::parameter_list`] hands them back as the shared
//! `tutti_plugin_types::ParameterInfo`, so a consumer never learns which format
//! it is reading.

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
pub use instance::{ClapActive, ClapLoaded, ClapSample, ProcessContext};
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
