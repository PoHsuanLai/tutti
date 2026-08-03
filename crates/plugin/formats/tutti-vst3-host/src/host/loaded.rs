//! Post-`initialize()` VST3 state. Audio processing is **not** active here —
//! [`Vst3Loaded::activate`] transitions to [`Vst3Instance`] for that.
//!
//! `Vst3Loaded` is what you want for GUI-only hosting, offline parameter
//! inspection, and state save/restore. `process()` lives exclusively on
//! [`Vst3Instance`]; the type system enforces that you can't call it here.

use std::path::Path;
use std::sync::Arc;

use vst3::ComPtr;
use vst3::Steinberg::{
    kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue, FUnknown, IBStream,
    IPlugView, IPlugViewContentScaleSupport, IPlugViewContentScaleSupportTrait, IPlugViewTrait,
    IPluginBaseTrait, IPluginCompatibility, IPluginCompatibilityTrait, ViewRect,
    Vst::{
        IAudioPresentationLatency, IAudioPresentationLatencyTrait, IAudioProcessor,
        IAudioProcessorTrait, IAutomationState, IAutomationStateTrait, IComponent, IComponentTrait,
        IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait,
        IKeyswitchController, IKeyswitchControllerTrait, IMidiLearn, INoteExpressionController,
        INoteExpressionControllerTrait, INoteExpressionPhysicalUIMapping,
        INoteExpressionPhysicalUIMappingTrait, IParameterFunctionName, IParameterFunctionNameTrait,
        IPrefetchableSupport, IPrefetchableSupportTrait, IProcessContextRequirements,
        IProcessContextRequirementsTrait, IRemapParamID, IRemapParamIDTrait,
        IXmlRepresentationController, IXmlRepresentationControllerTrait, PhysicalUIMap,
        PhysicalUIMapList, RepresentationInfo,
    },
};

#[cfg(target_os = "windows")]
use vst3::Steinberg::kPlatformTypeHWND;
#[cfg(target_os = "macos")]
use vst3::Steinberg::kPlatformTypeNSView;
#[cfg(target_os = "linux")]
use vst3::Steinberg::kPlatformTypeX11EmbedWindowID;

use crate::com::{
    BStream, ComponentHandler, HostApplication, HostPlugFrame, ParameterEditEvent, ProgressEvent,
    RestartFlags, UnitEvent,
};
use crate::error::{LoadStage, Result, Vst3Error};
use crate::helpers::cid_to_string;
use crate::types::{
    EditorCapabilities, EditorSize, PluginInfo, ProcessMode, Vst3KeyswitchInfo,
    Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3Sample, WindowHandle,
};

use super::instance::Vst3Instance;
use super::library::Vst3Library;
use super::midi_learn::MidiLearnConsumer;
use super::plugin_state::{Controller, EditorState, HostContext, PluginInterfaces};
use super::{IComponentExt, K_INPUT, K_OUTPUT};

const DEFAULT_EDITOR_SIZE: (u32, u32) = (800, 600);

/// Plugin instance that has been `initialize()`'d and has usable parameter,
/// editor, and state surfaces, but is **not** processing audio.
///
/// Transition to [`Vst3Instance`] via [`Vst3Loaded::activate`] to enable
/// `process()`. For GUI-only hosting (no audio ever), stay here — skip the
/// `setActive(1) + setProcessing(1)` cost entirely.
pub struct Vst3Loaded {
    // Declaration order IS teardown order — Rust drops fields top-to-bottom.
    // Every COM object below is implemented *inside* the plugin DSO, so its
    // vtable lives in that module's text: releasing one after the DSO is gone
    // jumps through a dangling function pointer. `_library` therefore has to be
    // the LAST field, not the first. (Same rule `Vst3Library` documents for its
    // own fields.)
    pub(super) interfaces: PluginInterfaces,
    pub(super) host: HostContext,
    pub(super) editor: EditorState,
    pub(super) info: PluginInfo,
    /// IMidiLearn forwarding: armed off the main thread, fed captured CCs from
    /// the audio thread, drained in [`poll_plugin_notifications`]. Built at load
    /// and outlives activate/deactivate cycles.
    pub(super) midi_learn: MidiLearnConsumer,
    /// Keeps the DSO loaded for the plugin's lifetime. **Must stay last** — see
    /// the teardown-order note at the top of this struct.
    pub(super) _library: Arc<Vst3Library>,
}

/// Summary of the host-side state changes triggered by draining one or more
/// `restartComponent(flags)` requests through
/// [`Vst3Loaded::poll_plugin_notifications`].
///
/// Lets the caller (the bridge/PDC owner) react to coalesced restart signals
/// without re-deriving the bit math: e.g. call
/// [`Vst3Loaded::read_latency_samples`] when `latency_changed` is set, or
/// re-pull the parameter list when `param_titles_changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestartOutcome {
    /// `kLatencyChanged` fired — the caller should call
    /// [`Vst3Loaded::read_latency_samples`] and push the result to PDC.
    pub latency_changed: bool,
    /// `kParamValuesChanged` fired — the caller should re-read parameter
    /// values (host-cached automation state is stale).
    pub param_values_changed: bool,
    /// `kParamTitlesChanged` fired — parameter titles/units/flags changed; the
    /// caller should re-pull the parameter list / info.
    pub param_titles_changed: bool,
    /// `kIoChanged` fired — the plugin wants a different bus configuration.
    /// The caller must run `Vst3Instance::restart_bus_configuration` (a
    /// deactivate/reactivate cycle) and then rewire.
    pub io_changed: bool,
    /// `kMidiCCAssignmentChanged` fired — the `IMidiMapping` CC→param table is
    /// stale and should be re-queried (V3).
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
    /// changed. Emitted during *project load*, when a newer plugin version
    /// remaps the ids an older session saved: dropping it silently detaches
    /// every automation lane that referenced a remapped parameter.
    pub param_id_mapping_changed: bool,
    // The six fields above stop here, at the format layer, on purpose. Carrying
    // them further means new `AsyncEvent` variants, and bincode encodes a
    // discriminant, so appending one is a `PROTOCOL_VERSION` bump — paid for a
    // signal nothing yet acts on. `AsyncEvent::IoChanged` already shows where
    // that leads: it crosses the wire to a `BridgeMessage` and no consumer
    // reads it. Decoding a flag and dropping it inside one crate is a gap;
    // shipping six across a versioned boundary to no receiver is the
    // `PluginTail` mistake, which stayed write-only for a release cycle.
    //
    // What this fix buys is that the signal now *exists* where a consumer can
    // reach it, and `every_decoded_restart_flag_reaches_the_outcome` keeps it
    // that way. Plumbing follows a consumer, not the other way round.
}

impl RestartOutcome {
    /// True if nothing actionable was reported — the caller can skip any
    /// follow-up work.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold one `restartComponent` bitmask into the coalesced outcome.
    ///
    /// Every flag `RestartFlags` decodes is forwarded. Six of the twelve used
    /// to stop here — decoded into the struct above and then dropped, so a
    /// plugin could signal them into a consumer that had no field to receive
    /// them. `param_id_mapping_changed` is the one that bites: it fires during
    /// project load and losing it detaches automation from remapped parameters.
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

/// One batch of everything the plugin's editor pushed to the host since the
/// last poll, returned by [`Vst3Loaded::poll_plugin_notifications`].
///
/// All four fields are independent; a typical idle tick has them all empty
/// (`param_edits`/`progress`/`units` empty and `restart.is_empty()`).
#[derive(Debug, Clone, Default)]
pub struct PluginNotifications {
    /// Non-restart parameter-edit events (`BeginEdit`/`PerformEdit`/`EndEdit`/
    /// `SetDirty`/…) in arrival order. `RestartComponent` requests are folded
    /// into `restart` instead of appearing here.
    pub param_edits: Vec<ParameterEditEvent>,
    /// Coalesced restart side-effects, all for the caller to act on — nothing
    /// here has been handled in place. `io_changed` and `latency_changed` both
    /// require a deactivate/reactivate cycle this type cannot perform.
    pub restart: RestartOutcome,
    /// `IProgress` reports for long plugin operations (sample loading, offline
    /// rendering) — drive a host progress indicator.
    pub progress: Vec<ProgressEvent>,
    /// `IUnitHandler` notifications: the user changed a unit / program inside
    /// the plugin's own UI, or the unit↔bus mapping changed.
    pub units: Vec<UnitEvent>,
}

impl Vst3Loaded {
    /// Lightweight metadata read: load the library, read factory and bus info,
    /// return without calling `initialize()` or `setActive()`. Safe for plugins
    /// that would otherwise pop license dialogs or hit the network during full
    /// load.
    pub fn probe(path: &Path) -> Result<PluginInfo> {
        check_exists(path)?;
        let library = Vst3Library::load(path)?;
        ensure_has_classes(&library, path)?;

        let class = find_audio_class(&library, path)?;
        let component: ComPtr<IComponent> = library.create_instance(&class.cid)?;
        let processor = component.cast::<IAudioProcessor>();
        Ok(build_plugin_info_raw(
            &library,
            &component,
            processor.as_ref(),
            &class,
        ))
    }

    /// Load a VST3 plugin for GUI / parameter work only — no audio processing
    /// will ever happen on the returned value. Stays in `Loaded` state, skipping
    /// the activation cost.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::LoadFailed`](crate::Vst3Error::LoadFailed) if the
    /// file is missing, the DSO can't be opened, the factory is empty, or no
    /// audio class is found; returns
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if
    /// `IPluginBase::initialize` fails.
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_class(path, None)
    }

    /// Load a specific audio class from a bundle, by display name.
    ///
    /// One VST3 bundle may export many plugins — that is the normal shape for a
    /// commercial suite, and the sample corpus has it too: `mda-vst3` exports
    /// 34 audio classes from one binary. [`load`](Self::load) takes the first,
    /// which is the right default for a single-plugin bundle and useless for
    /// picking "mda Delay" out of the 34.
    ///
    /// `class_name` matches [`ClassInfo::name`](crate::host::ClassInfo::name)
    /// exactly; `None` reproduces [`load`](Self::load).
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load), plus
    /// [`Vst3Error::LoadFailed`](crate::Vst3Error::LoadFailed) when no audio
    /// class carries `class_name` — the message lists what the bundle does
    /// export, since a near-miss on a display name is the likely cause.
    pub fn load_class(path: &Path, class_name: Option<&str>) -> Result<Self> {
        check_exists(path)?;
        let library = Vst3Library::load(path)?;
        ensure_has_classes(&library, path)?;

        let class = match class_name {
            Some(wanted) => find_audio_class_named(&library, path, wanted)?,
            None => find_audio_class(&library, path)?,
        };
        let component: ComPtr<IComponent> = library.create_instance(&class.cid)?;
        let processor =
            component
                .cast::<IAudioProcessor>()
                .ok_or_else(|| Vst3Error::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Instantiation,
                    reason: "VST3 plugin does not support IAudioProcessor".to_string(),
                })?;
        let controller = query_controller(&component, &library);
        let info = build_plugin_info(&library, &component, &processor, &class);

        let mut loaded = Self::assemble(library, component, processor, controller, info);
        loaded.initialize()?;
        Ok(loaded)
    }

    /// Build `Self` from already-queried interfaces. No side effects — the
    /// caller runs [`initialize`](Self::initialize).
    fn assemble(
        library: Arc<Vst3Library>,
        component: ComPtr<IComponent>,
        processor: ComPtr<IAudioProcessor>,
        controller: Controller,
        info: PluginInfo,
    ) -> Self {
        // Same run loop as the factory-level host context: a plugin that only
        // sees the context handed to `IPluginBase::initialize` must reach the
        // loop the host actually pumps.
        let host_application = HostApplication::new(
            super::library::HOST_NAME,
            #[cfg(target_os = "linux")]
            library.run_loop(),
        );
        let (component_handler, param_event_rx, progress_event_rx, unit_event_rx) =
            ComponentHandler::new();

        let process_context_requirements = query_process_context_requirements(&processor);
        let note_expression = controller
            .as_ref()
            .and_then(|c| c.cast::<INoteExpressionController>());
        let automation_state = controller
            .as_ref()
            .and_then(|c| c.cast::<IAutomationState>());
        let keyswitch = controller
            .as_ref()
            .and_then(|c| c.cast::<IKeyswitchController>());
        let remap_param_id = controller.as_ref().and_then(|c| c.cast::<IRemapParamID>());
        let parameter_function_name = controller
            .as_ref()
            .and_then(|c| c.cast::<IParameterFunctionName>());
        let physical_ui_mapping = controller
            .as_ref()
            .and_then(|c| c.cast::<INoteExpressionPhysicalUIMapping>());
        let xml_representation = controller
            .as_ref()
            .and_then(|c| c.cast::<IXmlRepresentationController>());
        let prefetchable_support = processor.cast::<IPrefetchableSupport>();
        let audio_presentation_latency = processor.cast::<IAudioPresentationLatency>();
        let midi_learn =
            MidiLearnConsumer::new(controller.as_ref().and_then(|c| c.cast::<IMidiLearn>()));

        Self {
            _library: library,
            interfaces: PluginInterfaces {
                component,
                processor,
                controller,
                process_context_requirements,
                note_expression,
                automation_state,
                keyswitch,
                remap_param_id,
                parameter_function_name,
                physical_ui_mapping,
                xml_representation,
                prefetchable_support,
                audio_presentation_latency,
            },
            host: HostContext {
                application: host_application,
                handler: component_handler,
                param_event_rx,
                progress_event_rx,
                unit_event_rx,
            },
            editor: EditorState::Closed,
            info,
            midi_learn,
        }
    }

    /// Transition to the processing state. Runs `setupProcessing`, activates
    /// buses, calls `setActive(1)` and `setProcessing(1)`. Returns a
    /// [`Vst3Instance<T>`] that exposes `process()`.
    ///
    /// `T` fixes the sample format: `f32` (the default) uses `kSample32`;
    /// `f64` uses `kSample64` and returns [`Vst3Error::NotSupported`] if the
    /// plugin does not advertise 64-bit support.
    pub fn activate<T: Vst3Sample>(
        self,
        sample_rate: f64,
        block_size: usize,
    ) -> Result<Vst3Instance<T>> {
        self.activate_with_mode(sample_rate, block_size, ProcessMode::Realtime)
    }

    /// Transition to the processing state for a specific [`ProcessMode`].
    ///
    /// The mode is chosen *here*, on the state transition, rather than on the
    /// resulting instance, because `setupProcessing` is where VST3 delivers it
    /// and that call happens exactly once per activation. Selecting
    /// [`Offline`](ProcessMode::Offline) afterwards would require re-running
    /// setup, which is what the spec's `ProcessSetup`/`ProcessData` agreement
    /// rule forbids doing silently — so the type-state boundary and the spec
    /// boundary are made to coincide. (The realtime↔prefetch pair *is*
    /// switchable on a live instance; that is
    /// [`Vst3Instance::set_prefetch`](crate::Vst3Instance::set_prefetch), and
    /// it is the one exception the rule names.)
    ///
    /// Query [`prefetchable_support`](Self::prefetchable_support) beforehand if
    /// you intend to use prefetch: it is the plugin's own statement about
    /// whether it can be driven that way.
    ///
    /// # Errors
    ///
    /// As [`activate`](Self::activate).
    pub fn activate_with_mode<T: Vst3Sample>(
        self,
        sample_rate: f64,
        block_size: usize,
        mode: ProcessMode,
    ) -> Result<Vst3Instance<T>> {
        Vst3Instance::from_loaded(self, sample_rate, block_size, mode)
    }

    /// Metadata snapshot (id, name, vendor, bus counts, MIDI and f64 support).
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Read the current processing latency directly from
    /// `IAudioProcessor::getLatencySamples`. Call this at load time and
    /// whenever [`RestartOutcome::latency_changed`] is set to get the fresh
    /// value for PDC.
    pub fn read_latency_samples(&self) -> u32 {
        tutti_plugin_types::assert_main_thread();
        unsafe { self.interfaces.processor.getLatencySamples() }
    }

    /// Read the plugin's tail length from `IAudioProcessor::getTailSamples` —
    /// how long it keeps sounding after its input goes silent.
    ///
    /// The raw count, so `0` (no tail) and the saturating `u32::MAX` (an
    /// effectively unbounded one) both reach the caller as the plugin stated
    /// them. `PluginTail::from_samples` is what turns those into the shared
    /// vocabulary; this stays at the ABI's own type so nothing is decided here.
    pub fn read_tail_samples(&self) -> u32 {
        tutti_plugin_types::assert_main_thread();
        unsafe { self.interfaces.processor.getTailSamples() }
    }

    // ── Parameters: ParamID vs index ──────────────────────────────────────────
    //
    // `IEditController` uses TWO different address spaces and conflating them
    // is silent — you get a plausible `f64` for the wrong parameter:
    //
    // - `getParameterInfo(int32 paramIndex, ..)` is addressed by **index**,
    //   `0 .. getParameterCount()`, and yields the `ParameterInfo` whose `.id`
    //   is the ParamID.
    // - `getParamNormalized(ParamID)` / `setParamNormalized(ParamID, ..)` are
    //   addressed by **ParamID**, an opaque plugin-chosen `uint32`. It is not
    //   an index and is under no obligation to be small, dense, or ordered —
    //   plenty of plugins derive it from a hash of the parameter name.
    //
    // The methods below are therefore named and documented for the space they
    // actually take, and the `*_by_index` pair resolves `index → ParamID`
    // through `getParameterInfo` instead of passing an index straight into a
    // ParamID slot (which reads whichever parameter happens to own that numeric
    // id, or nothing at all).

    /// Number of automatable parameters exposed by the edit controller — the
    /// exclusive upper bound of the **index** space.
    /// Returns `0` if the plugin has no controller.
    pub fn parameter_count(&self) -> u32 {
        match self.interfaces.controller.as_ref() {
            Some(c) => unsafe { c.getParameterCount() as u32 },
            None => 0,
        }
    }

    /// Read the normalized (0.0 – 1.0) value of the parameter with **ParamID**
    /// `param_id` — *not* an index (see the note above; use
    /// [`parameter_by_index`](Self::parameter_by_index) to address by index).
    /// Returns `0.0` if the plugin has no controller.
    pub fn parameter(&self, param_id: u32) -> f64 {
        match self.interfaces.controller.as_ref() {
            Some(c) => unsafe { c.getParamNormalized(param_id) },
            None => 0.0,
        }
    }

    /// Write a normalized (0.0 – 1.0) `value` to the parameter with **ParamID**
    /// `param_id` — *not* an index (use
    /// [`set_parameter_by_index`](Self::set_parameter_by_index) for that).
    /// No-op if the plugin has no controller.
    pub fn set_parameter(&mut self, param_id: u32, value: f64) {
        if let Some(c) = self.interfaces.controller.as_ref() {
            unsafe {
                c.setParamNormalized(param_id, value);
            }
        }
    }

    /// The **ParamID** of the parameter at `index` in `0 .. parameter_count()`.
    /// `None` when the index is out of range or the plugin has no controller.
    ///
    /// This is the bridge between the two address spaces: anything iterating
    /// `0..parameter_count()` must go through it (or
    /// [`parameter_info`](Self::parameter_info)) before calling
    /// [`parameter`](Self::parameter) / [`set_parameter`](Self::set_parameter).
    pub fn parameter_id_at(&self, index: u32) -> Option<u32> {
        self.parameter_info(index).map(|info| info.id)
    }

    /// Read the normalized value of the parameter at **index**, resolving the
    /// index to its ParamID first. `None` if the index is out of range or the
    /// plugin has no controller.
    pub fn parameter_by_index(&self, index: u32) -> Option<f64> {
        self.parameter_id_at(index).map(|id| self.parameter(id))
    }

    /// Write a normalized `value` to the parameter at **index**, resolving the
    /// index to its ParamID first. Returns `false` when the index is out of
    /// range or the plugin has no controller (nothing was written).
    pub fn set_parameter_by_index(&mut self, index: u32, value: f64) -> bool {
        match self.parameter_id_at(index) {
            Some(id) => {
                self.set_parameter(id, value);
                true
            }
            None => false,
        }
    }

    /// Descriptor for the parameter at **index** (title, units, flags, default,
    /// and `id` — the ParamID to address it by).
    /// Returns `None` if the index is out of range or the plugin has no
    /// controller.
    pub fn parameter_info(&self, index: u32) -> Option<Vst3ParameterInfo> {
        let controller = self.interfaces.controller.as_ref()?;
        let mut raw: vst3::Steinberg::Vst::ParameterInfo = unsafe { std::mem::zeroed() };
        let result = unsafe { controller.getParameterInfo(index as i32, &mut raw) };
        (result == kResultOk).then(|| Vst3ParameterInfo::from_c(&raw))
    }

    /// The plugin's own `[min, max]` for a parameter, recovered by asking its
    /// controller to invert the normalized map at the endpoints.
    ///
    /// VST3 reports no range on `ParameterInfo` — every value it exchanges is
    /// normalized `0..=1`. But `IEditController::normalizedParamToPlain` is the
    /// same map the plugin's own editor uses to render "440 Hz", so probing it
    /// recovers what the parameter actually means.
    ///
    /// `None` when there is no controller to ask, or when the map is not
    /// monotonic. `normalizedParamToPlain` returns a bare `ParamValue` with no
    /// `tresult`, so a plugin that doesn't implement it cannot report failure —
    /// the midpoint probe is the only available coherence check.
    ///
    /// An identity map is *not* a failure: the SDK's default implementation
    /// returns its input, and for a parameter with no separate plain domain
    /// (a Mix knob) `0..=1` is the truthful answer. That is the difference
    /// between this and hardcoding `0.0..1.0` — the numbers can coincide, but
    /// here they are what the plugin said when asked.
    ///
    /// Must run on the main thread with the controller connected, per the SDK's
    /// `[UI-thread & Connected]` annotation on the method.
    pub fn parameter_plain_range(&self, id: u32) -> Option<(f64, f64)> {
        let controller = self.interfaces.controller.as_ref()?;
        let at = |n: f64| unsafe { controller.normalizedParamToPlain(id, n) };

        let (lo, mid, hi) = (at(0.0), at(0.5), at(1.0));
        if !lo.is_finite() || !mid.is_finite() || !hi.is_finite() {
            return None;
        }
        // A range the endpoints alone would accept but whose interior
        // contradicts them is not a range we can map onto. Inclusive because a
        // legitimately constant parameter probes flat.
        let monotonic = (lo <= mid && mid <= hi) || (hi <= mid && mid <= lo);
        monotonic.then_some(if lo <= hi { (lo, hi) } else { (hi, lo) })
    }

    /// Number of per-note expression types the plugin supports on the given
    /// event `bus_index` / MIDI `channel`. Returns `0` if the plugin doesn't
    /// implement `INoteExpressionController`.
    ///
    /// This is the **read** side of note expression — pair it with
    /// [`note_expression_info`](Self::note_expression_info) to enumerate
    /// descriptors. The host can always **send**
    /// [`NoteExpressionValue`](crate::NoteExpressionValue) events regardless of
    /// what this reports.
    pub fn note_expression_count(&self, bus_index: i32, channel: i16) -> u32 {
        match &self.interfaces.note_expression {
            Some(c) => unsafe { c.getNoteExpressionCount(bus_index, channel).max(0) as u32 },
            None => 0,
        }
    }

    /// Raw `IProcessContextRequirements::getProcessContextRequirements` bitmask
    /// the plugin returned at load (see [`process_context_flags`]). `u32::MAX`
    /// means the plugin doesn't implement the interface — treat as "wants
    /// everything", matching the pre-interface unconditional behaviour.
    ///
    /// [`process_context_flags`]: crate::types::process_context_flags
    pub fn context_requirements(&self) -> u32 {
        self.interfaces.process_context_requirements
    }

    /// `true` if the plugin asked for any transport field (tempo, playhead,
    /// bar, cycle, time-sig, or transport state). A plugin that implements
    /// `IProcessContextRequirements` and requests none of these does not want a
    /// transport snapshot each block; one that doesn't implement the interface
    /// (`u32::MAX`) wants everything.
    pub fn wants_transport(&self) -> bool {
        use crate::types::process_context_flags as f;
        let req = self.interfaces.process_context_requirements;
        const TRANSPORT_BITS: u32 = f::NEED_TEMPO
            | f::NEED_PROJECT_TIME_MUSIC
            | f::NEED_BAR_POSITION_MUSIC
            | f::NEED_CYCLE_MUSIC
            | f::NEED_TIME_SIGNATURE
            | f::NEED_TRANSPORT_STATE
            | f::NEED_CONTINOUS_TIME_SAMPLES;
        req & TRANSPORT_BITS != 0
    }

    /// `true` if the plugin asked for the host-track chord field
    /// (`kNeedChord`) — the VST3 signal that it consumes sequencer context
    /// (chord / scale). Distinct from note-expression.
    pub fn wants_sequencer_context(&self) -> bool {
        use crate::types::process_context_flags as f;
        self.interfaces.process_context_requirements & f::NEED_CHORD != 0
    }

    /// Descriptor for the note-expression type at `index` on the given event
    /// `bus_index` / MIDI `channel` (title, units, value range, flags). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `INoteExpressionController`.
    pub fn note_expression_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3NoteExpressionInfo> {
        let controller = self.interfaces.note_expression.as_ref()?;
        let mut raw: vst3::Steinberg::Vst::NoteExpressionTypeInfo = unsafe { std::mem::zeroed() };
        let result =
            unsafe { controller.getNoteExpressionInfo(bus_index, channel, index as i32, &mut raw) };
        (result == kResultOk).then(|| Vst3NoteExpressionInfo::from_c(&raw))
    }

    /// Number of key-switch (articulation) entries the plugin exposes on the
    /// given event `bus_index` / MIDI `channel`. Returns `0` if the plugin
    /// doesn't implement `IKeyswitchController`.
    ///
    /// Pair with [`keyswitch_info`](Self::keyswitch_info) to enumerate the
    /// articulation map a sample-library instrument advertises.
    pub fn keyswitch_count(&self, bus_index: i32, channel: i16) -> u32 {
        match &self.interfaces.keyswitch {
            Some(c) => unsafe { c.getKeyswitchCount(bus_index, channel).max(0) as u32 },
            None => 0,
        }
    }

    /// Descriptor for the key switch at `index` on the given event `bus_index` /
    /// MIDI `channel` (articulation title, trigger key range, kind). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `IKeyswitchController`.
    pub fn keyswitch_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3KeyswitchInfo> {
        let controller = self.interfaces.keyswitch.as_ref()?;
        let mut raw: vst3::Steinberg::Vst::KeyswitchInfo = unsafe { std::mem::zeroed() };
        let result =
            unsafe { controller.getKeyswitchInfo(bus_index, channel, index as i32, &mut raw) };
        (result == kResultOk).then(|| Vst3KeyswitchInfo::from_c(&raw))
    }

    /// Ask the plugin (via `IRemapParamID`) for the parameter ID in *this*
    /// plugin that corresponds to `old_param_id` from a *previous* plugin
    /// identified by `plugin_to_replace_uid` (its processor class ID / `TUID`).
    ///
    /// This is how a host carries saved automation forward when swapping one
    /// plugin for a newer/compatible one: each old automation lane's `ParamID`
    /// is remapped to the replacement's. Returns `Some(new_id)` when the plugin
    /// reports a compatible parameter (possibly equal to `old_param_id`), or
    /// `None` when there is none or the plugin doesn't implement `IRemapParamID`.
    ///
    /// The host does **not** call this automatically anywhere — like JUCE, it's
    /// exposed for a caller-driven migration flow to use. Must run on the
    /// main/UI thread.
    pub fn remap_param_id(
        &self,
        plugin_to_replace_uid: &[i8; 16],
        old_param_id: u32,
    ) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        let remap = self.interfaces.remap_param_id.as_ref()?;
        let mut new_param_id: u32 = 0;
        let result = unsafe {
            remap.getCompatibleParamID(
                plugin_to_replace_uid as *const [i8; 16],
                old_param_id,
                &mut new_param_id,
            )
        };
        (result == kResultTrue).then_some(new_param_id)
    }

    /// Resolve a well-known parameter *function name* (the VST3 spec defines
    /// roles like "Wet/Dry Mix", "Master Volume", "Resonance") to the plugin's
    /// `ParamID` for that role, scoped to `unit_id`. Returns `None` if the role
    /// is unknown to the plugin or it doesn't implement `IParameterFunctionName`.
    ///
    /// Lets a host bind a generic "mix" knob to whatever parameter the plugin
    /// uses for it, without hard-coding parameter indices. Main/UI thread.
    pub fn param_id_for_function_name(&self, unit_id: i32, function_name: &str) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        let ctrl = self.interfaces.parameter_function_name.as_ref()?;
        let c_name = std::ffi::CString::new(function_name).ok()?;
        let mut param_id: u32 = 0;
        let result =
            unsafe { ctrl.getParameterIDFromFunctionName(unit_id, c_name.as_ptr(), &mut param_id) };
        (result == kResultOk).then_some(param_id)
    }

    /// Map the plugin's physical UI controls to the note-expression dimensions
    /// they drive, on the given event `bus_index` / MIDI `channel`. Returns one
    /// `(physical_ui_type, note_expression_type)` pair per physical control
    /// (X/Y movement, pressure — see [`physical_ui_type`](crate::physical_ui_type)),
    /// or an empty vec if the plugin doesn't implement
    /// `INoteExpressionPhysicalUIMapping`.
    ///
    /// The host allocates the list; the plugin fills the note-expression id each
    /// physical control is wired to. Main/UI thread.
    pub fn physical_ui_mapping(&self, bus_index: i32, channel: i16) -> Vec<(u32, u32)> {
        tutti_plugin_types::assert_main_thread();
        let Some(ctrl) = self.interfaces.physical_ui_mapping.as_ref() else {
            return Vec::new();
        };
        // Query all three physical UI types (X, Y, pressure). The plugin fills
        // each entry's noteExpressionTypeID, leaving kInvalidTypeID where the
        // control isn't mapped.
        let mut entries: Vec<PhysicalUIMap> = (0..3)
            .map(|i| PhysicalUIMap {
                physicalUITypeID: i,
                noteExpressionTypeID: u32::MAX,
            })
            .collect();
        let mut list = PhysicalUIMapList {
            count: entries.len() as u32,
            map: entries.as_mut_ptr(),
        };
        let result = unsafe { ctrl.getPhysicalUIMapping(bus_index, channel, &mut list) };
        if result != kResultOk {
            return Vec::new();
        }
        entries
            .iter()
            .map(|e| (e.physicalUITypeID, e.noteExpressionTypeID))
            .collect()
    }

    /// Export the plugin's parameter remote-control layout as XML, for the
    /// representation identified by `(vendor, name, version, host)`. Returns the
    /// XML string, or `None` if the plugin doesn't implement
    /// `IXmlRepresentationController` or produced nothing.
    ///
    /// Hardware controller surfaces use this to lay out a plugin's parameters.
    /// Main/UI thread.
    pub fn xml_representation(
        &self,
        vendor: &str,
        name: &str,
        version: &str,
        host: &str,
    ) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        let ctrl = self.interfaces.xml_representation.as_ref()?;

        let mut info: RepresentationInfo = unsafe { std::mem::zeroed() };
        fill_char8_64(&mut info.vendor, vendor);
        fill_char8_64(&mut info.name, name);
        fill_char8_64(&mut info.version, version);
        fill_char8_64(&mut info.host, host);

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        let result = unsafe { ctrl.getXmlRepresentationStream(&mut info, stream_ptr.as_ptr()) };
        if result != kResultOk {
            return None;
        }
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Read the bundle's machine-readable compatibility / migration info as a
    /// JSON string, via the factory's `IPluginCompatibility` class (the
    /// moduleinfo "compatibility" section). Describes which older plugins this
    /// one can replace, so a host can offer to swap an unavailable plugin for a
    /// compatible successor and migrate its state.
    ///
    /// Unlike the other accessors this is a **factory-level** class, not a
    /// controller/processor extension: we enumerate the factory's classes, find
    /// the one in the "Plugin Compatibility Class" category, instantiate it, and
    /// read its JSON. Returns `None` if the bundle ships no such class. Main/UI
    /// thread.
    pub fn compatibility_json(&self) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        // The SDK category string for the compatibility class (kPluginCompatibilityClass).
        const COMPATIBILITY_CATEGORY: &str = "Plugin Compatibility Class";

        let count = self._library.count_classes();
        let cid = (0..count).find_map(|i| {
            let info = self._library.get_class_info(i).ok()?;
            (info.category == COMPATIBILITY_CATEGORY).then_some(info.cid)
        })?;

        let compat = self
            ._library
            .create_instance::<IPluginCompatibility>(&cid)
            .ok()?;

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        let result = unsafe { compat.getCompatibilityJSON(stream_ptr.as_ptr()) };
        if result != kResultOk {
            return None;
        }
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Query the plugin's offline/prefetch processing support via
    /// `IPrefetchableSupport`. Returns one of the
    /// [`prefetchable_support`](crate::prefetchable_support) constants, or `None`
    /// if the plugin doesn't implement the interface. Main/UI thread.
    pub fn prefetchable_support(&self) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        let proc = self.interfaces.prefetchable_support.as_ref()?;
        let mut support: u32 = 0;
        let result = unsafe { proc.getPrefetchableSupport(&mut support) };
        (result == kResultOk).then_some(support)
    }

    /// Tell the plugin the downstream presentation latency (in samples) for a
    /// given bus, via `IAudioPresentationLatency` — the delay between the
    /// plugin's output and what the listener hears, so latency-aware plugins can
    /// compensate. `dir` is [`K_INPUT`](super::K_INPUT) / [`K_OUTPUT`](super::K_OUTPUT).
    /// Returns `true` if delivered; no-op if the plugin doesn't implement the
    /// interface. Main/UI thread.
    pub fn set_audio_presentation_latency(
        &mut self,
        dir: i32,
        bus_index: i32,
        latency_samples: u32,
    ) -> bool {
        tutti_plugin_types::assert_main_thread();
        match &self.interfaces.audio_presentation_latency {
            Some(p) => {
                unsafe {
                    p.setAudioPresentationLatencySamples(dir, bus_index, latency_samples);
                }
                true
            }
            None => false,
        }
    }

    /// Drain every host-side notification channel the plugin's editor pushes
    /// to and return them as one [`PluginNotifications`] batch.
    ///
    /// This is the single polling entry point for everything the plugin reports
    /// asynchronously between process calls:
    /// - **`param_edits`** — `BeginEdit`/`PerformEdit`/`EndEdit`/… in arrival
    ///   order (the `RestartComponent` requests are stripped out and folded into
    ///   `restart` instead).
    /// - **`restart`** — coalesced [`RestartOutcome`]. `kIoChanged` is acted on
    ///   in place (bus re-enumeration); `kLatencyChanged` and the rest are
    ///   flagged for the caller (e.g. call
    ///   [`read_latency_samples`](Self::read_latency_samples) on
    ///   `restart.latency_changed`).
    /// - **`progress`** — `IProgress` start/update/finish reports (long
    ///   operations such as sample loading), for a host-drawn progress bar.
    /// - **`units`** — `IUnitHandler` unit-selection / program-list / unit-by-bus
    ///   changes the user made inside the plugin's own UI.
    ///
    /// Must be called on the main thread; not while inside
    /// [`process`](Vst3Instance::process).
    pub fn poll_plugin_notifications(&mut self) -> PluginNotifications {
        tutti_plugin_types::assert_main_thread();
        let mut notifications = PluginNotifications::default();
        for event in self.host.param_event_rx.try_iter().collect::<Vec<_>>() {
            match event {
                ParameterEditEvent::RestartComponent(flags) => {
                    let flags = RestartFlags::from_bits(flags);
                    // `kIoChanged` is deliberately NOT acted on here. The spec
                    // requires deactivate → re-ask → reactivate
                    // (`ivsteditcontroller.h:125-127`), and this method holds a
                    // `&mut Vst3Loaded`, which cannot deactivate anything — the
                    // same constraint `reload_requested` is surfaced for.
                    //
                    // It used to call `reconcile_bus_counts()` from here, which
                    // reads fine until you notice `Vst3Instance` `DerefMut`s to
                    // this type: the production caller polls on a live active
                    // instance, so the re-enumeration ran mid-activation. The
                    // owner calls `Vst3Instance::restart_bus_configuration`.
                    notifications.restart.merge_flags(flags);
                }
                other => notifications.param_edits.push(other),
            }
        }
        notifications.progress = self.host.progress_event_rx.try_iter().collect();
        notifications.units = self.host.unit_event_rx.try_iter().collect();
        // Forward any live CCs the audio thread captured for MIDI-learn to the
        // plugin's IMidiLearn on this (main) thread, as the SDK requires. No-op
        // unless learn is armed and the plugin implements IMidiLearn.
        self.midi_learn.forward_pending();
        notifications
    }

    /// Arm or disarm VST3 MIDI learn (`IMidiLearn`).
    ///
    /// While armed, the realtime path captures incoming MIDI CCs and the next
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications) forwards
    /// them to the plugin's `IMidiLearn::onLiveMIDIControllerInput` — letting the
    /// plugin bind the moved controller to whatever parameter the user is
    /// editing in its own UI. Typically: arm on right-click-knob → "MIDI learn",
    /// let the user move a controller, observe the resulting
    /// `kMidiCCAssignmentChanged` restart, then disarm.
    ///
    /// No observable effect if the plugin doesn't implement `IMidiLearn`.
    pub fn arm_midi_learn(&mut self, armed: bool) {
        tutti_plugin_types::assert_main_thread();
        self.midi_learn.arm(armed);
    }

    /// Whether VST3 MIDI learn is currently armed. See
    /// [`arm_midi_learn`](Self::arm_midi_learn).
    pub fn is_midi_learn_armed(&self) -> bool {
        self.midi_learn.is_armed()
    }

    /// Tell the plugin the host's current automation read/write mode via
    /// `IAutomationState`. `state` is one of the
    /// [`automation_state`](crate::automation_state) constants
    /// (`NONE` / `READ` / `WRITE` / `READ_WRITE`).
    ///
    /// Some plugins change behaviour during automation playback vs recording
    /// (e.g. snapping a knob to the automation lane while reading). No-op if the
    /// plugin doesn't implement `IAutomationState`. Returns `true` if the call
    /// was delivered. Must run on the main/UI thread.
    pub fn set_automation_state(&mut self, state: i32) -> bool {
        tutti_plugin_types::assert_main_thread();
        match &self.interfaces.automation_state {
            Some(a) => {
                unsafe { a.setAutomationState(state) };
                true
            }
            None => false,
        }
    }

    /// Capture the plugin's state as an opaque byte blob suitable for
    /// persisting and later feeding back to [`set_state`](Self::set_state).
    ///
    /// This carries **both** of the plugin's streams. The spec gives the
    /// controller its own `getState`/`setState` pair, distinct from the
    /// component's, holding what only the UI knows: scroll position, the
    /// selected tab, a meter's display mode. Saving just the component stream
    /// discards all of it on every save/restore.
    ///
    /// The two are packed by [`pack_state`] rather than concatenated, because
    /// each half is a private format of unknown length — only an explicit
    /// length prefix can split them again.
    ///
    /// The bytes within each half are the plugin's private format; the host
    /// never interprets them. A plugin with no state at all yields an empty
    /// blob, which `set_state` accepts back as a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`](crate::Vst3Error::StateError) if the
    /// host-side `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if the plugin
    /// fails `getState`. We do **not** substitute a parameter dump: a plugin's
    /// state covers more than parameters (active preset, internal DSP state,
    /// sample references), so a lossy synthetic blob would restore incorrectly
    /// while masquerading as faithful state.
    ///
    /// A controller that fails its own `getState` is **not** an error: the
    /// component half is the one a project cannot be restored without, and
    /// plenty of controllers have no UI state to give. That half degrades to
    /// empty and the component half is still returned.
    pub fn state(&self) -> Result<Vec<u8>> {
        tutti_plugin_types::assert_main_thread();
        let component = self.read_component_state()?;
        let controller = self.read_controller_state();
        Ok(pack_state(&component, &controller))
    }

    /// Read the controller's own `getState` stream, or `None` when there is no
    /// controller, it does not implement the call, or it fails.
    ///
    /// Failure is folded into `None` rather than surfaced: see
    /// [`state`](Self::state) for why a controller's UI state is best-effort
    /// while the component's is not.
    fn read_controller_state(&self) -> Option<Vec<u8>> {
        let ctrl = self.interfaces.controller.as_ref()?;
        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;

        let result = unsafe { ctrl.getState(stream_ptr.as_ptr()) };
        if !state_result_ok(result) {
            return None;
        }

        let data = stream.data();
        (!data.is_empty()).then_some(data)
    }

    /// Write the component's `getState` blob into a fresh `IBStream` and return
    /// the bytes. Shared by [`state`](Self::state) and the load-time
    /// controller state-sync in [`initialize`](Self::initialize).
    ///
    /// A `kResultFalse` return (plugin has no state) yields an empty blob;
    /// only a genuine error tresult is surfaced as [`Vst3Error::PluginError`].
    fn read_component_state(&self) -> Result<Vec<u8>> {
        let stream = BStream::new();
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or_else(|| Vst3Error::StateError("Failed to wrap BStream".into()))?;

        let result = unsafe { self.interfaces.component.getState(stream_ptr.as_ptr()) };

        if !state_result_ok(result) {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        Ok(stream.data())
    }

    /// Feed a component-state blob to the edit controller via
    /// `IEditController::setComponentState`, so a separate controller reflects
    /// the processor's state. No-op if the plugin has no controller or the
    /// blob is empty. Errors from the controller are tolerated (some plugins
    /// return `kResultFalse` when they have no controller state to load) —
    /// this is best-effort synchronisation, shared by
    /// [`set_state`](Self::set_state) and [`initialize`](Self::initialize).
    fn push_controller_state(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let Some(ctrl) = self.interfaces.controller.as_ref() else {
            return;
        };
        let ctrl_stream = BStream::from_data(data.to_vec());
        if let Some(ctrl_stream_ref) = ctrl_stream.as_com_ref::<IBStream>() {
            unsafe {
                let _ = ctrl.setComponentState(ctrl_stream_ref.as_ptr());
            }
        }
    }

    /// Restore plugin state from a blob produced by [`state`](Self::state).
    ///
    /// Three things happen, in the order the spec requires. The component gets
    /// its own stream via `IComponent::setState`. The controller is then shown
    /// that same component stream through `setComponentState`, so a separate
    /// controller mirrors the processor's values. Finally the controller gets
    /// its *own* stream via `IEditController::setState` — the UI state that
    /// `setComponentState` cannot carry.
    ///
    /// An empty blob (from a plugin that had no state to save) is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`](crate::Vst3Error::StateError) if the
    /// `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if the plugin
    /// rejects the component blob via `setState`. A controller that rejects its
    /// own stream is tolerated, matching [`state`](Self::state).
    pub fn set_state(&mut self, data: &[u8]) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }

        let (component, controller) = unpack_state(data);
        if component.is_empty() {
            return Ok(());
        }

        let stream = BStream::from_data(component.to_vec());
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or_else(|| Vst3Error::StateError("Failed to wrap BStream".into()))?;

        let result = unsafe { self.interfaces.component.setState(stream_ptr.as_ptr()) };

        if !state_result_ok(result) {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        // Mirror into the controller (no-op for a same-object controller that
        // already saw the state, harmless for a separate one).
        self.push_controller_state(component);

        if let Some(controller) = controller {
            self.push_controller_own_state(controller);
        }

        Ok(())
    }

    /// Hand the controller its own `setState` stream — the half
    /// `setComponentState` does not cover.
    ///
    /// Best-effort for the same reason as
    /// [`read_controller_state`](Self::read_controller_state): losing the UI's
    /// scroll position must not fail a project load whose audio state restored
    /// perfectly.
    fn push_controller_own_state(&self, data: &[u8]) {
        let Some(ctrl) = self.interfaces.controller.as_ref() else {
            return;
        };
        let stream = BStream::from_data(data.to_vec());
        if let Some(stream_ref) = stream.as_com_ref::<IBStream>() {
            unsafe {
                let _ = ctrl.setState(stream_ref.as_ptr());
            }
        }
    }

    /// True if the plugin actually publishes an editor view.
    ///
    /// **Asks `createView`, not `controller.is_some()`.** Those are different
    /// questions and the old answer was the wrong one: nearly every VST3 has an
    /// edit controller — that is where parameters live — while only some also
    /// publish a UI. So this returned `true` unconditionally, for every plugin
    /// in the sample corpus including the four whose `open_editor` fails.
    ///
    /// It is not a cosmetic mismatch. `tutti-plugin-server` feeds this straight
    /// into `Features::EDITOR` on the plugin descriptor
    /// (`loaders/vst3.rs:116,150`), so a DAW advertised an "open editor"
    /// affordance for every VST3 it scanned and failed when the user took it.
    ///
    /// The view is created and immediately released — the same
    /// `createView(kEditor)` the SDK's own `editorhost` uses to decide there is
    /// a UI (`editorhost.cpp:207`). That costs a plugin-side allocation per
    /// call, so callers needing it per-frame should cache it; the DAW asks
    /// once, at scan time.
    ///
    /// A plugin with no controller at all still answers `false`, as before.
    pub fn has_editor(&self) -> bool {
        let Some(ctrl) = self.interfaces.controller.as_ref() else {
            return false;
        };
        let view = unsafe { ctrl.createView(c"editor".as_ptr()) };
        if view.is_null() {
            return false;
        }
        // `createView` returns an owned reference; drop it rather than leak a
        // view per query.
        unsafe {
            ComPtr::<IPlugView>::from_raw(view);
        }
        true
    }

    /// Create the plugin editor, attach it to `parent`, and return its initial
    /// pixel size. Only one editor may be open at a time per instance —
    /// opening a second replaces the first.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::NotSupported`](crate::Vst3Error::NotSupported) if
    /// the plugin has no controller, refuses to create a view, or the view
    /// rejects this platform's window type, and
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if
    /// `IPlugView::attached` fails.
    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        tutti_plugin_types::assert_main_thread();
        let ctrl = self
            .interfaces
            .controller
            .as_ref()
            .ok_or(Vst3Error::NotSupported(
                "Plugin has no editor controller".to_string(),
            ))?;

        let view_raw = unsafe { ctrl.createView(c"editor".as_ptr()) };
        let view = unsafe { ComPtr::from_raw(view_raw) }.ok_or(Vst3Error::NotSupported(
            "Failed to create plugin view".to_string(),
        ))?;

        #[cfg(target_os = "macos")]
        let platform_type = kPlatformTypeNSView;
        #[cfg(target_os = "windows")]
        let platform_type = kPlatformTypeHWND;
        #[cfg(target_os = "linux")]
        let platform_type = kPlatformTypeX11EmbedWindowID;

        // Ask before attaching: a view that only speaks Wayland or NSView must
        // not be handed an X11 window id. `editorhost` checks this first for the
        // same reason (WindowController::onShow), and a view that says no here
        // would otherwise take an untested `attached` path instead of failing
        // cleanly.
        let supported = unsafe { view.isPlatformTypeSupported(platform_type) };
        if platform_type_refused(supported) {
            return Err(Vst3Error::NotSupported(format!(
                "Plugin view does not support platform type {}",
                platform_type_name(platform_type)
            )));
        }

        // Create a fresh frame/channel pair for this editor session.
        // setFrame must precede attached() per Steinberg spec.
        let (plug_frame, resize_rx) = HostPlugFrame::new(
            #[cfg(target_os = "linux")]
            self._library.run_loop(),
        );
        let frame_ptr = plug_frame
            .as_com_ref::<vst3::Steinberg::IPlugFrame>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            let _ = view.setFrame(frame_ptr);
        }

        // HiDPI: if the view implements IPlugViewContentScaleSupport, tell it the
        // backing-store scale factor *before* `attached`, so scale-aware plugins
        // lay their UI out at the right resolution from the first frame. Plugins
        // that don't implement the interface (cast returns None) simply no-op.
        //
        // TODO: thread the real backing-scale from the frontend WindowHandle —
        // `WindowHandle` is a bare `*mut c_void` today and carries no DPI, so we
        // pass 1.0 (the neutral default: correct on standard-DPI displays, a safe
        // no-op elsewhere). The important part is that the *call* is wired, so a
        // real factor becomes a one-line change once the frontend supplies it.
        set_content_scale(&view, host_backing_scale(&parent));

        let result = unsafe { view.attached(parent.as_ptr(), platform_type) };
        if result != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        let (width, height) = query_view_size(&view).unwrap_or(DEFAULT_EDITOR_SIZE);
        self.editor = EditorState::Open {
            view,
            plug_frame,
            resize_rx,
        };

        Ok(EditorSize { width, height })
    }

    /// Close the editor if open, calling `IPlugView::removed`. No-op otherwise.
    /// Called automatically on `Drop`.
    pub fn close_editor(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.close_editor_unchecked();
    }

    /// Assert-free editor teardown for [`Drop`], which can run on the audio
    /// thread when the fundsp graph releases the instance. The public
    /// [`close_editor`](Self::close_editor) asserts the main thread before
    /// delegating here; `Drop` calls this directly to avoid panicking off it.
    fn close_editor_unchecked(&mut self) {
        if let EditorState::Open { view, .. } =
            std::mem::replace(&mut self.editor, EditorState::Closed)
        {
            detach_view(&view);
        }
    }

    pub fn editor_capabilities(&self) -> EditorCapabilities {
        let EditorState::Open { view, .. } = &self.editor else {
            return EditorCapabilities::default();
        };
        let resizable = unsafe { view.canResize() } == kResultOk;
        EditorCapabilities {
            resize: tutti_plugin_types::ResizeHints {
                resizable,
                ..Default::default()
            },
            ..EditorCapabilities::default()
        }
    }

    /// Run one iteration of the event loop this host lends the plugin
    /// (`Linux::IRunLoop`): fire any timers that came due and dispatch any
    /// plugin file descriptor that became readable.
    ///
    /// **The embedder must call this from its UI thread, every frame, for as
    /// long as any editor of this plugin is open.** X11 has no ambient run loop
    /// the way Cocoa and Win32 do, so `iplugview.h` makes the loop the *host's*
    /// job: "the host has to call the event handler when the file descriptor is
    /// marked readable", and a registered timer "will be called repeatedly until
    /// it is unregistered". Nothing else drives them. Skip this and the editor
    /// opens, paints once, and then freezes — no redraws, no animation, no
    /// response to input.
    ///
    /// Cheap and non-blocking: the `poll` uses a zero timeout, so calling it on
    /// a frame where nothing is ready costs one syscall and returns. That is why
    /// it is safe to call unconditionally from a render loop.
    ///
    /// # Why this is not folded into `poll_plugin_notifications`
    ///
    /// The two look alike but run on different clocks, and merging them would
    /// break one or the other.
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications) drains
    /// queues that this host filled; the work is already done and arriving late
    /// only delays a UI update. This call *is* the plugin's event loop — its
    /// cadence sets the editor's frame rate, and a host that polls
    /// notifications a few times a second (perfectly adequate for parameter
    /// echoes) would render such an editor unusable. Equally, a host with no
    /// editor open should not be forced to pump a loop with nothing in it.
    /// Keeping them separate lets each be called at the rate it actually needs.
    ///
    /// # Why the host and not a timer thread
    ///
    /// A background thread ticking this would be simpler for the embedder and
    /// is wrong: the handlers are plugin GUI code reaching into its X
    /// connection, and the spec's whole premise is that the host donates *its
    /// UI thread*. Calling them from anywhere else is the same data race as
    /// touching any other toolkit off-thread. So the obligation is the
    /// embedder's, and this method is the seam — it deliberately cannot be
    /// automated away from inside a library that owns no event loop.
    ///
    /// No-op on non-Linux targets, where the OS provides the run loop, so
    /// calling it unconditionally is portable.
    pub fn run_editor_loop_iteration(&mut self) {
        tutti_plugin_types::assert_main_thread();
        // The library-scoped loop, not the frame's — plugins register against
        // the host context (via `setHostContext`) before any editor exists, and
        // both objects share this one loop. See `com/run_loop.rs`.
        #[cfg(target_os = "linux")]
        self._library.run_loop().run_iteration();
    }

    /// What the plugin has registered with our run loop, and how much this host
    /// has dispatched. Test-only observation seam behind the `conformance`
    /// feature — see [`RunLoopActivity`](crate::RunLoopActivity).
    ///
    /// All-zero off Linux, where the OS owns the run loop and plugins register
    /// nothing with us, so callers need no `cfg` of their own.
    #[cfg(feature = "conformance")]
    pub fn run_loop_activity(&self) -> crate::RunLoopActivity {
        #[cfg(target_os = "linux")]
        {
            self._library.run_loop().activity()
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::RunLoopActivity::default()
        }
    }

    /// Coalesces multiple `IPlugFrame::resizeView` requests received
    /// since the last poll, returning only the latest.
    pub fn poll_editor_resize_request(&mut self) -> Option<EditorSize> {
        let EditorState::Open { resize_rx, .. } = &self.editor else {
            return None;
        };
        let mut latest = None;
        while let Ok(size) = resize_rx.try_recv() {
            latest = Some(size);
        }
        latest
    }

    /// Ask the plugin what it would snap `requested` to, without applying it.
    ///
    /// A read-only probe of `IPlugView::checkSizeConstraint`: the plugin clamps
    /// the rect to the nearest size it accepts, and this reports that size
    /// without the `onSize` that [`resize_editor`](Self::resize_editor) would
    /// follow with. Returns `None` when no editor is open.
    ///
    /// Exposed for the conformance suite, which uses it to check that a size
    /// the host granted is genuinely one the plugin accepts.
    #[cfg(feature = "conformance")]
    pub fn check_editor_size_constraint(&self, requested: EditorSize) -> Option<EditorSize> {
        let EditorState::Open { view, .. } = &self.editor else {
            return None;
        };
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: requested.width as i32,
            bottom: requested.height as i32,
        };
        // `kResultFalse` means "no constraint to apply", which leaves `rect`
        // holding the request — the same fallback `resize_editor` uses.
        unsafe { view.checkSizeConstraint(&mut rect) };
        Some(EditorSize {
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
        })
    }

    /// Returns the snapped size the plugin applied.
    pub fn resize_editor(&mut self, requested: EditorSize) -> Result<EditorSize> {
        let EditorState::Open { view, .. } = &self.editor else {
            return Err(Vst3Error::NotSupported("editor not open".to_string()));
        };
        let requested_rect = ViewRect {
            left: 0,
            top: 0,
            right: requested.width as i32,
            bottom: requested.height as i32,
        };
        // `checkSizeConstraint` snaps the rect to the nearest size the plugin
        // will accept (aspect-ratio locks, min/max, integer-multiple grids). We
        // must apply that *constrained* rect via `onSize`, not the raw request —
        // otherwise a plugin that only accepts, say, 4:3 sizes gets handed a
        // size it rejects. The call snaps the rect in place and returns
        // `kResultTrue` when it changed it; a `kResultFalse` (no constraint / not
        // implemented) leaves `constrained` equal to the request, which is the
        // correct fallback.
        let mut constrained = requested_rect;
        let snapped = unsafe { view.checkSizeConstraint(&mut constrained) } == kResultTrue;
        // On decline, honour the original request verbatim rather than any
        // partially-written rect the plugin may have left behind.
        let mut rect = if snapped { constrained } else { requested_rect };
        let res = unsafe { view.onSize(&mut rect) };
        if res != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: res,
            });
        }
        Ok(EditorSize {
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
        })
    }

    /// Wire the component's connection point directly to the separate
    /// controller's. No-op unless both ends expose `IConnectionPoint`.
    ///
    /// The host deliberately does **not** interpose its own
    /// `IConnectionPoint` proxy between the two halves: it direct-wires the
    /// plugin's component and controller to each other, so their private
    /// messages pass straight through without the host inspecting or
    /// reformatting them.
    /// Returns whether both halves accepted the connection. A refusal is not an
    /// error — the plugin simply runs unconnected, as it does when either half
    /// declines to expose `IConnectionPoint` at all.
    ///
    /// The two `connect` calls have to succeed or fail together. `connect`
    /// returns a `tresult` and both returns were discarded, so a plugin that
    /// accepted the first and refused the second was left asymmetrically wired
    /// — the component believing it has a peer, the controller not — and
    /// `initialize()` continued regardless. The plugin's private
    /// component↔controller messages then flow one way only, which surfaces
    /// much later as an editor that does not track the processor.
    #[must_use]
    fn connect_separate_controller(&self, ctrl: &ComPtr<IEditController>) -> bool {
        let Some(comp_conn) = self.interfaces.component.cast::<IConnectionPoint>() else {
            return false;
        };
        let Some(ctrl_conn) = ctrl.cast::<IConnectionPoint>() else {
            return false;
        };
        unsafe {
            if comp_conn.connect(ctrl_conn.as_ptr()) != kResultOk {
                return false;
            }
            if ctrl_conn.connect(comp_conn.as_ptr()) != kResultOk {
                // Unwind the half that took, so the component is not left
                // holding a peer that never reciprocated. Its return is
                // genuinely uninteresting: there is no third state to recover
                // to, and the disconnect path discards its results for the
                // same reason.
                comp_conn.disconnect(ctrl_conn.as_ptr());
                return false;
            }
        }
        true
    }

    /// Reverse [`connect_separate_controller`](Self::connect_separate_controller):
    /// tell each half to drop its connection to the other before
    /// `terminate()`. Symmetric to the connect — casts both ends to
    /// `IConnectionPoint` and `disconnect`s each. No-op unless both ends expose
    /// the interface. Called from `Drop` for a `Controller::Separate` plugin.
    fn disconnect_separate_controller(&self) {
        let Controller::Separate(ctrl) = &self.interfaces.controller else {
            return;
        };
        let Some(comp_conn) = self.interfaces.component.cast::<IConnectionPoint>() else {
            return;
        };
        let Some(ctrl_conn) = ctrl.cast::<IConnectionPoint>() else {
            return;
        };
        unsafe {
            comp_conn.disconnect(ctrl_conn.as_ptr());
            ctrl_conn.disconnect(comp_conn.as_ptr());
        }
    }

    fn initialize(&mut self) -> Result<()> {
        let host_ptr = self.host_context_ptr()?;

        let result = unsafe { self.interfaces.component.initialize(host_ptr) };
        // `kResultFalse` is a *refusal*, exactly as in `set_active`: the plugin
        // is declining to come up, and every later call would run against a
        // component that never initialised. The SDK's own host agrees —
        // `plugprovider.cpp:140` requires `== kResultOk` and reports a failure
        // otherwise.
        //
        // `kNotImplemented` is not tolerated either, unlike the state methods:
        // `IPluginBase::initialize` is mandatory, so a plugin that has not
        // implemented it has not implemented the interface.
        if result != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        self.reconcile_bus_counts();

        let separate_controller = matches!(self.interfaces.controller, Controller::Separate(_));

        if let Controller::Separate(ctrl) = &self.interfaces.controller {
            unsafe {
                let _ = ctrl.initialize(host_ptr);
            }
            let ctrl = ctrl.clone();
            // A refused connection is not fatal — a plugin whose halves cannot
            // talk to each other still processes audio and still shows an
            // editor; it just loses its private message channel. That is the
            // same outcome as a plugin not exposing `IConnectionPoint`, which
            // this host has always tolerated. What matters is that the failure
            // does not leave one half wired to a peer that is not wired back.
            let _connected = self.connect_separate_controller(&ctrl);
        }

        self.attach_component_handler();

        // Bridge the processor's own state into a *separate* controller so its
        // editor opens showing the real values, not defaults. A same-object
        // controller already shares the component's state, so this is only
        // needed (and only correct) for `Controller::Separate`. `setState` is
        // the sole other path that reaches `setComponentState`; at load there is
        // no host-supplied blob, so we read the component's current state and
        // push it across. Tolerate a plugin with no state (empty blob / the
        // controller returning `kResultFalse`) — never fail `load()` on a
        // state-sync miss.
        if separate_controller {
            // `read_component_state` only errors if the host-side BStream can't
            // be wrapped or the plugin's `getState` returns a hard failure;
            // treat either as "no state to sync" and continue — the editor then
            // opens at defaults exactly as before this fix, rather than failing
            // the load. (No logging framework is wired into this crate.)
            if let Ok(state) = self.read_component_state() {
                self.push_controller_state(&state);
            }
        }

        Ok(())
    }

    /// The live reference count on the host-context object. Test-only seam for
    /// the load-path refcount assertion in `tests/vst3_conformance.rs`: the
    /// hand-off contract below is otherwise invisible from outside, and a leak
    /// of one reference per load is not observable any other way.
    ///
    /// Reads the count without moving it — `add_ref` returns the value *after*
    /// incrementing, so the matching `release` restores it and the count before
    /// the pair is one less.
    #[cfg(feature = "conformance")]
    pub fn host_context_refcount(&self) -> usize {
        use vst3::com_scrape_types::Unknown;
        use vst3::Steinberg::Vst::IHostApplication;

        let Some(iface) = self.host.application.as_com_ref::<IHostApplication>() else {
            return 0;
        };
        unsafe {
            let after_add = IHostApplication::add_ref(iface.as_ptr());
            IHostApplication::release(iface.as_ptr());
            after_add - 1
        }
    }

    /// `IHostApplication` upcast to `FUnknown`, **borrowed**: the returned
    /// pointer carries no reference for the caller to hand over or drop. It is
    /// valid only while `self.host.application` is alive, which is why this is
    /// private and its result never outlives the calling method.
    ///
    /// `IPluginBase::initialize` does **not** take ownership of the context.
    /// The plugin retains it itself if it keeps it: `ComponentBase::hostContext`
    /// is an `IPtr<FUnknown>` (`public.sdk/source/vst/vstcomponentbase.h:84`),
    /// so the bare `hostContext = context;` in `ComponentBase::initialize`
    /// (`vstcomponentbase.cpp:42`) runs `IPtr::operator=`, which addRefs — and
    /// the `hostContext = nullptr;` in `terminate` (same file, line 51) releases.
    /// `IPluginBase::terminate`'s own contract says as much:
    /// "You have to release all references to any host application interfaces"
    /// (`pluginterfaces/base/ipluginbase.h:48`).
    ///
    /// So the host must lend, not give. Passing an owned `+1` here (a
    /// `to_com_ptr().into_raw()`) leaks one reference per load, unbounded across
    /// load/unload cycles. The matching trap is the inverse: because the plugin
    /// never consumed a reference, a host that "balances" this with a
    /// `FUnknown::release` on any path frees a reference it does not own — a
    /// use-after-free, not a leak. Steinberg's own reference host borrows and
    /// releases nothing on either path
    /// (`public.sdk/source/vst/hosting/plugprovider.cpp:140,178`).
    fn host_context_ptr(&self) -> Result<*mut FUnknown> {
        Ok(self
            .host
            .application
            .as_com_ref::<vst3::Steinberg::Vst::IHostApplication>()
            .ok_or(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: 0,
            })?
            .upcast::<FUnknown>()
            .as_ptr())
    }

    /// Re-query bus counts from the component — `initialize` may have changed
    /// them (some plugins don't declare bus counts until after init).
    ///
    /// `pub(crate)` only so `Vst3Instance::restart_bus_configuration` can call
    /// it from inside the deactivate/reactivate cycle. Deliberately not public:
    /// on a live instance this must not be reached on its own, which is exactly
    /// the mistake the `kIoChanged` path used to make.
    pub(crate) fn reconcile_bus_counts(&mut self) {
        if let Some(layout) = self.interfaces.component.audio_bus_channel_count(K_INPUT) {
            // `PluginInfo` carries raw usize channel counts; take the count at
            // this boundary.
            let ch = layout.count() as usize;
            if ch != self.info.num_inputs {
                self.info = self.info.clone().audio_io(ch, self.info.num_outputs);
            }
        }
        if let Some(layout) = self.interfaces.component.audio_bus_channel_count(K_OUTPUT) {
            let ch = layout.count() as usize;
            if ch != self.info.num_outputs {
                self.info = self.info.clone().audio_io(self.info.num_inputs, ch);
            }
        }
        // Re-enumerate the full per-bus layout — `initialize` may have changed
        // bus counts, and aux/sidechain buses are only visible post-init on
        // some plugins.
        let input_bus_channels = self.interfaces.component.audio_bus_channels(K_INPUT);
        let output_bus_channels = self.interfaces.component.audio_bus_channels(K_OUTPUT);
        self.info = self
            .info
            .clone()
            .bus_channels(input_bus_channels, output_bus_channels);

        // Event buses need the same treatment, and for the same reason: a
        // component adds them in `initialize` (`addEventInput`), so the
        // pre-init query in `build_plugin_info` always sees zero. Without this,
        // every instrument reports `has_midi_input == false` and a host that
        // gates MIDI delivery on it never sends the plugin a single note.
        let receives_midi = unsafe {
            self.interfaces
                .component
                .getBusCount(crate::host::instance::K_EVENT, K_INPUT)
                > 0
        };
        let emits_midi = unsafe {
            self.interfaces
                .component
                .getBusCount(crate::host::instance::K_EVENT, K_OUTPUT)
                > 0
        };
        self.info = self
            .info
            .clone()
            .midi(receives_midi)
            .midi_output(emits_midi);
    }

    /// Hand the controller our `IComponentHandler` so it can report param
    /// edits, bus-activation requests, etc.
    fn attach_component_handler(&self) {
        let Some(ctrl) = self.interfaces.controller.as_ref() else {
            return;
        };
        let handler_ptr = self
            .host
            .handler
            .as_com_ref::<vst3::Steinberg::Vst::IComponentHandler>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            let _ = ctrl.setComponentHandler(handler_ptr);
        }
    }
}

impl Drop for Vst3Loaded {
    fn drop(&mut self) {
        // No main-thread assert: Drop can run on the audio thread when the
        // fundsp graph releases the instance. See `close_editor_unchecked`.
        self.close_editor_unchecked();
        // Mirror the load-time connect in reverse: for a separate controller,
        // tear the component↔controller connection down before terminating
        // either half. No-op for same-object / no controller.
        self.disconnect_separate_controller();
        // Retract the handler before terminating. Unlike `initialize`, which
        // borrows the host context, `setComponentHandler` *retains* — so a
        // plugin that overrides `terminate` without chaining up to the base
        // class (which resets it) would hold our handler past its own teardown.
        // Order — retract, controller, component — follows Steinberg's own
        // wrapper (`basewrapper.cpp:369`).
        if let Some(ctrl) = self.interfaces.controller.as_ref() {
            unsafe {
                let _ = ctrl.setComponentHandler(std::ptr::null_mut());
                ctrl.terminate();
            }
        }
        unsafe {
            self.interfaces.component.terminate();
        }
    }
}

fn check_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Scanning,
            reason: "Plugin file not found".to_string(),
        });
    }
    Ok(())
}

/// One factory class — the handful of fields we need to keep together when
/// walking the `IPluginFactory`. Returned by [`find_audio_class`].
pub(super) struct AudioClass {
    /// Steinberg-signed class id, used for `IPluginFactory::createInstance`.
    pub cid: [i8; 16],
    /// Unsigned byte form of the cid — used for human-readable IDs only.
    pub cid_bytes: [u8; 16],
    /// Display name from `PClassInfo::name`.
    pub name: String,
    /// Per-class vendor, when declared. `None` falls back to the factory's.
    pub vendor: Option<String>,
    /// Per-class version string, when declared.
    pub version: Option<String>,
}

fn ensure_has_classes(library: &Vst3Library, path: &Path) -> Result<()> {
    if library.count_classes() == 0 {
        return Err(Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            reason: "VST3 factory contains no classes".to_string(),
        });
    }
    Ok(())
}

/// The backing-store scale factor to advertise to the plugin view.
///
/// The host `WindowHandle` is a raw pointer with no attached DPI/scale
/// information, so there is nothing to read from it yet — we return the neutral
/// `1.0`. When the frontend grows a way to carry the display's backing scale
/// (e.g. `NSWindow.backingScaleFactor` / Win32 `GetDpiForWindow`), source it
/// here and the wired `setContentScaleFactor` call starts delivering real
/// values with no further plumbing.
fn host_backing_scale(_parent: &WindowHandle) -> f32 {
    // TODO: thread real backing-scale from frontend WindowHandle.
    1.0
}

/// Does this `isPlatformTypeSupported` result mean "no"?
///
/// Only an explicit denial counts. `editorhost` treats anything but
/// `kResultTrue` as fatal, and copying that would break working plugins: the
/// SDK's own `CPluginView` — base class of `EditorView`, and so of a large
/// share of shipping plugins — returns `kNotImplemented` unconditionally
/// (`public.sdk/source/common/pluginview.cpp`) while its `attached` succeeds
/// anyway. A view that declines to answer is not a view that said no.
///
/// `kInvalidArgument` *is* a refusal: it is what `VSTGUIEditor` returns for a
/// type it does not handle (`vstguieditor.cpp`), which is precisely the
/// Wayland-only-view-handed-an-X11-id case this check exists to catch.
///
/// The asymmetry is deliberate — a false "unsupported" costs the user their
/// editor, while a false "supported" only lands us where we already were
/// before this check existed.
pub fn platform_type_refused(result: i32) -> bool {
    result == kResultFalse || result == kInvalidArgument
}

/// Whether a `getState` / `setState` result counts as success.
///
/// **`kNotImplemented`, not `kResultFalse`** — and getting this backwards is not
/// a hypothetical. The SDK's own `Component` base returns `kNotImplemented` from
/// both methods (`vstcomponent.cpp:159,165`), so *every* plugin that does not
/// override state answers that way. This used to accept
/// `kResultOk || kResultFalse`, which rejected exactly those plugins: saving a
/// project containing one failed with a `PluginError`.
///
/// The list matches `verify` in the SDK's own preset writer
/// (`vstpresetfile.cpp:53`), which is the closest thing to a reference host for
/// this call and accepts `kResultOk || kNotImplemented`.
///
/// `kResultFalse` is deliberately *not* here. Unlike the state methods, where
/// "I have none" is the common honest answer, a plugin that actively fails a
/// state round-trip has told us the blob is bad — and silently returning an
/// empty one would persist a project that cannot be restored.
fn state_result_ok(result: i32) -> bool {
    result == kResultOk || result == kNotImplemented
}

/// Magic prefixing a two-stream state blob. Chosen to be something no VST3
/// plugin would plausibly open its own private state with, so
/// [`unpack_state`] can tell a packed blob from a bare component stream saved
/// before this host carried the controller's half.
const STATE_MAGIC: &[u8; 8] = b"TUTTIVS3";

/// Container version. Bump only if the layout after the magic changes; a
/// reader that meets a version it does not know falls back to treating the
/// whole blob as a bare component stream, which is wrong but recoverable —
/// where mis-splitting it would hand the plugin garbage.
const STATE_VERSION: u8 = 1;

/// Pack the component and controller streams into one opaque blob.
///
/// Layout: magic, version, then the component length as a little-endian `u32`,
/// then the two payloads. The controller half is whatever remains, so it needs
/// no length of its own.
///
/// A plugin with no controller state at all packs to `component` alone, with
/// no header. That keeps the common case byte-identical to what this host
/// saved before the controller stream existed, so nothing re-saves a project
/// merely for having been opened.
fn pack_state(component: &[u8], controller: &Option<Vec<u8>>) -> Vec<u8> {
    let Some(controller) = controller.as_ref().filter(|c| !c.is_empty()) else {
        return component.to_vec();
    };

    // A component stream too long to describe in a u32 cannot be split back
    // apart, so keep the half a project cannot be restored without and drop
    // the UI half. 4 GiB of component state is not a case worth a wider field.
    let Ok(len) = u32::try_from(component.len()) else {
        return component.to_vec();
    };

    let mut out = Vec::with_capacity(STATE_MAGIC.len() + 5 + component.len() + controller.len());
    out.extend_from_slice(STATE_MAGIC);
    out.push(STATE_VERSION);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(component);
    out.extend_from_slice(controller);
    out
}

/// Split a blob back into its component and controller halves.
///
/// Anything not carrying [`STATE_MAGIC`] is a bare component stream — either
/// saved by an older build of this host, or packed by [`pack_state`] for a
/// plugin with no controller state. Both restore correctly by handing the
/// whole blob to the component, which is what this host has always done.
///
/// A blob that starts with the magic but is then malformed (unknown version,
/// truncated, a length past the end) is treated the same way. That cannot
/// restore the controller half, but the alternative — splitting at a length we
/// have reason to distrust — hands the *component* a corrupt stream, and the
/// component half is the one a project cannot be restored without.
fn unpack_state(data: &[u8]) -> (&[u8], Option<&[u8]>) {
    let Some(rest) = data.strip_prefix(STATE_MAGIC.as_slice()) else {
        return (data, None);
    };
    let Some((&version, rest)) = rest.split_first() else {
        return (data, None);
    };
    if version != STATE_VERSION {
        return (data, None);
    }
    if rest.len() < 4 {
        return (data, None);
    }
    let (len_bytes, payload) = rest.split_at(4);
    let len =
        u32::from_le_bytes(len_bytes.try_into().expect("split_at(4) yields 4 bytes")) as usize;
    if len > payload.len() {
        return (data, None);
    }

    let (component, controller) = payload.split_at(len);
    (component, (!controller.is_empty()).then_some(controller))
}

/// Tear a view down: retract the host frame, then tell the view it is removed.
///
/// The order is the point, and it matches `editorhost`'s `closePlugView`. The
/// `HostPlugFrame` this view was given dies with the `EditorState` that owned
/// it, so a plugin still holding the pointer during `removed()` — to report a
/// final `resizeView`, say — would call through memory about to be freed.
/// Retracting first makes that unrepresentable rather than merely unlikely.
///
/// Split out of `close_editor_unchecked` so the ordering is reachable from a
/// test without a loaded plugin: the state this runs on can only be built by
/// `open_editor`, but the sequence itself is what needs pinning.
// `pub` in a private module: `super` re-exports it only under `conformance`, so
// this never widens the public API of a normal build.
pub fn detach_view(view: &ComPtr<IPlugView>) {
    unsafe {
        view.setFrame(std::ptr::null_mut());
        view.removed();
    }
}

/// Render a `kPlatformType*` constant as text for error messages. These are C
/// string literals from the SDK, not Rust `&str`, so they need decoding at the
/// FFI edge; a malformed one degrades to a placeholder rather than failing the
/// error path we are already on.
fn platform_type_name(platform_type: vst3::Steinberg::FIDString) -> String {
    if platform_type.is_null() {
        return "<null>".to_string();
    }
    // SAFETY: `platform_type` is one of the SDK's static `kPlatformType*`
    // literals, which are NUL-terminated and live for the program's duration.
    unsafe { std::ffi::CStr::from_ptr(platform_type) }
        .to_str()
        .unwrap_or("<invalid utf-8>")
        .to_string()
}

/// Tell the view its content scale via `IPlugViewContentScaleSupport`, if it
/// implements that interface. No-op for views that don't (the cast returns
/// `None`) — the common case for non-HiDPI-aware plugins.
fn set_content_scale(view: &ComPtr<IPlugView>, scale: f32) {
    if let Some(scale_support) = view.cast::<IPlugViewContentScaleSupport>() {
        unsafe {
            let _ = scale_support.setContentScaleFactor(scale);
        }
    }
}

/// Read the plug-view's `getSize()` and translate it into our `(width, height)`
/// tuple. Returns `None` if the view refuses — callers fall back to a default.
fn query_view_size(view: &ComPtr<IPlugView>) -> Option<(u32, u32)> {
    let mut rect = ViewRect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let result = unsafe { view.getSize(&mut rect) };
    if result == kResultOk {
        Some((
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    } else {
        None
    }
}

/// Assemble `PluginInfo` from already-queried interfaces. Used by both
/// [`Vst3Loaded::probe`] (which may not own an `IAudioProcessor`) and
/// [`Vst3Loaded::load`] (which does).
fn build_plugin_info_raw(
    library: &Vst3Library,
    component: &ComPtr<IComponent>,
    processor: Option<&ComPtr<IAudioProcessor>>,
    class: &AudioClass,
) -> PluginInfo {
    // The class's own vendor wins over the factory's: `ipluginbase.h:357`
    // documents the field as "overwrite vendor information from factory info".
    // They differ on distributor-published bundles, where the factory names the
    // distributor and the class names the maker — reading only the factory
    // credits the wrong one.
    let vendor = class.vendor.clone().unwrap_or_else(|| {
        library
            .get_factory_info()
            .map(|info| info.vendor)
            .unwrap_or_default()
    });
    // Every VST3 plugin used to report "1.0.0" — a literal, unconditional, for
    // all of them. The real string is on the class (e.g. "1.0.0.512",
    // Major.Minor.Subversion.Build). A plugin that declares none keeps the old
    // placeholder rather than showing an empty version field.
    let version = class.version.clone().unwrap_or_else(|| "1.0.0".to_string());
    // `PluginInfo` carries raw usize channel counts; take the count at this
    // boundary. A plugin that reports no bus, or whose query fails, contributes
    // 0 — the per-bus vecs below carry the same absence, and `bus_channels` in
    // the server loader is the one place that decides what to do about it.
    let num_inputs = component
        .audio_bus_channel_count(K_INPUT)
        .map_or(0, |l| l.count() as usize);
    let num_outputs = component
        .audio_bus_channel_count(K_OUTPUT)
        .map_or(0, |l| l.count() as usize);
    let input_bus_channels = component.audio_bus_channels(K_INPUT);
    let output_bus_channels = component.audio_bus_channels(K_OUTPUT);
    let supports_f64 = processor
        .map(|p| unsafe { p.canProcessSampleSize(crate::types::K_SAMPLE_64_INT) == kResultOk })
        .unwrap_or(false);
    let receives_midi =
        unsafe { component.getBusCount(crate::host::instance::K_EVENT, K_INPUT) > 0 };
    let emits_midi = unsafe { component.getBusCount(crate::host::instance::K_EVENT, K_OUTPUT) > 0 };

    PluginInfo::new(
        format!("vst3.{}", cid_to_string(&class.cid_bytes)),
        class.name.clone(),
    )
    .vendor(vendor)
    .version(version)
    .audio_io(num_inputs, num_outputs)
    .bus_channels(input_bus_channels, output_bus_channels)
    .midi(receives_midi)
    .midi_output(emits_midi)
    .f64_support(supports_f64)
}

/// Convenience wrapper for the load path where we always have a processor.
fn build_plugin_info(
    library: &Vst3Library,
    component: &ComPtr<IComponent>,
    processor: &ComPtr<IAudioProcessor>,
    class: &AudioClass,
) -> PluginInfo {
    build_plugin_info_raw(library, component, Some(processor), class)
}

/// The audio class named `wanted`, or a `LoadFailed` naming what is on offer.
fn find_audio_class_named(library: &Vst3Library, path: &Path, wanted: &str) -> Result<AudioClass> {
    let audio: Vec<_> = (0..library.count_classes())
        .filter_map(|i| library.get_class_info(i).ok())
        .filter(|info| info.category.contains("Audio"))
        .collect();

    audio
        .iter()
        .find(|info| info.name == wanted)
        .map(|info| AudioClass {
            cid: info.cid,
            cid_bytes: info.cid_bytes,
            name: info.name.clone(),
            vendor: info.vendor.clone(),
            version: info.version.clone(),
        })
        .ok_or_else(|| Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            reason: format!(
                "no audio class named {wanted:?}; this bundle exports {:?}",
                audio.iter().map(|i| &i.name).collect::<Vec<_>>()
            ),
        })
}

fn find_audio_class(library: &Vst3Library, path: &Path) -> Result<AudioClass> {
    let count = library.count_classes();
    (0..count)
        .find_map(|i| {
            let info = library.get_class_info(i).ok()?;
            if !info.category.contains("Audio") {
                return None;
            }
            Some(AudioClass {
                cid: info.cid,
                cid_bytes: info.cid_bytes,
                name: info.name,
                vendor: info.vendor,
                version: info.version,
            })
        })
        .ok_or_else(|| Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            reason: "No audio processor classes found in VST3".to_string(),
        })
}

/// Ask the plugin (via `IProcessContextRequirements`) which `ProcessContext`
/// fields it actually consumes, so [`crate::types::to_process_context`] can
/// skip populating the rest.
///
/// Plugins that don't implement the interface get [`u32::MAX`] — every bit set,
/// i.e. "send everything", which is both the pre-spec default and exactly what
/// this host did before this interface was wired. So the gating is a strict
/// no-op for them.
/// Copy a Rust `&str` into a fixed `[char8; 64]` (`i8`) VST3 string buffer as
/// NUL-terminated ASCII/UTF-8 bytes, truncating to fit (leaving room for the
/// terminator). Used to fill `RepresentationInfo`'s vendor/name/version/host.
fn fill_char8_64(dst: &mut [i8; 64], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (slot, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *slot = b as i8;
    }
    dst[n] = 0;
}

fn query_process_context_requirements(processor: &ComPtr<IAudioProcessor>) -> u32 {
    match processor.cast::<IProcessContextRequirements>() {
        Some(reqs) => unsafe { reqs.getProcessContextRequirements() },
        None => u32::MAX,
    }
}

fn query_controller(component: &ComPtr<IComponent>, library: &Vst3Library) -> Controller {
    if let Some(ctrl) = component.cast::<IEditController>() {
        return Controller::Same(ctrl);
    }
    let mut controller_cid = [0i8; 16];
    let result = unsafe { component.getControllerClassId(&mut controller_cid) };
    if result == kResultOk && controller_cid != [0i8; 16] {
        match library.create_instance::<IEditController>(&controller_cid) {
            Ok(ctrl) => Controller::Separate(ctrl),
            Err(_) => Controller::None,
        }
    } else {
        Controller::None
    }
}

#[cfg(test)]
mod char8_fill_tests {
    use super::fill_char8_64;

    /// A short string is copied verbatim and NUL-terminated.
    #[test]
    fn fills_and_terminates() {
        let mut buf = [0i8; 64];
        fill_char8_64(&mut buf, "Acme");
        assert_eq!(&buf[..4], &[b'A' as i8, b'c' as i8, b'm' as i8, b'e' as i8]);
        assert_eq!(buf[4], 0);
    }

    /// An over-long string is truncated, always leaving room for the NUL
    /// terminator at index 63.
    #[test]
    fn truncates_leaving_room_for_terminator() {
        let mut buf = [1i8; 64];
        let long = "x".repeat(100);
        fill_char8_64(&mut buf, &long);
        // 63 chars written, last slot is the terminator.
        assert!(buf[..63].iter().all(|&b| b == b'x' as i8));
        assert_eq!(buf[63], 0);
    }

    /// An empty string yields an immediate terminator.
    #[test]
    fn empty_is_just_terminator() {
        let mut buf = [9i8; 64];
        fill_char8_64(&mut buf, "");
        assert_eq!(buf[0], 0);
    }
}

#[cfg(test)]
mod state_container_tests {
    use super::{pack_state, unpack_state, STATE_MAGIC, STATE_VERSION};

    /// Both halves survive the round trip, split back at the right byte.
    #[test]
    fn both_halves_survive_a_round_trip() {
        let component = b"component-private-bytes".as_slice();
        let controller = b"scroll=42;tab=2".to_vec();

        let packed = pack_state(component, &Some(controller.clone()));
        let (got_component, got_controller) = unpack_state(&packed);

        assert_eq!(got_component, component);
        assert_eq!(got_controller, Some(controller.as_slice()));
    }

    /// A blob saved before this host carried the controller's stream is a bare
    /// component blob with no header. It must still restore, or every existing
    /// project loses its plugin state.
    ///
    /// The fixture is binary rather than text, and its leading bytes are chosen
    /// to parse as a *valid* header if the magic check were skipped: a version
    /// byte, then a little-endian length well inside the blob. A plugin's
    /// private state is binary, so opening on such bytes is ordinary rather
    /// than contrived — and a reader that trusted them would hand the component
    /// the first three bytes of a twenty-byte stream and call the rest UI state.
    #[test]
    fn a_bare_component_blob_still_restores() {
        let mut legacy = vec![STATE_VERSION];
        legacy.extend_from_slice(&3u32.to_le_bytes());
        legacy.extend_from_slice(b"abcdefghijklmno");

        let (component, controller) = unpack_state(&legacy);
        assert_eq!(component, legacy.as_slice());
        assert_eq!(controller, None);
    }

    /// A plugin with no controller state packs to exactly the component bytes,
    /// so a project does not churn on disk merely for being opened by a build
    /// that knows about the second stream.
    #[test]
    fn no_controller_state_packs_to_the_bare_component_bytes() {
        let component = b"component-only".as_slice();
        assert_eq!(pack_state(component, &None), component);
        assert_eq!(pack_state(component, &Some(Vec::new())), component);
    }

    /// A component half whose own bytes begin with the magic must not be
    /// mistaken for a container when it is handed back unpacked.
    #[test]
    fn a_component_blob_starting_with_the_magic_round_trips() {
        let mut component = STATE_MAGIC.to_vec();
        component.extend_from_slice(b"...plugin's own bytes");
        let controller = b"ui".to_vec();

        let packed = pack_state(&component, &Some(controller.clone()));
        let (got_component, got_controller) = unpack_state(&packed);

        assert_eq!(got_component, component);
        assert_eq!(got_controller, Some(controller.as_slice()));
    }

    /// A truncated or otherwise malformed container degrades to "all of it is
    /// component state" rather than splitting at a length it cannot trust.
    /// Restoring the component half wrongly is worse than losing the UI half.
    #[test]
    fn a_malformed_container_falls_back_to_the_whole_blob() {
        let mut truncated = STATE_MAGIC.to_vec();
        truncated.push(STATE_VERSION);
        truncated.extend_from_slice(&[0u8, 1]); // a 2-byte length field, not 4
        assert_eq!(unpack_state(&truncated), (truncated.as_slice(), None));

        let mut past_the_end = STATE_MAGIC.to_vec();
        past_the_end.push(STATE_VERSION);
        past_the_end.extend_from_slice(&99u32.to_le_bytes());
        past_the_end.extend_from_slice(b"short");
        assert_eq!(unpack_state(&past_the_end), (past_the_end.as_slice(), None));

        let mut unknown_version = STATE_MAGIC.to_vec();
        unknown_version.push(STATE_VERSION.wrapping_add(1));
        unknown_version.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            unpack_state(&unknown_version),
            (unknown_version.as_slice(), None)
        );
    }

    /// An empty controller half is spelled `None`, never `Some(&[])`, so a
    /// caller cannot hand a plugin a zero-length `setState` stream that means
    /// nothing.
    #[test]
    fn an_empty_controller_half_reads_back_as_absent() {
        let packed = pack_state(b"component", &Some(b"x".to_vec()));
        let (_, controller) = unpack_state(&packed);
        assert_eq!(controller, Some(b"x".as_slice()));

        // Built by hand: a container claiming the whole payload is component.
        let mut zero_tail = STATE_MAGIC.to_vec();
        zero_tail.push(STATE_VERSION);
        zero_tail.extend_from_slice(&9u32.to_le_bytes());
        zero_tail.extend_from_slice(b"component");
        assert_eq!(unpack_state(&zero_tail), (b"component".as_slice(), None));
    }
}

#[cfg(test)]
mod restart_outcome_tests {
    use super::RestartOutcome;
    use crate::com::RestartFlags;

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
    /// Six of the twelve used to stop at `RestartFlags`: decoded into a named
    /// field, then dropped by `merge_flags`, so a plugin could signal them and
    /// no consumer could ever see it. A per-flag test would not have caught
    /// that — each one passes by simply not being written — so this asserts the
    /// *whole* mapping at once.
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
