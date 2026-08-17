//! Shared value vocabulary for tutti's plugin host crates.
//!
//! Each format-specific host crate (`tutti-vst2-host`, `tutti-vst3-host`,
//! `tutti-clap-host`, `tutti-au-host`) re-exports these types from its own
//! public API so callers can stay format-agnostic when only the shared
//! surface is in play.
//!
//! # Example
//!
//! Two format hosts describe one parameter each. The shapes differ where the
//! ABIs differ, and the reading path — [`ParameterInfo`]'s accessors — is the
//! same for both:
//!
//! ```
//! use tutti_plugin_types::{
//!     Normalized, ParamAddress, ParamFlags, ParamId, ParamSteps, ParameterInfo,
//! };
//!
//! // VST3/CLAP/AU: a plugin-chosen handle, often a hash of the name. Not an
//! // index, not dense, not ordered.
//! let cutoff = ParameterInfo::new(ParamId::new(0x8000_0001), "Cutoff")
//!     .with_plain_range(20.0, 20_000.0, 440.0)
//!     .with_steps(ParamSteps::Continuous)
//!     .with_flags(ParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE);
//!
//! // VST2 alone: a dense position in `[0, numParams)`, and a format that
//! // declined `effGetParameterProperties` — so it declared no range at all.
//! let mix = ParameterInfo::new(ParamAddress::Index(3), "Mix")
//!     .with_normalized_default(0.5);
//!
//! // An absent declaration is `None`, never a substituted number.
//! assert_eq!(cutoff.bounds(), Some((20.0, 20_000.0)));
//! assert_eq!(mix.bounds(), None);
//! assert_eq!(mix.step_count(), None); // `Unknown`, not "continuous"
//! assert_eq!(mix.flag(ParamFlags::AUTOMATABLE), None); // unreported, not `false`
//!
//! // Arithmetic on the number means something for one model and nothing for
//! // the other, so the address is asked rather than cast.
//! assert_eq!(cutoff.id.index(), None);
//! assert_eq!(mix.id.index(), Some(3));
//!
//! // The seam speaks normalized `0..=1`; `to_plain` maps onto the declared range.
//! assert_eq!(cutoff.to_plain(Normalized::new(1.0).get()), 20_000.0);
//! ```
//!
//! # The clamp in [`Normalized::new`] is silent
//!
//! It saturates out-of-range input at the nearest bound and maps NaN to `0.0`,
//! returning no error and logging nothing. Encode ordering data — or any plain
//! value — as a `Normalized` and every entry above `1.0` becomes `1.0`, which
//! reads downstream as a legitimate sweep pinned at maximum rather than as bad
//! input. Normalize before constructing; this type will not tell you that you
//! did not:
//!
//! ```
//! use tutti_plugin_types::Normalized;
//!
//! // A plain 20 kHz cutoff, handed to the normalized seam.
//! assert_eq!(Normalized::new(20_000.0).get(), 1.0);
//!
//! // Three distinct positions collapse onto one value, in silence.
//! let ranks: Vec<f64> = [7.0, 42.0, 79.0]
//!     .into_iter()
//!     .map(|r| Normalized::new(r).get())
//!     .collect();
//! assert_eq!(ranks, [1.0, 1.0, 1.0]);
//!
//! // Divide by the span first, and the positions survive.
//! let span = 79.0;
//! let ranks: Vec<f64> = [7.0, 42.0, 79.0]
//!     .into_iter()
//!     .map(|r| Normalized::new(r / span).get())
//!     .collect();
//! assert!(ranks[0] < ranks[1] && ranks[1] < ranks[2]);
//! ```

mod automation;
mod automation_mode;
mod channels;
mod classification;
mod descriptor;
mod editor;
mod error;
pub mod features;
mod format_host;
mod harmony;
mod load_stage;
mod main_thread;
mod metadata;
mod midi;
mod note_expression;
mod note_id;
mod parameters;
mod presets;
mod process;
mod render_mode;
mod transport;

/// Re-exported so every format crate spells a sample count the same way
/// without each taking its own `tutti-types` dependency — `ChannelLayout`
/// above is here for the same reason.
pub use tutti_types::Samples;
pub use tutti_types::{ChannelLayout, ChannelTopology, Speaker};

mod layout_support;
pub use layout_support::LayoutSupport;
// Musical vocabulary carried on `TransportInfo`. Re-exported for the same reason
// as `ChannelLayout`: format hosts speak these at their ABI boundary and should
// not need a `tutti-types` dependency of their own to name them.
pub use tutti_types::meter::{BarNumber, BeatsPerBar, NoteValue, TimeSignature};

pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use automation_mode::AutomationMode;
pub use channels::{AudioBuffer, AudioBuffer32, AudioBuffer64, AudioBufferMut, BufferPtrs, Sample};
pub use classification::{
    clap_features_role, ClapFeature, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories,
};
pub use descriptor::{AuComponentType, EditorPresence, PluginClass, PluginDescriptor};
pub use editor::{
    AspectRatio, EditorCapabilities, EditorError, EditorSize, ResizeHints, WindowHandle,
};
// Exported as `PluginResult` only. It used to be re-exported under both names
// at this same scope; every one of the 31 call sites took `PluginResult`, and
// the bare `Result` had none — while being exactly the name that shadows std's
// on a glob import.
pub use error::{Delivered, PluginError, Result as PluginResult, StateError};
pub use features::{FeatureReport, Features};
pub use format_host::{
    PluginAudio, PluginEditorHost, PluginInstance, PluginMeta, PluginParams, PluginPresets,
    PluginState,
};
pub use harmony::{
    ChordChanges, ChordValue, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, ScaleChanges, ScaleValue,
};
pub use load_stage::LoadStage;
pub use main_thread::{assert_main_thread, mark_main_thread};
pub use metadata::{BusChannels, BusTopologies, LoadedPlugin, PluginTail};
pub use midi::{MidiEventVec, RtMidiEvents, MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY};
// `NoteExpressionVec` is the SmallVec alias the change list is built from, so
// a caller assembling one has to name it.
pub use note_expression::{
    NoteExpressionChanges, NoteExpressionType, NoteExpressionValue, NoteExpressionVec,
};
pub use note_id::{note_id_for, note_id_to_channel_note, MAX_HOST_NOTE_ID};
pub use parameters::{
    Normalized, ParamAddress, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterInfo,
};
pub use presets::{Preset, PresetId, PresetSupport};
pub use process::{ExpressiveContext, ProcessContext, ProcessOutput};
pub use render_mode::RenderMode;
pub use transport::{
    is_usable, BarInfo, LoopRegion, MusicalTiming, TransportFlags, TransportInfo, TransportPosition,
};

/// Re-export of the workspace-wide MIDI event so host crates don't all
/// need to add a direct `tutti-midi-types` dependency just for the type.
pub use tutti_midi_types::MidiEvent;
