#![doc = include_str!("../README.md")]
//! The shared value vocabulary underneath Tutti's four plugin-host crates.
//!
//! ## What this is
//!
//! Everything a VST2 / VST3 / CLAP / AU host needs to speak that is *not* specific
//! to one format: [`PluginDescriptor`] and [`LoadedPlugin`] (identity and the
//! load-time wiring snapshot), [`ParameterInfo`] with its [`ParamId`] /
//! [`ParamRange`] / [`ParamFlags`] parts, [`ProcessContext`] and [`ProcessOutput`],
//! [`TransportInfo`], [`ParameterChanges`] automation queues, note expression and
//! harmony changes, [`AudioBuffer`] in its f32/f64 forms, editor vocabulary
//! ([`WindowHandle`], [`EditorUnit`], [`EditorCapabilities`]), presets, and the
//! [`Features`] capability bitset.
//!
//! It also carries the [`bundle`] helpers that find the loadable module inside a
//! `.vst3` / `.vst` bundle.
//!
//! ## What it does not own
//!
//! **Any format's lifecycle.** The traits that carry a plugin's transitions and
//! control surface (`Loaded`, `Processor`, `Controller`, `Control`) are the
//! vocabulary; each format crate decides what its own lifecycle is as tightly as its
//! own contract allows, and the comparative account is in `tutti-plugin`'s crate
//! docs under *The plugin state machine*.
//!
//! **Any FFI.** Nothing here links a plugin SDK. The format host crates
//! (`tutti-vst2-host`, `tutti-vst3-host`, `tutti-clap-host`, `tutti-au-host`) do
//! that, and each re-exports these types from its own public API so a caller can
//! stay format-agnostic whenever only the shared surface is in play.
//!
//! ## Why it is its own crate
//!
//! **It sits under all four format hosts, and each re-exports it.** Without a common
//! home, the same `TransportInfo` would exist four times and a host would translate
//! between four dialects of it.
//!
//! The shape here is designed to keep formats' disagreements **representable rather
//! than averaged away**: [`ParamRange`] is a sum type because there is no safe number
//! to substitute when a format declares no range; [`ParamSteps`] separates
//! "continuous" from "nobody said", which a bare `step_count: u32` fuses at zero;
//! [`ParamFlags`] is paired with a `known` mask so a flag nobody reported reads as
//! `None` rather than `false`.
//!
//! Representable is not the same as unavoidable, though. A consumer should read a
//! parameter through [`ParameterInfo`]'s own accessors ([`bounds`], [`default_value`],
//! [`step_count`], [`flag`]) — the variants exist for the host that
//! must produce them, not as the reading path. A range is data for display: the
//! plain value of a normalized one is the format's answer
//! ([`AnyThread::parameter_plain`]: each format's own [`ParamMapping`], held to its
//! laws by `contract::mapping_contract!` under the `contract` feature), never a
//! formula over `bounds`.
//!
//! ## Example
//!
//! Two format hosts describe one parameter each. The shapes differ where the ABIs
//! differ, and the reading path is the same for both:
//!
//! ```rust
//! use tutti_plugin_types::{ParamFlags, ParamId, ParamSteps, ParameterInfo};
//!
//! // VST3/CLAP/AU: a plugin-chosen handle, often a hash of the name. Not an
//! // index, not dense, not ordered.
//! let cutoff = ParameterInfo::new(ParamId::new(0x8000_0001), "Cutoff")
//!     .with_plain_range(20.0, 20_000.0, 440.0)
//!     .with_steps(ParamSteps::Continuous)
//!     .with_flags(ParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE);
//!
//! // VST2: the id is the parameter's position in `[0, numParams)`, and a
//! // plugin that declined `effGetParameterProperties` declared no range at all.
//! let mix = ParameterInfo::new(ParamId::new(3), "Mix")
//!     .with_normalized_default(0.5);
//!
//! // An absent declaration is `None`, never a substituted number.
//! assert_eq!(cutoff.bounds(), Some((20.0, 20_000.0)));
//! assert_eq!(mix.bounds(), None);
//! assert_eq!(mix.step_count(), None); // `Unknown`, not "continuous"
//! assert_eq!(mix.flag(ParamFlags::AUTOMATABLE), None); // unreported, not `false`
//!
//! // The id is the plugin's own number, and the type has no arithmetic: a host
//! // hands it back, it does not count with it.
//! assert_eq!(cutoff.id(), ParamId::new(0x8000_0001));
//! assert_eq!(mix.id().get(), 3);
//!
//! // The seam speaks normalized `0..=1`; what a value is in the plugin's own unit
//! // is asked of the format (`AnyThread::parameter_plain`), not computed here.
//! assert_eq!(cutoff.default_value(), 440.0);
//! ```
//!
//! ## `Normalized::clamped` is silent; `Normalized::new` refuses
//!
//! `Normalized::new` returns `None` for anything off `0..=1`, NaN included, and so
//! does deserializing one. `Normalized::clamped` saturates out-of-range input at the
//! nearest bound and maps NaN to `0.0`, returning no error and logging nothing.
//! Encode ordering data — or any plain value — through it and every entry above
//! `1.0` becomes `1.0`, which reads downstream as a legitimate sweep pinned at
//! maximum rather than as bad input. Normalize before constructing:
//!
//! ```rust
//! use tutti_plugin_types::Normalized;
//!
//! // A plain 20 kHz cutoff, handed to the normalized seam.
//! assert_eq!(Normalized::clamped(20_000.0).get(), 1.0);
//!
//! // Three distinct positions collapse onto one value, in silence.
//! let ranks: Vec<f64> = [7.0, 42.0, 79.0]
//!     .into_iter()
//!     .map(|r| Normalized::clamped(r).get())
//!     .collect();
//! assert_eq!(ranks, [1.0, 1.0, 1.0]);
//!
//! // Divide by the span first, and the positions survive.
//! let span = 79.0;
//! let ranks: Vec<f64> = [7.0, 42.0, 79.0]
//!     .into_iter()
//!     .map(|r| Normalized::clamped(r / span).get())
//!     .collect();
//! assert!(ranks[0] < ranks[1] && ranks[1] < ranks[2]);
//! ```
//!
//! ## Where it sits
//!
//! Depends on `tutti-types` ([`Samples`], [`ChannelLayout`], [`ChannelTopology`], the
//! meter vocabulary), `tutti-graph` (`Offset`), `tutti-rt`, `tutti-ump` and
//! `tutti-midi-protocol`, re-exporting the parts format crates need so none of them takes its
//! own dependency for the type names.
//! All four format hosts and `tutti-plugin` depend on it.
//!
//! ## Features
//!
//! `default = []`.
//!
//! - `serde` — `Serialize`/`Deserialize` on the wire-eligible types
//!   ([`TransportInfo`], [`ParameterChanges`],
//!   [`EditorCapabilities`], [`WindowHandle`], and the metadata that rides the
//!   plugin-load reply). Required by the plugin IPC protocol (postcard); a host
//!   crate that does no IPC does not pay for the dependency.
//! - `contract` — the `contract` module: `mapping_contract!`, the laws a
//!   [`ParamMapping`] is held to. Test support, off by default; a format host turns
//!   it on in its dev-dependency.
//!
//! [`PluginDescriptor`]: crate::PluginDescriptor
//! [`LoadedPlugin`]: crate::LoadedPlugin
//! [`ParameterInfo`]: crate::ParameterInfo
//! [`ParamId`]: crate::ParamId
//! [`ParamRange`]: crate::ParamRange
//! [`ParamFlags`]: crate::ParamFlags
//! [`ParamSteps`]: crate::ParamSteps
//! [`ProcessContext`]: crate::ProcessContext
//! [`ProcessOutput`]: crate::ProcessOutput
//! [`TransportInfo`]: crate::TransportInfo
//! [`ParameterChanges`]: crate::ParameterChanges
//! [`AudioBuffer`]: crate::AudioBuffer
//! [`WindowHandle`]: crate::WindowHandle
//! [`EditorUnit`]: crate::EditorUnit
//! [`EditorCapabilities`]: crate::EditorCapabilities
//! [`Features`]: crate::Features
//! [`bundle`]: crate::bundle
//! [`Samples`]: crate::Samples
//! [`ChannelLayout`]: crate::ChannelLayout
//! [`ChannelTopology`]: crate::ChannelTopology
//! [`bounds`]: crate::ParameterInfo::bounds
//! [`default_value`]: crate::ParameterInfo::default_value
//! [`step_count`]: crate::ParameterInfo::step_count
//! [`flag`]: crate::ParameterInfo::flag
//! [`AnyThread::parameter_plain`]: crate::AnyThread::parameter_plain
//! [`ParamMapping`]: crate::ParamMapping
#![cfg_attr(docsrs, feature(doc_cfg))]
// One `unsafe` item: `WindowHandle::from_u64`, the constructor whose caller
// vouches for a window's bare platform number.
#![deny(unsafe_code)]

mod any_thread;
mod automation;
mod automation_mode;
mod bridge_error;
pub mod bundle;
mod channels;
mod classification;
#[cfg(feature = "contract")]
pub mod contract;
mod descriptor;
mod driver;
mod editor;
mod error;
mod failure;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod features;
mod harmony;
mod host;
mod hosting;
mod invalidation;
mod lifecycle;
mod load_stage;
mod locate;
mod main_thread;
mod measured;
mod metadata;
mod midi;
mod midi_intake;
mod note_expression;
mod note_id;
mod order;
mod param_edit;
mod param_mapping;
mod param_unit;
mod parameters;
mod pipeline;
mod points;
mod presets;
#[cfg(feature = "probe-wire")]
mod probe_wire;
mod process;
mod process_wake;
mod render_mode;
mod requests;
mod sample_format;
mod state_use;
mod sysex;
mod track_info;
mod transport;
mod transport_request;
mod wakeups;

// Test-only global allocator for the no-alloc gates: panics on any heap
// allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

pub use tutti_rt::{RequestBit, RequestBits, RequestWord};
pub use tutti_types::{ChannelLayout, ChannelTopology, Speaker};
/// Re-exported so every format crate spells a sample count the same way
/// without each taking its own `tutti-types` dependency — `ChannelLayout`
/// above is here for the same reason.
pub use tutti_types::{MaxBlock, SampleRate, Samples};

mod layout_support;
pub use layout_support::LayoutSupport;
// Musical vocabulary carried on `TransportInfo`. Re-exported for the same reason
// as `ChannelLayout`: format hosts speak these at their ABI boundary and should
// not need a `tutti-types` dependency of their own to name them.
pub use tutti_types::{BarNumber, BeatsPerBar, NoteValue, Seconds, TimeSignature};

pub use any_thread::{AnyThread, Health};
pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use automation_mode::{AutomationMode, AutomationModeError};
pub use bridge_error::{BridgeError, BridgeResult, PluginForkError};
pub use channels::{
    AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, MAX_BLOCK_CHANNELS, Sample,
};
pub use classification::{ClapFeature, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories};
pub use descriptor::{AuComponentType, EditorPresence, PluginClass, PluginDescriptor};
pub use driver::{Hand, Lifecycle, Seat};
pub use editor::{
    AspectRatio, ContentScale, EditorCapabilities, EditorError, EditorUnit, InvalidContentScale,
    KeyEvent, KeyUse, ModifierConvention, ResizeHints, WindowHandle,
};
pub use failure::{PluginFailure, TerminationReason};
pub use host::{Control, Controller, Loaded, PluginIo, Processor, Refused, Stuck, Wraps};
pub use hosting::{Editor, EditorWindow, InProcess, Load, Origin, OutOfProcess, Reopen};
pub use invalidation::{PluginInvalidation, PluginRefresh, ResyncClass, ResyncKind};
pub use order::{StableOrder, insertion_sort_by_key};
pub use points::{PointBuf, PointPool};
pub use process_wake::ProcessWake;
pub use requests::{Edit, EditDrain, EditQueue, EditSink, edits, taken};
pub use state_use::StateUse;
pub use track_info::{TrackInfo, TrackKind};
pub use transport_request::TransportRequest;
// Exported as `PluginResult` only: the bare name `Result` shadows std's under a
// glob import.
pub use error::{Delivered, PluginError, PresetError, Result as PluginResult, StateError};
pub use features::{FeatureReport, Features};
pub use harmony::{
    ChordChanges, ChordValue, HARMONY_CAPACITY, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, ScaleChanges, ScaleValue,
};
pub use lifecycle::{
    EFFECT_CAPACITY, Effect, Event, FDS_PER_TICK, FdId, HostOp, MAX_STEP_EFFECTS, Notice, Prepare,
    Reply, Request, RescanScope, RestartRules, Stage, State, TIMERS_PER_TICK, TimerId, standing,
    step,
};
pub use load_stage::LoadStage;
pub use main_thread::{MainThread, NotMainThread, defer_to_main, run_deferred};
pub use measured::{
    LatencyVerdict, Measured, NotChecked, PROBE_FLOOR, ProbeConfig, StateHash, TailVerdict, Verdict,
};
pub use metadata::{
    BusChannels, Layout, LoadedPlugin, PluginBus, PluginBuses, PluginTail, Processing,
};
pub use midi::{MIDI_STACK_CAPACITY, MidiEventVec, RT_MIDI_CAPACITY, RtMidiEvents, WireMidi};
pub use midi_intake::MidiIntake;
pub use tutti_graph::BusRole;
pub use wakeups::{Interest, Ready, Wakeups, Watch};
// `NoteExpressionVec` is the alias the change list is built from, so a caller
// assembling one has to name it.
pub use locate::{Helper, Locator};
pub use note_expression::{NoteExpressionChanges, NoteExpressionType, NoteExpressionValue};
pub use note_id::{note_id_to_channel_note, plugin_note_id};
pub use param_edit::ParamEdit;
pub use param_mapping::ParamMapping;
pub use param_unit::{ParamUnit, PlainUnit};
pub use parameters::{
    InvalidNormalized, Normalized, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterInfo,
};
pub use pipeline::{BlockCounts, ChunkCounters, MAX_CHUNK};
pub use presets::{Preset, PresetId, PresetSupport};
#[cfg(feature = "probe-wire")]
pub use probe_wire::{ProbeOutcome, ResultError, read_result, write_result};
pub use process::{BlockStatus, ExpressiveContext, ProcessContext, ProcessOutput};
pub use render_mode::{RenderMode, RenderModeSupport};
pub use sample_format::{SampleFormat, negotiate};
pub use sysex::{SYSEX_MESSAGE_CAPACITY, push_sysex, sysex_assembler, sysex_packets};
pub use transport::{
    BarInfo, LoopRegion, MusicalTiming, PlayheadWatch, TransportChange, TransportChanges,
    TransportFlags, TransportInfo, TransportPosition, is_usable,
};
