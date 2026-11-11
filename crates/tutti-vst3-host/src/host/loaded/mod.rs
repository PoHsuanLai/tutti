//! Post-`initialize()` VST3 state. Audio processing is **not** active here:
//! [`Loaded::activate`](tutti_plugin_types::Loaded::activate) splits the plugin
//! into a [`ProcessorAny`](crate::ProcessorAny), which renders, and a
//! [`Vst3Controller`](crate::Vst3Controller), which holds everything else.
//!
//! `Vst3Loaded` serves GUI-only hosting, offline parameter inspection and state
//! save/restore: the parameter tree, units and program lists, note expression
//! and keyswitch tables, `IMidiLearn`, XML representations, state, and the
//! editor. `process()` lives on the Processor only. The Controller `Deref`s to
//! the `Vst3Loaded` it holds, so all of this stays reachable while the plugin
//! renders.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use tutti_plugin_types::{
    Normalized, ParamFlags, ParamId, ParamRange, ParamSteps, ParameterInfo, PluginDescriptor,
    SampleFormat,
};
use tutti_rt::ArcLend;
use tutti_types::ChannelTopology;
use tutti_vst3_ffi::{
    BusDirection, CreateError, EditorError, MediaType, Plugin, PluginNotification, RestartFlags,
};

use crate::error::{LoadCause, LoadStage, Result, Vst3Error};
use crate::helpers::cid_to_string;
use crate::types::{
    ContentScale, EditorCapabilities, EditorSize, KeyEvent, KeyOutcome, PluginInfo,
    PrefetchSupport, Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo,
    Vst3ProgramListInfo, Vst3UnitInfo, WindowHandle,
};

use super::instance::usable_topology;
use super::library::Library;
use super::midi_learn::MidiLearnConsumer;
use super::midi_mapping::{MidiCcCell, MidiCcMapping};

mod buses;
mod editor;
mod lifecycle;
mod parameters;
mod state;

/// Plugin instance that has been `initialize()`'d and has usable parameter,
/// editor, and state surfaces, but is **not** processing audio.
///
/// [`Loaded::activate`](tutti_plugin_types::Loaded::activate) splits it into a
/// Processor and a Controller to render. For GUI-only hosting (no audio ever),
/// stay here: skip the `setActive(1) + setProcessing(1)` cost entirely.
pub struct Vst3Loaded {
    // Declaration order IS teardown order — Rust drops fields top-to-bottom.
    // Every COM object below is implemented *inside* the plugin DSO, so its
    // vtable lives in that module's text: releasing one after the DSO is gone
    // jumps through a dangling function pointer. `_library` therefore has to be
    // the LAST field, not the first. (Same rule `Library` documents for its
    // own fields.)
    pub(super) plugin: Plugin,
    pub(super) info: PluginInfo,
    /// IMidiLearn forwarding: armed off the main thread, fed captured CCs from
    /// the audio thread, drained in [`Self::poll_plugin_notifications`]. Built at load
    /// and outlives activate/deactivate cycles.
    pub(super) midi_learn: MidiLearnConsumer,
    /// What `rebuild` and `Reopen` load again.
    pub(super) origin: tutti_plugin_types::Origin,
    /// The catalog identity, with the editor measured on first ask.
    pub(super) descriptor: OnceLock<PluginDescriptor>,
    /// Whether the plugin is activated, and at which width.
    pub(super) active: Activation,
    /// Writes a rejoin took from the halves that no block had delivered, for
    /// the next activation's first block.
    pub(super) carried: Vec<(ParamId, Normalized)>,
    /// The `IMidiMapping` table the Processor routes against: published by
    /// whichever half rebuilds it, lent to the Processor once per block.
    pub(super) midi_cc: Arc<ArcLend<MidiCcCell>>,
    /// Keeps the DSO loaded for the plugin's lifetime. **Must stay last** — see
    /// the teardown-order note at the top of this struct.
    pub(super) _library: Arc<Library>,
}

/// The plugin's activation, as the host last made it.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Activation {
    /// The sample format of the current activation; `None` while `setActive(1)`
    /// has not run or `setActive(0)` has since.
    pub(super) format: Option<SampleFormat>,
}

impl Activation {
    pub(super) fn is_active(self) -> bool {
        self.format.is_some()
    }

    pub(super) fn is_f64(self) -> bool {
        self.format == Some(SampleFormat::Float64)
    }
}

/// How to load a plugin from a bundle.
///
/// The default loads the bundle's first audio class.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    class: Option<String>,
}

impl LoadOptions {
    /// The default: the bundle's first audio class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the audio class with this display name from a bundle that exports
    /// several (a name from [`Library::class_info`]).
    ///
    /// One VST3 bundle may export many plugins: that is the normal shape for a
    /// commercial suite, and `mda-vst3` exports 34 audio classes from one
    /// binary.
    #[must_use]
    pub fn class(mut self, name: impl Into<String>) -> Self {
        self.class = Some(name.into());
        self
    }
}

impl std::fmt::Debug for Vst3Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Loaded")
            .field("info", &self.info)
            .field("_library", &self._library)
            .finish_non_exhaustive()
    }
}

/// Summary of the host-side state changes triggered by draining one or more
/// `restartComponent(flags)` requests through
/// [`Vst3Loaded::poll_plugin_notifications`].
///
/// Lets the caller (the bridge/PDC owner) react to coalesced restart signals
/// without re-deriving the bit math: e.g. call
/// [`Vst3Loaded::latency_samples`] when `latency_changed` is set, or
/// re-pull the parameter list when `param_titles_changed`.
///
/// A host that drives a [`Lifecycle`](tutti_plugin_types::Lifecycle) reads the
/// same flags as the core's requests
/// ([`Control::take_requests`](tutti_plugin_types::Control::take_requests)); this is for a plugin held without one.
///
/// The last six restart fields (`note_expression_changed` through
/// `param_id_mapping_changed`) stop at this format layer: carrying them further
/// needs new `AsyncEvent` variants, and appending one is a `PROTOCOL_VERSION`
/// bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestartOutcome {
    /// `kLatencyChanged` fired — the caller should call
    /// [`Vst3Loaded::latency_samples`] and push the result to PDC.
    pub latency_changed: bool,
    /// `kParamValuesChanged` fired — the caller should re-read parameter
    /// values (host-cached automation state is stale).
    pub param_values_changed: bool,
    /// `kParamTitlesChanged` fired — parameter titles/units/flags changed; the
    /// caller should re-pull the parameter list / info.
    pub param_titles_changed: bool,
    /// `kIoChanged` fired — the plugin wants a different bus configuration.
    /// The caller must deactivate and activate the plugin again and then
    /// rewire.
    pub io_changed: bool,
    /// `kMidiCCAssignmentChanged` fired — the `IMidiMapping` CC→param table is
    /// stale and should be re-queried.
    pub midi_cc_assignment_changed: bool,
    /// `kReloadComponent` fired — the plugin needs a full deactivate/reload.
    /// The host path cannot do that from a `&mut Vst3Loaded` (it requires
    /// reconstructing the instance), so this is surfaced for the owner to act.
    pub reload_requested: bool,
    /// `kNoteExpressionChanged` fired — the note-expression type list is stale;
    /// re-query `INoteExpressionController`.
    pub note_expression_changed: bool,
    /// `kIoTitlesChanged` fired — bus *names* changed. Cosmetic: the geometry
    /// is unaffected, so unlike `io_changed` this needs no restart cycle.
    pub io_titles_changed: bool,
    /// `kPrefetchableSupportChanged` fired — the plugin's answer to
    /// `IPrefetchableSupport` changed; re-query before the next offline render.
    pub prefetchable_support_changed: bool,
    /// `kRoutingInfoChanged` fired — `IComponent::getRoutingInfo` is stale.
    pub routing_info_changed: bool,
    /// `kKeyswitchChanged` fired — the keyswitch list is stale; re-query
    /// `IKeyswitchController`.
    pub keyswitch_changed: bool,
    /// `kParamIDMappingChanged` fired (3.7.11) — the `IRemapParamID` mapping
    /// changed. Emitted during project load when a newer plugin version remaps
    /// the ids an older session saved; ignoring it detaches automation lanes
    /// from remapped parameters. Remap through
    /// [`Vst3Loaded::remap_param_id`].
    pub param_id_mapping_changed: bool,
    /// Not a restart flag: the plugin sent more notifications than the queue
    /// holds since the last poll, and the newest were dropped (see
    /// [`NOTIFICATION_CAPACITY`](crate::NOTIFICATION_CAPACITY)). An edit
    /// gesture may have lost its last `PerformEdit`, so the host's copy of any
    /// parameter may be stale: re-read every value, as for
    /// `param_values_changed`. Set only by
    /// [`Vst3Loaded::poll_plugin_notifications`], once per overflow episode.
    pub notifications_overflowed: bool,
}

impl RestartOutcome {
    /// Returns `true` if nothing actionable was reported, so the caller has
    /// nothing to refresh.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The coalesced outcome of one `restartComponent` flag word — or of the
    /// union of several, which is what the handler stores.
    fn from_flags(flags: RestartFlags) -> Self {
        let mut outcome = Self::default();
        outcome.merge_flags(flags);
        outcome
    }

    /// Fold one `restartComponent` bitmask into the coalesced outcome.
    ///
    /// Every flag `RestartFlags` decodes is forwarded.
    fn merge_flags(&mut self, flags: RestartFlags) {
        self.latency_changed |= flags.latency_changed;
        self.param_values_changed |= flags.param_values_changed;
        self.param_titles_changed |= flags.param_titles_changed;
        self.io_changed |= flags.io_changed;
        self.midi_cc_assignment_changed |= flags.midi_cc_assignment_changed;
        self.reload_requested |= flags.reload_component;
        self.note_expression_changed |= flags.note_expression_changed;
        self.io_titles_changed |= flags.io_titles_changed;
        self.prefetchable_support_changed |= flags.prefetchable_support_changed;
        self.routing_info_changed |= flags.routing_info_changed;
        self.keyswitch_changed |= flags.keyswitch_changed;
        self.param_id_mapping_changed |= flags.param_id_mapping_changed;
    }
}

#[cfg(test)]
mod restart_outcome_tests {
    use super::RestartOutcome;
    use tutti_vst3_ffi::RestartFlags;

    #[test]
    fn default_outcome_is_empty() {
        assert!(RestartOutcome::default().is_empty());
    }

    #[test]
    fn merge_flags_accumulates_signals() {
        let mut outcome = RestartOutcome::default();
        outcome.merge_flags(RestartFlags {
            param_values_changed: true,
            ..RestartFlags::default()
        });
        outcome.merge_flags(RestartFlags {
            param_titles_changed: true,
            io_changed: true,
            ..RestartFlags::default()
        });
        assert!(outcome.param_values_changed);
        assert!(outcome.param_titles_changed);
        assert!(outcome.io_changed);
        assert!(!outcome.reload_requested);
        assert!(!outcome.is_empty());
        assert!(!outcome.latency_changed);
    }

    #[test]
    fn merge_flags_is_monotonic() {
        // Once a signal is set, a later empty merge must not clear it.
        let mut outcome = RestartOutcome::default();
        outcome.merge_flags(RestartFlags {
            reload_component: true,
            midi_cc_assignment_changed: true,
            ..RestartFlags::default()
        });
        outcome.merge_flags(RestartFlags::default());
        assert!(outcome.reload_requested);
        assert!(outcome.midi_cc_assignment_changed);
    }

    /// Every flag the decode understands reaches the outcome.
    ///
    /// The failure this guards against is a flag that stops at `RestartFlags`:
    /// decoded into a named field, then dropped by `merge_flags`, so a plugin
    /// can signal it and no consumer ever sees it. A per-flag test would not
    /// catch that — each one passes by simply not being written — so this
    /// asserts the *whole* mapping at once.
    ///
    /// Deliberately spelled without `..Default::default()` on the input: adding
    /// a thirteenth flag must fail to compile here rather than silently join
    /// the set of things that go nowhere.
    #[test]
    fn every_decoded_restart_flag_reaches_the_outcome() {
        let all = RestartFlags {
            reload_component: true,
            io_changed: true,
            param_values_changed: true,
            latency_changed: true,
            param_titles_changed: true,
            midi_cc_assignment_changed: true,
            note_expression_changed: true,
            io_titles_changed: true,
            prefetchable_support_changed: true,
            routing_info_changed: true,
            keyswitch_changed: true,
            param_id_mapping_changed: true,
        };

        let mut outcome = RestartOutcome::default();
        outcome.merge_flags(all);

        // Named individually rather than compared against a fully-populated
        // literal: a missing forward then names the flag that was dropped,
        // instead of printing two twelve-field structs to diff by eye.
        let dropped: Vec<&str> = [
            ("reload_component", outcome.reload_requested),
            ("io_changed", outcome.io_changed),
            ("param_values_changed", outcome.param_values_changed),
            ("latency_changed", outcome.latency_changed),
            ("param_titles_changed", outcome.param_titles_changed),
            (
                "midi_cc_assignment_changed",
                outcome.midi_cc_assignment_changed,
            ),
            ("note_expression_changed", outcome.note_expression_changed),
            ("io_titles_changed", outcome.io_titles_changed),
            (
                "prefetchable_support_changed",
                outcome.prefetchable_support_changed,
            ),
            ("routing_info_changed", outcome.routing_info_changed),
            ("keyswitch_changed", outcome.keyswitch_changed),
            ("param_id_mapping_changed", outcome.param_id_mapping_changed),
        ]
        .into_iter()
        .filter_map(|(name, forwarded)| (!forwarded).then_some(name))
        .collect();

        assert!(
            dropped.is_empty(),
            "these restart flags are decoded but never forwarded, so no \
             consumer can act on them: {dropped:?}"
        );
    }
}

#[cfg(test)]
mod activation_tests {
    use super::Activation;
    use tutti_plugin_types::SampleFormat;

    /// Inactive is the absence of a format; only a 64-bit activation is f64.
    #[test]
    fn activation_is_its_format() {
        let off = Activation::default();
        assert!(!off.is_active() && !off.is_f64());
        let f32_ = Activation {
            format: Some(SampleFormat::Float32),
        };
        assert!(f32_.is_active() && !f32_.is_f64());
        let f64_ = Activation {
            format: Some(SampleFormat::Float64),
        };
        assert!(f64_.is_active() && f64_.is_f64());
    }
}
